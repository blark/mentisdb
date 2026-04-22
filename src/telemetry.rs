//! Telemetry: logging, tracing, and metrics for mentisdbd.
//!
//! Replaces `env_logger` with the `tracing` ecosystem. Always writes to
//! stdout (pretty when stdout is a TTY, JSON otherwise). When
//! `MENTISDB_OTLP_ENDPOINT` is set, additionally exports logs and traces
//! over OTLP/HTTP to the configured collector.
//!
//! The upstream `log::…!` macros continue to work via the `tracing-log`
//! bridge — no upstream call sites need to change.
//!
//! # Environment variables
//!
//! | Variable | Default | Purpose |
//! |---|---|---|
//! | `MENTISDB_OTLP_ENDPOINT` | (unset) | OTLP/HTTP collector URL; OTLP disabled when unset |
//! | `MENTISDB_OTLP_AUTH` | (unset) | Value of the `Authorization` header |
//! | `MENTISDB_OTLP_STREAM` | `"mentisdb"` | Value of the `stream-name` header (OpenObserve) |
//! | `MENTISDB_DEPLOY_ENV` | `"homelab"` | `deployment.environment` resource attribute |

use std::io::IsTerminal;
use std::time::Duration;

use tracing_log::LogTracer;
use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Registry};

// Environment variable names
const ENV_OTLP_ENDPOINT: &str = "MENTISDB_OTLP_ENDPOINT";
const ENV_OTLP_AUTH: &str = "MENTISDB_OTLP_AUTH";
const ENV_OTLP_STREAM: &str = "MENTISDB_OTLP_STREAM";
const ENV_DEPLOY_ENV: &str = "MENTISDB_DEPLOY_ENV";
const DEFAULT_STREAM: &str = "mentisdb";
const DEFAULT_DEPLOY_ENV: &str = "homelab";

/// Errors that can occur during telemetry initialization.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryInitError {
    /// The global tracing subscriber has already been set.
    #[error("global tracing subscriber already set")]
    AlreadyInitialized,

    /// Failed to build an OTLP exporter.
    #[error("OTLP exporter build error: {0}")]
    OtlpBuild(String),
}

/// RAII guard for graceful telemetry shutdown.
///
/// Drop this at the end of `main()` so batched OTLP exports flush
/// cleanly. Held by `mentisdbd::main` for the daemon's lifetime.
pub struct TelemetryGuard {
    tracer_provider: Option<opentelemetry_sdk::trace::TracerProvider>,
    logger_provider: Option<opentelemetry_sdk::logs::LoggerProvider>,
}

impl std::fmt::Debug for TelemetryGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryGuard")
            .field("tracer_provider", &self.tracer_provider.is_some())
            .field("logger_provider", &self.logger_provider.is_some())
            .finish()
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(tp) = self.tracer_provider.take() {
            if let Err(e) = tp.shutdown() {
                eprintln!("telemetry: tracer provider shutdown error: {e}");
            }
        }
        if let Some(lp) = self.logger_provider.take() {
            if let Err(e) = lp.shutdown() {
                eprintln!("telemetry: logger provider shutdown error: {e}");
            }
        }
    }
}

/// Raw OTLP providers, before layers are attached to a subscriber.
struct OtlpProviders {
    tracer_provider: opentelemetry_sdk::trace::TracerProvider,
    logger_provider: opentelemetry_sdk::logs::LoggerProvider,
}

/// Build OTLP providers from the environment.
///
/// Returns `None` when `MENTISDB_OTLP_ENDPOINT` is unset or empty.
fn build_otlp_providers(endpoint: &str) -> Result<Option<OtlpProviders>, TelemetryInitError> {
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::{LogExporter, SpanExporter, WithExportConfig, WithHttpConfig};
    use opentelemetry_sdk::logs::LoggerProvider;
    use opentelemetry_sdk::trace::TracerProvider;
    use opentelemetry_sdk::{runtime, Resource};

    if endpoint.is_empty() {
        return Ok(None);
    }

    // Building an OTLP exporter (via reqwest) requires a running Tokio
    // reactor. Return None gracefully when called outside one (e.g. in
    // tests). Export failures from an unreachable endpoint are async and
    // do not affect init.
    if tokio::runtime::Handle::try_current().is_err() {
        return Ok(None);
    }

    let resource = Resource::new(vec![
        KeyValue::new("service.name", "mentisdbd"),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
        KeyValue::new(
            "deployment.environment",
            std::env::var(ENV_DEPLOY_ENV)
                .unwrap_or_else(|_| DEFAULT_DEPLOY_ENV.to_string()),
        ),
    ]);

    let mut headers = std::collections::HashMap::new();
    if let Ok(auth) = std::env::var(ENV_OTLP_AUTH) {
        if !auth.is_empty() {
            headers.insert("Authorization".to_string(), auth);
        }
    }
    let stream = std::env::var(ENV_OTLP_STREAM)
        .unwrap_or_else(|_| DEFAULT_STREAM.to_string());
    headers.insert("stream-name".to_string(), stream);

    // --- Traces ---
    let span_exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .with_timeout(Duration::from_secs(10))
        .with_headers(headers.clone())
        .build()
        .map_err(|e| TelemetryInitError::OtlpBuild(e.to_string()))?;

    let tracer_provider = TracerProvider::builder()
        .with_batch_exporter(span_exporter, runtime::Tokio)
        .with_resource(resource.clone())
        .build();

    // --- Logs ---
    let log_exporter = LogExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .with_timeout(Duration::from_secs(10))
        .with_headers(headers)
        .build()
        .map_err(|e| TelemetryInitError::OtlpBuild(e.to_string()))?;

    let logger_provider = LoggerProvider::builder()
        .with_batch_exporter(log_exporter, runtime::Tokio)
        .with_resource(resource)
        .build();

    Ok(Some(OtlpProviders {
        tracer_provider,
        logger_provider,
    }))
}

