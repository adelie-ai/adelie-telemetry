//! Emits one span, one counter and one log record, then flushes and exits.
//!
//! The probe the environment-handling acceptance tests drive. Each of them gives the
//! three signals an endpoint of its own, so a signal that was switched off can be seen
//! not to connect at all, rather than only being reported as off.
//!
//! Two knobs, both read from the environment so a test can set them on the child:
//!
//! - `PROBE_SHUTDOWN_BUDGET_MS` calls `Config::with_shutdown_budget`, which is the
//!   in-code override that must outrank the variable.
//! - `PROBE_SUMMARY_INTERVAL_MS` calls `Config::with_metrics_dump_interval`, the other
//!   in-code override that must outrank its variable.
//! - `PROBE_RUNTIME=tokio` runs the same work inside a Tokio runtime.
//!
//! Needs the `otel` feature to export anything. Without it the program still runs, still
//! records, and sends nothing anywhere.

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
    let mut config = Config::new("signals-probe").with_default_filter("info");
    if let Ok(millis) = std::env::var("PROBE_SHUTDOWN_BUDGET_MS")
        && let Ok(millis) = millis.parse::<u64>()
    {
        config = config.with_shutdown_budget(Duration::from_millis(millis));
    }

    if let Ok(millis) = std::env::var("PROBE_SUMMARY_INTERVAL_MS")
        && let Ok(millis) = millis.parse::<u64>()
    {
        config = config.with_metrics_dump_interval(Duration::from_millis(millis));
    }

    let guard = adelie_telemetry::init(config).expect("init must return");

    // The span is entered and dropped inside this block. A span exports when it closes,
    // so one still alive at the flush never reaches a collector, and the traces signal
    // would look switched off when it was not.
    {
        let span = tracing::info_span!("probe_span");
        let _entered = span.enter();
        tracing::info!("a line that must still reach the console");
    }

    metrics::increment("probe.requests", &[Label::new("provider", "example")]);

    // Dropping the guard flushes every signal and writes the final metrics summary.
    drop(guard);
}
