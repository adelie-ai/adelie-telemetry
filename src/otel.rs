//! The three OTLP pipelines, and the layers that feed them.
//!
//! Only compiled with the `otel` feature. With the feature off no opentelemetry crate is
//! resolved at all.
//!
//! # Configuration
//!
//! Every knob comes from the standard `OTEL_*` environment variables. This module passes
//! no endpoint, no protocol, no header and no timeout to the exporter builders, and that
//! is deliberate: `opentelemetry-otlp` resolves each of them itself, in the order
//! programmatic value, then the per-signal variable, then the generic variable, then its
//! own default. Passing a value here would take the top slot and make every variable
//! below it unreachable.
//!
//! The same reasoning applies to the transport. Calling `.with_tonic()` or `.with_http()`
//! fixes the transport in code and makes `OTEL_EXPORTER_OTLP_PROTOCOL` unusable, so
//! neither is called. The plain `.build()` path reads the protocol variable and picks the
//! transport at run time.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::Instrument;
use opentelemetry_sdk::metrics::{Aggregation, InstrumentKind, SdkMeterProvider, Stream};
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::Level;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::registry::LookupSpan;

use crate::config::Config;
use crate::metrics::DURATION_BUCKETS_MS;

/// The bucket boundaries the OTLP view is built from.
///
/// The in-process registry reports the same values, so a measurement falls in the same
/// bucket whichever path reads it.
pub fn duration_bucket_boundaries() -> Vec<f64> {
    DURATION_BUCKETS_MS.to_vec()
}

/// The three providers, held so they can be flushed and shut down together.
#[derive(Debug)]
pub(crate) struct Pipelines {
    traces: SdkTracerProvider,
    metrics: SdkMeterProvider,
    logs: SdkLoggerProvider,
}

impl Pipelines {
    /// Build the three pipelines, register them globally, and return them.
    pub(crate) fn build(config: &Config) -> Result<Self, crate::Error> {
        let resource = Resource::builder()
            .with_service_name(config.service_name().to_owned())
            .build();

        let span_exporter = SpanExporter::builder()
            .build()
            .map_err(|error| pipeline_error("traces", &error))?;
        let traces = SdkTracerProvider::builder()
            .with_resource(resource.clone())
            .with_batch_exporter(span_exporter)
            .build();

        let metric_exporter = MetricExporter::builder()
            .build()
            .map_err(|error| pipeline_error("metrics", &error))?;
        let metrics = SdkMeterProvider::builder()
            .with_resource(resource.clone())
            .with_periodic_exporter(metric_exporter)
            .with_view(duration_view)
            .build();

        let log_exporter = LogExporter::builder()
            .build()
            .map_err(|error| pipeline_error("logs", &error))?;
        let logs = SdkLoggerProvider::builder()
            .with_resource(resource)
            .with_batch_exporter(log_exporter)
            .build();

        opentelemetry::global::set_tracer_provider(traces.clone());
        opentelemetry::global::set_meter_provider(metrics.clone());

        // Any instrument built before that call is bound to the no-op meter provider and
        // would record nothing for the rest of the process. A call site is allowed to
        // record before the binary calls `init`, so drop them and let them be rebuilt.
        crate::metrics::otel_bridge::reset_instruments();

        Ok(Self {
            traces,
            metrics,
            logs,
        })
    }