/// Initialize telemetry.
///
/// Returns a `TelemetryGuard` whose `Drop` flushes in-flight batched
/// exports. Safe to call once per process; returns
/// `TelemetryInitError::AlreadyInitialized` on subsequent calls.
pub fn init() -> Result<TelemetryGuard, TelemetryInitError> {
    // Set the log→tracing bridge first so `log::…!` macros are captured.
    // Ignore the error — in tests, multiple calls share a process and this
    // will have already been set.
    let _ = LogTracer::init();

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // Check for OTLP endpoint.
    let endpoint = std::env::var(ENV_OTLP_ENDPOINT).unwrap_or_default();
    let otlp = build_otlp_providers(&endpoint)?;

    // Use set_global_default directly to avoid try_init() also calling
    // LogTracer::init() internally, which would conflict with the call above.
    //
    // tracing-subscriber's layered types are not object-safe in a way that
    // would let us erase the full subscriber type into Box<dyn Subscriber>.
    // Instead we spell out the four concrete paths (tty/json × otlp/no-otlp).
    // Each branch produces a different but fully monomorphic subscriber type.
    let (tracer_provider, logger_provider) = if std::io::stdout().is_terminal() {
        let fmt_layer = tracing_subscriber::fmt::layer().with_ansi(true);
        match otlp {
            Some(providers) => {
                use opentelemetry::trace::TracerProvider as _;
                let tracer = providers.tracer_provider.tracer("mentisdbd");
                let trace_layer = tracing_opentelemetry::layer().with_tracer(tracer);
                let log_layer = opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                    &providers.logger_provider,
                );
                let subscriber = Registry::default()
                    .with(env_filter)
                    .with(fmt_layer)
                    .with(trace_layer)
                    .with(log_layer);
                tracing::subscriber::set_global_default(subscriber)
                    .map_err(|_| TelemetryInitError::AlreadyInitialized)?;
                (Some(providers.tracer_provider), Some(providers.logger_provider))
            }
            None => {
                let subscriber = Registry::default()
                    .with(env_filter)
                    .with(fmt_layer);
                tracing::subscriber::set_global_default(subscriber)
                    .map_err(|_| TelemetryInitError::AlreadyInitialized)?;
                (None, None)
            }
        }
    } else {
        let fmt_layer = tracing_subscriber::fmt::layer().json().with_ansi(false);
        match otlp {
            Some(providers) => {
                use opentelemetry::trace::TracerProvider as _;
                let tracer = providers.tracer_provider.tracer("mentisdbd");
                let trace_layer = tracing_opentelemetry::layer().with_tracer(tracer);
                let log_layer = opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                    &providers.logger_provider,
                );
                let subscriber = Registry::default()
                    .with(env_filter)
                    .with(fmt_layer)
                    .with(trace_layer)
                    .with(log_layer);
                tracing::subscriber::set_global_default(subscriber)
                    .map_err(|_| TelemetryInitError::AlreadyInitialized)?;
                (Some(providers.tracer_provider), Some(providers.logger_provider))
            }
            None => {
                let subscriber = Registry::default()
                    .with(env_filter)
                    .with(fmt_layer);
                tracing::subscriber::set_global_default(subscriber)
                    .map_err(|_| TelemetryInitError::AlreadyInitialized)?;
                (None, None)
            }
        }
    };

    Ok(TelemetryGuard {
        tracer_provider,
        logger_provider,
    })
}
