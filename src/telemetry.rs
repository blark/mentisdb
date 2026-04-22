//! Telemetry: logging, tracing, and metrics for mentisdbd.
//!
//! Replaces `env_logger` with the `tracing` ecosystem. Always writes to
//! stdout (pretty when stdout is a TTY, JSON otherwise). When
//! `MENTISDB_OTLP_ENDPOINT` is set, additionally exports logs, traces,
//! and metrics over OTLP/HTTP to the configured collector.
//!
//! The upstream `log::…!` macros continue to work via the `tracing-log`
//! bridge — no upstream call sites need to change.

use std::io::IsTerminal;

use tracing_log::LogTracer;
use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Registry};

/// Errors that can occur during telemetry initialization.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryInitError {
    /// The global tracing subscriber has already been set.
    #[error("global tracing subscriber already set")]
    AlreadyInitialized,
}

/// RAII guard for graceful telemetry shutdown.
///
/// Drop this at the end of `main()` so batched OTLP exports flush
/// cleanly. Held by `mentisdbd::main` for the daemon's lifetime.
#[derive(Debug)]
pub struct TelemetryGuard {
    _private: (),
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        // OTLP shutdown wiring added in Task 4. Stdout-only path has
        // nothing to flush.
    }
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

    // Use set_global_default directly to avoid try_init() also calling
    // LogTracer::init() internally, which would conflict with the call above.
    let subscriber: Box<dyn tracing::Subscriber + Send + Sync>;
    if std::io::stdout().is_terminal() {
        subscriber = Box::new(
            Registry::default()
                .with(env_filter)
                .with(tracing_subscriber::fmt::layer().with_ansi(true)),
        );
    } else {
        subscriber = Box::new(
            Registry::default()
                .with(env_filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_ansi(false),
                ),
        );
    }

    tracing::subscriber::set_global_default(subscriber)
        .map_err(|_| TelemetryInitError::AlreadyInitialized)?;

    Ok(TelemetryGuard { _private: () })
}
