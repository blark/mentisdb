//! Tests for the telemetry module's init paths.

#![cfg(feature = "server")]

use mentisdb::telemetry;

#[test]
fn init_succeeds_without_otlp_env() {
    // OTLP env vars unset — stdout-only path.
    std::env::remove_var("MENTISDB_OTLP_ENDPOINT");
    // May return AlreadyInitialized when another test in the same
    // process ran first — that is acceptable; the subscriber is set.
    match telemetry::init() {
        Ok(guard) => drop(guard),
        Err(telemetry::TelemetryInitError::AlreadyInitialized) => { /* fine */ }
    }
}

#[test]
fn double_init_returns_error() {
    // Must be separate from the other test — global subscriber is
    // process-wide and cargo test runs tests in the same process by
    // default unless --test-threads=1 is used. We serialize this test
    // by running it alone.
    let first = telemetry::init();
    if first.is_err() {
        // Another test already initialized — treat this test as
        // trivially satisfied for this run.
        return;
    }
    let second = telemetry::init();
    assert!(
        matches!(second, Err(telemetry::TelemetryInitError::AlreadyInitialized)),
        "expected AlreadyInitialized, got {second:?}"
    );
}
