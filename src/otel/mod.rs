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
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::Instrument;
use opentelemetry_sdk::metrics::{Aggregation, InstrumentKind, SdkMeterProvider, Stream};
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::time::Duration;

use tracing::Level;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::registry::LookupSpan;

use crate::config::Config;
use crate::metrics::DURATION_BUCKETS_MS;

mod preflight;

/// The OTLP variables that are set, for a failure report. Header values are withheld.
pub(crate) fn configuration_summary() -> String {
    preflight::configuration_summary()
}

/// The bucket boundaries the OTLP view is built from.
///
/// The in-process registry reports the same values, so a measurement falls in the same
/// bucket whichever path reads it.
fn duration_bucket_boundaries() -> Vec<f64> {
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

        preflight::check(
            "traces",
            "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        )?;
        let span_exporter = roots::traces_exporter("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")
            .map_err(|error| pipeline_error("traces", &error))?;
        let traces = SdkTracerProvider::builder()
            .with_resource(resource.clone())
            .with_batch_exporter(span_exporter)
            .build();

        preflight::check(
            "metrics",
            "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
        )?;
        let metric_exporter = roots::metrics_exporter("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL")
            .map_err(|error| pipeline_error("metrics", &error))?;
        let metrics = SdkMeterProvider::builder()
            .with_resource(resource.clone())
            .with_periodic_exporter(metric_exporter)
            .with_view(duration_view)
            .build();

        preflight::check(
            "logs",
            "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
        )?;
        let log_exporter = roots::logs_exporter("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL")
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

    /// Flush and shut down all three pipelines, in the order traces, metrics, logs,
    /// within `budget`.
    ///
    /// Why a budget: each provider's flush and shutdown blocks for up to five seconds
    /// against an unreachable collector, and there are six calls. Thirty seconds in `Drop`
    /// is longer than the thirty-second `terminationGracePeriodSeconds` Kubernetes
    /// defaults to, so the pod is killed part way through shutdown, which is the failure
    /// this telemetry exists to make visible.
    ///
    /// The work runs on its own thread so the budget can be enforced. A thread that
    /// overruns is left running; the process is exiting, and an exporter that will not
    /// stop must not decide when.
    pub(crate) fn shutdown(&self, budget: Duration) {
        let traces = self.traces.clone();
        let metrics = self.metrics.clone();
        let logs = self.logs.clone();

        let (done, finished) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("adelie-telemetry-shutdown".to_owned())
            .spawn(move || {
                // A failure here is reported and then dropped. The process is on its way
                // out, and there is nowhere left to propagate to.
                report("traces", "flush", traces.force_flush());
                report("traces", "shutdown", traces.shutdown_with_timeout(budget));
                report("metrics", "flush", metrics.force_flush());
                report("metrics", "shutdown", metrics.shutdown_with_timeout(budget));
                report("logs", "flush", logs.force_flush());
                report("logs", "shutdown", logs.shutdown_with_timeout(budget));
                let _ = done.send(());
            });

        match spawned {
            Ok(_handle) => {
                if finished.recv_timeout(budget).is_err() {
                    tracing::warn!(
                        budget_seconds = budget.as_secs(),
                        "the OTLP pipelines did not shut down within their budget; \
                         buffered telemetry may be lost"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not start the shutdown thread; flushing inline");
                report(
                    "traces",
                    "shutdown",
                    self.traces.shutdown_with_timeout(budget),
                );
                report(
                    "metrics",
                    "shutdown",
                    self.metrics.shutdown_with_timeout(budget),
                );
                report("logs", "shutdown", self.logs.shutdown_with_timeout(budget));
            }
        }
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

/// Exporter construction, and the trust anchors the gRPC transport verifies against.
///
/// The gRPC transport needs its roots passed in explicitly, and that is not obvious.
/// `opentelemetry-otlp` builds its channel with a bare `ClientTlsConfig::new()` for an
/// https endpoint, and tonic's root sets are opt-in booleans on that config which default
/// to false. So a gRPC exporter left to itself verifies against **no roots at all** and
/// rejects every certificate as `UnknownIssuer`, whichever `tls-*-roots` feature is
/// compiled in. Enabling the feature is a no-op on its own; `with_enabled_roots` is what
/// turns it on.
///
/// Only the gRPC path needs this. The HTTP path goes through reqwest, which uses
/// `rustls-platform-verifier` and reads the operating system trust store by itself.
#[cfg(feature = "otel-tls")]
mod roots {
    use opentelemetry_otlp::tonic_types::transport::ClientTlsConfig;
    use opentelemetry_otlp::{
        ExporterBuildError, LogExporter, MetricExporter, SpanExporter, WithTonicConfig,
    };

    fn trusted_roots() -> ClientTlsConfig {
        ClientTlsConfig::new().with_enabled_roots()
    }

    /// Whether this signal resolves to the gRPC transport.
    fn grpc(signal_protocol_var: &str) -> bool {
        super::preflight::resolves_to_grpc(signal_protocol_var)
    }

    pub(super) fn traces_exporter(var: &str) -> Result<SpanExporter, ExporterBuildError> {
        if grpc(var) {
            SpanExporter::builder()
                .with_tonic()
                .with_tls_config(trusted_roots())
                .build()
        } else {
            SpanExporter::builder().build()
        }
    }

    pub(super) fn metrics_exporter(var: &str) -> Result<MetricExporter, ExporterBuildError> {
        if grpc(var) {
            MetricExporter::builder()
                .with_tonic()
                .with_tls_config(trusted_roots())
                .build()
        } else {
            MetricExporter::builder().build()
        }
    }

    pub(super) fn logs_exporter(var: &str) -> Result<LogExporter, ExporterBuildError> {
        if grpc(var) {
            LogExporter::builder()
                .with_tonic()
                .with_tls_config(trusted_roots())
                .build()
        } else {
            LogExporter::builder().build()
        }
    }
}

/// Exporter construction for a build with no TLS backend.
///
/// There are no trust anchors to pass, and no `https` endpoint to use them on: the
/// pre-flight check refuses one before it gets here.
#[cfg(not(feature = "otel-tls"))]
mod roots {
    use opentelemetry_otlp::{ExporterBuildError, LogExporter, MetricExporter, SpanExporter};

    pub(super) fn traces_exporter(_var: &str) -> Result<SpanExporter, ExporterBuildError> {
        SpanExporter::builder().build()
    }

    pub(super) fn metrics_exporter(_var: &str) -> Result<MetricExporter, ExporterBuildError> {
        MetricExporter::builder().build()
    }

    pub(super) fn logs_exporter(_var: &str) -> Result<LogExporter, ExporterBuildError> {
        LogExporter::builder().build()
    }
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

    /// The in-process path and the OTLP path report the same bucket boundaries.
    ///
    /// This is the acceptance criterion from `mcp-core#44`, and it carries the criterion's
    /// name so a failing run says which requirement is unmet.
    ///
    /// The exported bounds are read back off a real export rather than compared against
    /// the constant the view was built from. Asserting that the constant equals itself
    /// passes even when the view matches no instrument at all and every histogram silently
    /// falls back to the SDK defaults.
    #[cfg(feature = "otel-testing")]
    #[test]
    fn histogram_buckets_match_otlp_export() {
        use opentelemetry::KeyValue;
        use opentelemetry::metrics::MeterProvider as _;
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
        use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader};

        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .with_view(duration_view)
            .build();

        // Built through the same function the facade builds its instruments with, so the
        // test sees whatever the bridge would really produce.
        let histogram = crate::metrics::otel_bridge::build_duration_histogram(
            &provider.meter("test"),
            "probe.latency",
        );
        histogram.record(320.0, &[KeyValue::new("provider", "example")]);

        provider.force_flush().expect("the reader must flush");

        let exported = exporter
            .get_finished_metrics()
            .expect("metrics must export");
        let histogram = exported
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == "probe.latency")
            .expect("the histogram must be exported");

        let AggregatedMetrics::F64(MetricData::Histogram(data)) = histogram.data() else {
            panic!("a duration histogram must export as an f64 histogram");
        };
        let point = data
            .data_points()
            .next()
            .expect("one measurement was recorded");

        let exported_bounds = point.bounds().collect::<Vec<f64>>();
        assert_eq!(
            exported_bounds,
            DURATION_BUCKETS_MS.to_vec(),
            "the OTLP export must use the shared boundaries"
        );
        assert_eq!(point.count(), 1);

        // The other path, compared directly rather than assumed. The criterion is that the
        // two agree, so the test has to hold both of them at once.
        let registry = crate::metrics::Registry::new(
            crate::metrics::Settings::default(),
            std::sync::Arc::new(crate::clock::ManualClock::new()),
        );
        registry.record_duration("probe.latency", std::time::Duration::from_millis(320), &[]);
        let in_process = registry.snapshot().histograms[0].total.bounds();

        assert_eq!(
            in_process,
            exported_bounds
                .iter()
                .copied()
                .chain(std::iter::once(f64::INFINITY))
                .collect::<Vec<f64>>(),
            "the in-process dump and the OTLP export must place a measurement in the same \
             bucket; the dump adds the overflow bucket the OTLP form leaves implicit"
        );
    }

    /// The view must select the instruments the facade creates.
    ///
    /// It matches on kind and unit, so a change to either in the bridge silently unhooks
    /// every histogram from the shared boundaries.
    #[test]
    fn the_bridge_creates_instruments_the_view_selects() {
        assert_eq!(
            crate::metrics::otel_bridge::DURATION_UNIT,
            "ms",
            "the view matches on this unit; changing it detaches the shared boundaries"
        );
    }
}
