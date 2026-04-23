//! Telemetry: logging, tracing, and metrics for mentisdbd.
//!
//! Replaces `env_logger` with the `tracing` ecosystem. Always writes to
//! stdout (pretty when stdout is a TTY, JSON otherwise). When
//! `MENTISDB_OTLP_ENDPOINT` is set, additionally exports logs, traces,
//! and metrics over OTLP/HTTP to the configured collector.
//!
//! The upstream `log::…!` macros continue to work via the `tracing-log`
//! bridge — no upstream call sites need to change.
//!
//! Metrics are picked up via `tracing_opentelemetry::MetricsLayer`, which
//! intercepts tracing events whose field names begin with `counter.`,
//! `histogram.`, or `monotonic_counter.` and routes them through the
//! `SdkMeterProvider`.
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
use tracing_subscriber::{layer::SubscriberExt, Layer, EnvFilter, Registry};

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
    meter_provider: Option<opentelemetry_sdk::metrics::SdkMeterProvider>,
}

impl std::fmt::Debug for TelemetryGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryGuard")
            .field("tracer_provider", &self.tracer_provider.is_some())
            .field("logger_provider", &self.logger_provider.is_some())
            .field("meter_provider", &self.meter_provider.is_some())
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
        if let Some(mp) = self.meter_provider.take() {
            if let Err(e) = mp.shutdown() {
                eprintln!("telemetry: meter provider shutdown error: {e}");
            }
        }
    }
}

/// Raw OTLP providers, before layers are attached to a subscriber.
struct OtlpProviders {
    tracer_provider: opentelemetry_sdk::trace::TracerProvider,
    logger_provider: opentelemetry_sdk::logs::LoggerProvider,
    meter_provider: opentelemetry_sdk::metrics::SdkMeterProvider,
}

/// Build OTLP providers from the environment.
///
/// Returns `None` when `MENTISDB_OTLP_ENDPOINT` is unset or empty.
fn build_otlp_providers(endpoint: &str) -> Result<Option<OtlpProviders>, TelemetryInitError> {
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter, WithExportConfig, WithHttpConfig};
    use opentelemetry_sdk::logs::LoggerProvider;
    use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
    use opentelemetry_sdk::trace::TracerProvider;
    use opentelemetry_sdk::{runtime, Resource};

    if endpoint.is_empty() {
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
        .with_headers(headers.clone())
        .build()
        .map_err(|e| TelemetryInitError::OtlpBuild(e.to_string()))?;

    let logger_provider = LoggerProvider::builder()
        .with_batch_exporter(log_exporter, runtime::Tokio)
        .with_resource(resource.clone())
        .build();

    // --- Metrics ---
    let metric_exporter = MetricExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .with_timeout(Duration::from_secs(10))
        .with_headers(headers)
        .build()
        .map_err(|e| TelemetryInitError::OtlpBuild(e.to_string()))?;

    let reader = PeriodicReader::builder(metric_exporter, runtime::Tokio)
        .with_interval(Duration::from_secs(60))
        .build();

    let meter_provider = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .build();

    opentelemetry::global::set_meter_provider(meter_provider.clone());

    Ok(Some(OtlpProviders {
        tracer_provider,
        logger_provider,
        meter_provider,
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
    // Vec<Box<dyn Layer<S>>> implements Layer<S>, so we collect all layers
    // into a single Vec and add them via one .with() call. This collapses
    // the 2×2 Cartesian product (tty/json × otlp/no-otlp) into a single
    // construction. Task 6 can add metrics via layers.push(...) with no
    // additional branching.

    type DynLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

    let mut layers: Vec<DynLayer> = Vec::new();

    layers.push(env_filter.boxed());

    if std::io::stdout().is_terminal() {
        layers.push(tracing_subscriber::fmt::layer().with_ansi(true).boxed());
    } else {
        layers.push(tracing_subscriber::fmt::layer().json().with_ansi(false).boxed());
    }

    let (tracer_provider, logger_provider, meter_provider) = match otlp {
        Some(providers) => {
            use opentelemetry::trace::TracerProvider as _;
            let tracer = providers.tracer_provider.tracer("mentisdbd");
            layers.push(tracing_opentelemetry::layer().with_tracer(tracer).boxed());
            layers.push(
                opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                    &providers.logger_provider,
                ).boxed(),
            );
            layers.push(
                tracing_opentelemetry::MetricsLayer::new(providers.meter_provider.clone()).boxed(),
            );
            (
                Some(providers.tracer_provider),
                Some(providers.logger_provider),
                Some(providers.meter_provider),
            )
        }
        None => (None, None, None),
    };

    let subscriber = Registry::default()
        .with(layers);
    if let Err(_) = tracing::subscriber::set_global_default(subscriber) {
        // Subscriber already set. Leak any OTLP providers rather than
        // dropping them: their Drop impl calls a blocking shutdown which
        // tries to flush in-flight batches to the (possibly unreachable)
        // endpoint. We're not the owner of the running subscriber, so
        // there's nothing useful to flush here.
        if let Some(tp) = tracer_provider {
            std::mem::forget(tp);
        }
        if let Some(lp) = logger_provider {
            std::mem::forget(lp);
        }
        if let Some(mp) = meter_provider {
            std::mem::forget(mp);
        }
        return Err(TelemetryInitError::AlreadyInitialized);
    }

    Ok(TelemetryGuard {
        tracer_provider,
        logger_provider,
        meter_provider,
    })
}

/// Register an OpenTelemetry observable gauge that reports the current size of
/// each chain. The provided closure is called by the OTel SDK on each metrics
/// collection cycle (default 60 s) and must be cheap to invoke.
///
/// Uses the global meter provider set during [`init`]. When OTLP is disabled
/// (`MENTISDB_OTLP_ENDPOINT` unset) the global provider is a no-op
/// implementation and this function registers a no-op gauge harmlessly.
///
/// The `_gauge` handle returned by `.build()` is intentionally dropped here;
/// the callback is retained by the meter provider internally and fires on every
/// collection cycle until the meter provider is shut down.
pub fn register_chain_size_gauge<F>(provider: F)
where
    F: Fn() -> Vec<(String, u64)> + Send + Sync + 'static,
{
    use opentelemetry::KeyValue;

    let meter = opentelemetry::global::meter("mentisdbd");
    let _gauge = meter
        .u64_observable_gauge("mentisdb.thought.chain_size")
        .with_description("Number of thoughts in each chain")
        .with_callback(move |observer| {
            for (key, size) in provider() {
                observer.observe(size, &[KeyValue::new("chain_key", key)]);
            }
        })
        .build();
}
