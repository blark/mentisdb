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
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry};

/// Errors that can occur during telemetry initialization.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryInitError {
    /// The global tracing subscriber has already been set.
    #[error("global tracing subscriber already set")]
    AlreadyInitialized,
    /// Installation of the `log` → `tracing` bridge failed.
    #[error("log bridge installation failed: {0}")]
    LogBridge(#[from] tracing_log::log_tracer::SetLoggerError),
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
    // Bridge upstream `log::…!` macros into tracing.
    // Ignore the error if already initialized (e.g., in tests where
    // multiple test functions share a process).
    let _ = LogTracer::init();

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    if std::io::stdout().is_terminal() {
        Registry::default()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().with_ansi(true))
            .try_init()
            .map_err(|_| TelemetryInitError::AlreadyInitialized)?;
    } else {
        Registry::default()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_ansi(false),
            )
            .try_init()
            .map_err(|_| TelemetryInitError::AlreadyInitialized)?;
    }

    Ok(TelemetryGuard { _private: () })
}
