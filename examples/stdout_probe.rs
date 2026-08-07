//! Writes a log line at every level, inside a span and outside one, then exits.
//!
//! The test `console_layer_writes_to_stderr_only` runs this and asserts that its stdout
//! is empty. That check has to happen in a separate process: the only honest way to prove
//! nothing reaches stdout is to look at the real file descriptor of a real process that
//! installed telemetry the way a binary does.
//!
//! The one line this writes to stdout is the marker the test looks for, so that an empty
//! stdout caused by the program failing to start cannot be mistaken for a pass.

use std::time::Duration;

use adelie_telemetry::Config;

fn main() {
    println!("STDOUT-MARKER");

    let guard = adelie_telemetry::init(
        Config::new("stdout-probe")
            .with_default_filter("trace")
            .with_span_close_events(true)
            .with_metrics_dump_interval(Duration::ZERO),
    )
    .expect("telemetry must install");

    tracing::trace!("trace level");
    tracing::debug!("debug level");
    tracing::info!("info level");
    tracing::warn!("warn level");
    tracing::error!("error level");

    let span = tracing::info_span!("probe_span", probe = true);
    {
        let _entered = span.enter();
        tracing::info!("inside a span");
    }

    drop(guard);
}
