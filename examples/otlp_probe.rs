//! Emits spans, counters, a duration and log records, then flushes and exits.
//!
//! Run it against a collector to confirm that all three signals arrive:
//!
//! ```text
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 \
//! OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf \
//!   cargo run --features otel --example otlp_probe
//! ```
//!
//! Set `PROBE_RUNTIME=tokio` to run the same work inside a Tokio runtime, which the gRPC
//! transport needs and the HTTP transport does not.
//!
//! Needs the `otel` feature. Without it the program still runs, still records, and sends
//! nothing anywhere, which is the behaviour a default build is supposed to have.

use std::time::Duration;

use adelie_telemetry::metrics::{self, Label};
use adelie_telemetry::{Config, trace_context};

fn main() {
    if std::env::var("PROBE_RUNTIME").as_deref() == Ok("tokio") {
        let runtime = tokio::runtime::Runtime::new().expect("a runtime must start");
        runtime.block_on(async { probe() });
        return;
    }
    probe();
}

fn probe() {
    let guard = adelie_telemetry::init(
        Config::new("otlp-probe")
            .with_default_filter("info")
            .with_span_close_events(true)
            .with_metrics_dump_interval(Duration::ZERO),
    )
    .expect("telemetry must install");

    let request_id = [
        0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e, 0x47,
        0x36,
    ];
    let origin = trace_context::resolve_trace(None, request_id).expect("a valid request id");

    // Both spans are dropped inside this block. A span exports when it closes, which is
    // when the last handle to it is dropped, so a span still alive at the flush never
    // reaches the collector.
    {
        let turn = tracing::info_span!("probe_turn", turn_id = %origin.trace_id());
        let _turn = turn.enter();
        tracing::info!(rounds = 1, "a turn ran");

        let round = tracing::info_span!("probe_round", round = 1);
        let _round = round.enter();
        tracing::info!("a round ran");
        tracing::error!("an error event inside a span");
    }

    tracing::info!("an event outside any span");

    let labels = [Label::new("provider", "example")];
    metrics::increment("probe.requests", &labels);
    metrics::add("probe.tokens", 1_234, &labels);
    metrics::record_duration("probe.latency", Duration::from_millis(320), &labels);

    // Dropping the guard flushes and shuts down all three pipelines.
    drop(guard);
}