    /// The subscriber layers that feed these pipelines.
    ///
    /// # Which pipeline owns an event
    ///
    /// `tracing-opentelemetry` turns a tracing event inside a span into a span event, and
    /// `opentelemetry-appender-tracing` exports the same event as a log record. Left
    /// alone, both happen and every event is counted twice.
    ///
    /// The log pipeline wins. The trace layer is filtered down to spans, so an event
    /// reaches the backend exactly once, as a log record. Two reasons: the log record is
    /// the searchable, complete one, and the span event is not complete at all.
    /// `on_event` in `tracing-opentelemetry` silently drops any event that has no span
    /// open around it, so a pipeline built on span events would lose every event emitted
    /// outside a span with no sign that it had.
    ///
    /// The one deliberate exception is `ERROR`. The trace layer sets a span's status to
    /// failed when it sees an error event, and that status is what makes a failed turn
    /// visible in a trace view rather than looking green. Error events are therefore
    /// allowed through to both, and are the only events that appear twice.
    pub(crate) fn layers<S>(&self) -> Vec<Box<dyn Layer<S> + Send + Sync>>
    where
        S: tracing::Subscriber + for<'a> LookupSpan<'a> + Send + Sync + 'static,
    {
        let trace_layer = tracing_opentelemetry::layer::<S>()
            .with_tracer(self.traces.tracer(env!("CARGO_PKG_NAME")))
            .with_filter(filter_fn(|metadata: &tracing::Metadata<'_>| {
                metadata.is_span() || *metadata.level() == Level::ERROR
            }));

        let log_layer: OpenTelemetryTracingBridge<SdkLoggerProvider, _> =
            OpenTelemetryTracingBridge::new(&self.logs);

        vec![Box::new(trace_layer), Box::new(log_layer)]
    }

    /// Flush and shut down all three pipelines, in the order traces, metrics, logs.
    pub(crate) fn shutdown(&self) {
        // A failure here is reported and then dropped. The process is on its way out, and
        // there is nowhere left to propagate to.
        report("traces", "flush", self.traces.force_flush());
        report("traces", "shutdown", self.traces.shutdown());
        report("metrics", "flush", self.metrics.force_flush());
        report("metrics", "shutdown", self.metrics.shutdown());
        report("logs", "flush", self.logs.force_flush());
        report("logs", "shutdown", self.logs.shutdown());
    }
}

/// The view that gives every duration histogram the shared bucket boundaries.
///
/// It selects on the instrument's kind and unit rather than on its name. This crate
/// refuses to own any domain vocabulary, so it cannot enumerate the metrics a binary will
/// record, and a name convention would silently miss any metric that did not follow it.
/// Every histogram the facade creates is a duration in milliseconds, so kind and unit
/// identify exactly the right set.
fn duration_view(instrument: &Instrument) -> Option<Stream> {
    if instrument.kind() != InstrumentKind::Histogram
        || instrument.unit() != crate::metrics::otel_bridge::DURATION_UNIT
    {
        return None;
    }
    build_duration_stream().ok()
}

/// The histogram stream the view installs, boundaries and all.
///
/// Separated so a test can assert that the shared boundaries are ones the SDK accepts.
/// `Stream::build` rejects boundaries that are unsorted, duplicated, NaN or infinite, so
/// a constant that fails validation would silently leave the OTLP path on the SDK default
/// buckets while the in-process path used ours.
pub(crate) fn build_duration_stream() -> Result<Stream, Box<dyn std::error::Error>> {
    Stream::builder()
        .with_aggregation(Aggregation::ExplicitBucketHistogram {
            boundaries: duration_bucket_boundaries(),
            record_min_max: true,
        })
        .build()
}

fn pipeline_error(signal: &'static str, error: &dyn std::fmt::Display) -> crate::Error {
    crate::Error::Pipeline {
        signal,
        message: error.to_string(),
    }
}

fn report(signal: &str, action: &str, result: opentelemetry_sdk::error::OTelSdkResult) {
    if let Err(error) = result {
        tracing::warn!(signal, action, %error, "an OTLP pipeline did not shut down cleanly");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SDK must accept the shared boundaries, or the OTLP path would quietly fall
    /// back to different buckets from the in-process path.
    #[test]
    fn shared_bucket_boundaries_are_valid_for_the_sdk() {
        assert!(
            build_duration_stream().is_ok(),
            "the shared boundaries must pass the SDK's own validation"
        );
    }

    /// The OTLP view is built from the same constant the in-process registry uses.
    #[test]
    fn otlp_view_uses_the_shared_bucket_boundaries() {
        assert_eq!(duration_bucket_boundaries(), DURATION_BUCKETS_MS.to_vec());
    }
}
