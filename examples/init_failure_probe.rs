//! Installs telemetry, logs a line, records a metric, and exits.
//!
//! The environment decides whether the OTLP side can be built at all. The point of the
//! probe is what the process still has when it cannot: a console layer and a metrics
//! summary. `tests/acceptance_resilience.rs` drives it.
//!
//! Set `PROBE_RUNTIME=tokio` to run inside a Tokio runtime.

use std::time::Duration;

use adelie_telemetry::Config;
use adelie_telemetry::metrics::{self, Label};

fn main() {
    if std::env::var("PROBE_RUNTIME").as_deref() == Ok("tokio") {
        let runtime = tokio::runtime::Runtime::new().expect("a runtime must start");
        runtime.block_on(async { probe() });
        return;
    }
    probe();
}

fn probe() {
    let guard = adelie_telemetry::init(Config::new("init-failure-probe"))
        .expect("init must return even when the OTLP side cannot be built");

    tracing::info!("a line that must still reach the console");
    metrics::increment("probe.requests", &[Label::new("provider", "example")]);
    metrics::record_duration(
        "probe.latency",
        Duration::from_millis(12),
        &[Label::new("provider", "example")],
    );

    // Dropping the guard writes the final metrics summary.
    drop(guard);
}
