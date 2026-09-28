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
use crate::safe::Safe;

mod preflight;
mod resource;
mod switches;

use switches::Switches;

/// The endpoint variables, checked together to tell whether any endpoint was set at all.
const ENDPOINT_VARS: &[&str] = &[
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
];

/// Where the OTLP exporters send when no endpoint variable names one.
///
/// The specification's default, and this crate keeps it. Refusing to start would break
/// the ordinary case of a collector on the default port, and exporting nothing would be a
/// second surprise in place of the first. Naming it at startup costs one line and removes
/// the surprise, which is what [`Report::say`] does.
const DEFAULT_ENDPOINT: &str = "http://localhost:4318";

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

/// What `init` says about the export it has just set up.
///
/// Held rather than written where each part is decided, because none of it can be
/// written until the subscriber exists, and the subscriber cannot be built until the
/// layers are. [`say`](Self::say) is called once the console is there to say it on.
#[derive(Clone, Debug)]
pub(crate) struct Report {
    /// The `service.name` the built resource really carries.
    service_name: String,
    /// Which of the two possible sources supplied it.
    service_name_from: &'static str,
    /// Every attribute key the resource carries, so an operator can see whether the
    /// pairs they put in `OTEL_RESOURCE_ATTRIBUTES` arrived.
    resource_attributes: String,
    /// Whether the trace signal exports.
    traces: bool,
    /// Whether the metric signal exports.
    metrics: bool,
    /// Whether the log-record signal exports.
    logs: bool,
    /// How long the guard may spend flushing.
    shutdown_budget: Duration,
    /// No endpoint variable was set, so the OTLP default applies.
    default_endpoint: bool,
    /// `OTEL_SDK_DISABLED` switched every signal off.
    disabled: bool,
    /// What the environment asked for and could not have.
    complaints: Vec<String>,
}

impl Report {
    /// Write what this process will export, and why.
    ///
    /// Every line here exists because a setting that changes nothing and says nothing
    /// costs an operator the time it takes to work out that it was never wired.
    pub(crate) fn say(&self) {
        for complaint in &self.complaints {
            // Sanitised, because the text quotes a value that came from a deployment
            // overlay. A YAML block scalar puts a line break in one by accident long
            // before anybody does it on purpose, and one forged line in `kubectl logs`
            // reads exactly like a real one.
            tracing::warn!(
                detail = %Safe::message(complaint),
                "a telemetry variable could not be honoured"
            );
        }

        if self.disabled {
            tracing::info!(
                "telemetry export is off by request: {} is true. Console logging and the \
                 metrics summary are unaffected",
                switches::SDK_DISABLED_VAR
            );
            return;
        }

        tracing::info!(
            service_name = %Safe::name(&self.service_name),
            service_name_from = self.service_name_from,
            resource_attributes = %Safe::message(&self.resource_attributes),
            traces = on_off(self.traces),
            metrics = on_off(self.metrics),
            logs = on_off(self.logs),
            shutdown_budget_ms =
                u64::try_from(self.shutdown_budget.as_millis()).unwrap_or(u64::MAX),
            "telemetry export is on"
        );

        if self.default_endpoint {
            tracing::info!(
                "no endpoint variable is set, so the OTLP default {DEFAULT_ENDPOINT} \
                 applies. Set {}=true, or {}, {} or {} to none, to export nothing",
                switches::SDK_DISABLED_VAR,
                switches::TRACES_EXPORTER_VAR,
                switches::METRICS_EXPORTER_VAR,
                switches::LOGS_EXPORTER_VAR
            );
        }
    }
}

/// A signal's state, for a log field.
fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

/// The providers that were built, held so they can be flushed and shut down together.
///
/// Each is optional because the environment may switch its signal off, and a pipeline
/// that exists always exports somewhere. All three are `None` when `OTEL_SDK_DISABLED`
/// asked for that.
#[derive(Debug)]
pub(crate) struct Pipelines {
    traces: Option<SdkTracerProvider>,
    metrics: Option<SdkMeterProvider>,
    logs: Option<SdkLoggerProvider>,
    report: Report,
}

impl Pipelines {
    /// Build the pipelines the environment asks for, register them globally, and return
    /// them.
    pub(crate) fn build(config: &Config) -> Result<Self, crate::Error> {
        Self::build_from(config, |name| std::env::var(name).ok())
    }

    /// The same, over a given source of variable values.
    ///
    /// The source is a parameter for the reason the registry takes a clock: a test cannot
    /// set up process-global state without disturbing another test running beside it, and
    /// the environment is the worst case of that.
    fn build_from(
        config: &Config,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, crate::Error> {
        let switches = Switches::resolve(&lookup);
        let (service_name, source) = resource::resolve_service_name(config.service_name(), &lookup);

        let mut report = Report {
            service_name,
            service_name_from: source.as_str(),
            resource_attributes: String::new(),
            traces: switches.traces,
            metrics: switches.metrics,
            logs: switches.logs,
            shutdown_budget: config.shutdown_budget(),
            default_endpoint: ENDPOINT_VARS
                .iter()
                .all(|name| lookup(name).is_none_or(|value| value.trim().is_empty())),
            disabled: switches.sdk_disabled,
            complaints: switches.complaints,
        };

        if switches.sdk_disabled {
            return Ok(Self {
                traces: None,
                metrics: None,
                logs: None,
                report,
            });
        }

        let resource = resource::resource(Resource::builder(), report.service_name.clone());

        // Read back off the built resource rather than taken from the decision that went
        // into it, so what startup reports is what the SDK produced. If the SDK's merge
        // direction ever reversed, this line would say so instead of hiding it.
        report.service_name = resource::service_name_of(&resource);
        report.resource_attributes = resource::attribute_keys(&resource);

        let traces = if switches.traces {
            preflight::check(
                "traces",
                "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            )?;
            let span_exporter = roots::traces_exporter("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")
                .map_err(|error| pipeline_error("traces", &error))?;
            Some(
                SdkTracerProvider::builder()
                    .with_resource(resource.clone())
                    .with_batch_exporter(span_exporter)
                    .build(),
            )
        } else {
            None
        };

        let metrics = if switches.metrics {
            preflight::check(
                "metrics",
                "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
                "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
            )?;
            let metric_exporter = roots::metrics_exporter("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL")
                .map_err(|error| pipeline_error("metrics", &error))?;
            let builder = SdkMeterProvider::builder()
                .with_resource(resource.clone())
                .with_periodic_exporter(metric_exporter);
            Some(attach_histogram_views(builder, config).build())
        } else {
            None
        };

        let logs = if switches.logs {
            preflight::check(
                "logs",
                "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
                "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
            )?;
            let log_exporter = roots::logs_exporter("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL")
                .map_err(|error| pipeline_error("logs", &error))?;
            Some(
                SdkLoggerProvider::builder()
                    .with_resource(resource)
                    .with_batch_exporter(log_exporter)
                    .build(),
            )
        } else {
            None
        };

        if let Some(traces) = traces.as_ref() {
            opentelemetry::global::set_tracer_provider(traces.clone());
        }
        if let Some(metrics) = metrics.as_ref() {
            opentelemetry::global::set_meter_provider(metrics.clone());

            // Any instrument built before that call is bound to the no-op meter provider
            // and would record nothing for the rest of the process. A call site is
            // allowed to record before the binary calls `init`, so drop them and let them
            // be rebuilt.
            crate::metrics::otel_bridge::reset_instruments();
        }

        Ok(Self {
            traces,
            metrics,
            logs,
            report,
        })
    }

    /// What startup should say about this process's export.
    pub(crate) fn report(&self) -> &Report {
        &self.report
    }

    /// Whether the OTLP metrics pipeline was built.
    ///
    /// What decides the in-process metrics summary: while this is true the same series
    /// are already exported as metrics, so writing them to the log as well would be a
    /// second copy of one set of numbers.
    pub(crate) fn metrics_active(&self) -> bool {
        self.metrics.is_some()
    }

    /// The subscriber layers that feed these pipelines, or `None` when there are none.
    ///
    /// A signal that was switched off contributes no layer, so nothing is recorded for it
    /// and nothing is buffered waiting for a pipeline that does not exist.
    ///
    /// # Why `None` and not an empty `Vec`
    ///
    /// `Layer` is implemented for `Vec<L>`, and an empty one is not a layer that does
    /// nothing. Its `max_level_hint` is documented to "default to `OFF` if there are no
    /// inner layers", and its `register_callsite` returns `Interest::never()`. Both feed
    /// the subscriber's global hint, so an empty `Vec` silences **every** layer beside
    /// it, including the console. With `OTEL_SDK_DISABLED=true` that made the whole
    /// process mute. `Option::None` contributes nothing at all, which is what is wanted
    /// here.
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
    pub(crate) fn layers<S>(&self) -> Option<Vec<Box<dyn Layer<S> + Send + Sync>>>
    where
        S: tracing::Subscriber + for<'a> LookupSpan<'a> + Send + Sync + 'static,
    {
        let mut layers: Vec<Box<dyn Layer<S> + Send + Sync>> = Vec::new();

        if let Some(traces) = self.traces.as_ref() {
            layers.push(Box::new(
                tracing_opentelemetry::layer::<S>()
                    .with_tracer(traces.tracer(env!("CARGO_PKG_NAME")))
                    .with_filter(filter_fn(|metadata: &tracing::Metadata<'_>| {
                        metadata.is_span() || *metadata.level() == Level::ERROR
                    })),
            ));
        }

        if let Some(logs) = self.logs.as_ref() {
            let log_layer: OpenTelemetryTracingBridge<SdkLoggerProvider, _> =
                OpenTelemetryTracingBridge::new(logs);
            layers.push(Box::new(log_layer));
        }

        if layers.is_empty() {
            None
        } else {
            Some(layers)
        }
    }

    /// Flush and shut down every pipeline that was built, in the order traces, metrics,
    /// logs, within `budget`.
    ///
    /// Why a budget: each provider's flush and shutdown blocks for up to five seconds
    /// against an unreachable collector, and there are six calls. Thirty seconds in `Drop`
    /// is longer than the thirty-second `terminationGracePeriodSeconds` Kubernetes
    /// defaults to, so the pod is killed part way through shutdown, which is the failure
    /// this telemetry exists to make visible.
    ///
    /// A budget of zero means do not wait at all, and nothing is flushed. Whoever set it
    /// asked for exactly that, so it is stated once and is not a warning. The alternative
    /// was letting `Duration::ZERO` reach `shutdown_with_timeout`, where it times out at
    /// once and warns about lost telemetry on every single stop.
    ///
    /// The work runs on its own thread so the budget can be enforced. A thread that
    /// overruns is left running; the process is exiting, and an exporter that will not
    /// stop must not decide when.
    pub(crate) fn shutdown(&self, budget: Duration) {
        if self.traces.is_none() && self.metrics.is_none() && self.logs.is_none() {
            return;
        }

        if budget.is_zero() {
            tracing::info!(
                "the shutdown budget is zero, so the OTLP pipelines are not flushed; \
                 whatever they had buffered is dropped"
            );
            return;
        }

        let traces = self.traces.clone();
        let metrics = self.metrics.clone();
        let logs = self.logs.clone();

        let (done, finished) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("adelie-telemetry-shutdown".to_owned())
            .spawn(move || {
                flush_and_stop(traces, metrics, logs, budget);
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
                flush_and_stop(
                    self.traces.clone(),
                    self.metrics.clone(),
                    self.logs.clone(),
                    budget,
                );
            }
        }
    }
}

/// Flush and stop whichever providers exist, in the order traces, metrics, logs.
///
/// A failure here is reported and then dropped. The process is on its way out, and there
/// is nowhere left to propagate to.
fn flush_and_stop(
    traces: Option<SdkTracerProvider>,
    metrics: Option<SdkMeterProvider>,
    logs: Option<SdkLoggerProvider>,
    budget: Duration,
) {
    if let Some(traces) = traces {
        report("traces", "flush", traces.force_flush());
        report("traces", "shutdown", traces.shutdown_with_timeout(budget));
    }
    if let Some(metrics) = metrics {
        report("metrics", "flush", metrics.force_flush());
        report("metrics", "shutdown", metrics.shutdown_with_timeout(budget));
    }
    if let Some(logs) = logs {
        report("logs", "flush", logs.force_flush());
        report("logs", "shutdown", logs.shutdown_with_timeout(budget));
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

/// The view that gives a value histogram recorded under `unit` its registered bucket
/// boundaries.
///
/// One of these is added per [`crate::HistogramView`] a binary declared with
/// [`crate::Config::with_histogram_view`]. It matches like [`duration_view`] does - kind
/// and unit, never name - so a binary that records more than one histogram under the same
/// unit shares this view, and one recorded under a different unit needs its own.
fn value_view(
    unit: &'static str,
    boundaries: &'static [f64],
) -> impl Fn(&Instrument) -> Option<Stream> + Send + Sync + 'static {
    move |instrument: &Instrument| {
        if instrument.kind() != InstrumentKind::Histogram || instrument.unit() != unit {
            return None;
        }
        Stream::builder()
            .with_aggregation(Aggregation::ExplicitBucketHistogram {
                boundaries: boundaries.to_vec(),
                record_min_max: true,
            })
            .build()
            .ok()
    }
}

/// Attach the shared duration view and every histogram view `config` declared to a meter
/// provider builder that is not yet built.
///
/// Separated from [`Pipelines::build_from`] so a test can attach these to a builder of its
/// own - one backed by an in-memory reader rather than a live collector - and prove the
/// same code the real pipeline runs reaches a real SDK meter provider. A test that
/// duplicated this loop instead of calling it would pass even if the loop it copied from
/// were deleted.
pub(crate) fn attach_histogram_views(
    builder: opentelemetry_sdk::metrics::MeterProviderBuilder,
    config: &Config,
) -> opentelemetry_sdk::metrics::MeterProviderBuilder {
    let mut builder = builder.with_view(duration_view);
    for view in config.histogram_views() {
        builder = builder.with_view(value_view(view.unit, view.boundaries));
    }
    builder
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
        let histogram = crate::metrics::otel_bridge::build_value_histogram(
            &provider.meter("test"),
            "probe.latency",
            crate::metrics::otel_bridge::DURATION_UNIT,
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

    /// A `Config::with_histogram_view` registration reaches the OTLP export: a
    /// `record_value` call under that unit reports the declared boundaries, read back off
    /// a real export rather than compared against the constant the view was built from.
    ///
    /// This is the acceptance criterion from adelie-ai/adelie-telemetry#19.
    #[cfg(feature = "otel-testing")]
    #[test]
    fn a_registered_histogram_view_reaches_the_otlp_export() {
        use opentelemetry::metrics::MeterProvider as _;
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
        use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader};

        const TOKEN_BOUNDARIES: &[f64] = &[0.0, 64.0, 25_000.0, 1_048_576.0];

        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .with_view(value_view("{token}", TOKEN_BOUNDARIES))
            .build();

        let histogram = crate::metrics::otel_bridge::build_value_histogram(
            &provider.meter("test"),
            "gen_ai.client.token.usage",
            "{token}",
        );
        histogram.record(30_000.0, &[]);

        provider.force_flush().expect("the reader must flush");

        let exported = exporter
            .get_finished_metrics()
            .expect("metrics must export");
        let histogram = exported
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == "gen_ai.client.token.usage")
            .expect("the value histogram must be exported");

        let AggregatedMetrics::F64(MetricData::Histogram(data)) = histogram.data() else {
            panic!("a value histogram must export as an f64 histogram");
        };
        let point = data
            .data_points()
            .next()
            .expect("one measurement was recorded");

        assert_eq!(
            point.bounds().collect::<Vec<f64>>(),
            TOKEN_BOUNDARIES.to_vec(),
            "the OTLP export must use the boundaries registered for this unit"
        );
        assert_eq!(point.count(), 1);
    }

    /// Two views for two different units, registered on the same provider, must not cross
    /// boundaries: a duration histogram keeps the shared duration buckets and a value
    /// histogram under a different unit keeps its own, even though both are the same
    /// `InstrumentKind::Histogram`.
    #[cfg(feature = "otel-testing")]
    #[test]
    fn two_histogram_views_for_two_units_do_not_cross_boundaries() {
        use opentelemetry::metrics::MeterProvider as _;
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
        use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader};

        const TOKEN_BOUNDARIES: &[f64] = &[1.0, 2.0, 3.0];

        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .with_view(duration_view)
            .with_view(value_view("{token}", TOKEN_BOUNDARIES))
            .build();

        let duration_histogram = crate::metrics::otel_bridge::build_value_histogram(
            &provider.meter("test"),
            "probe.latency",
            crate::metrics::otel_bridge::DURATION_UNIT,
        );
        duration_histogram.record(10.0, &[]);

        let value_histogram = crate::metrics::otel_bridge::build_value_histogram(
            &provider.meter("test"),
            "probe.count",
            "{token}",
        );
        value_histogram.record(1.5, &[]);

        provider.force_flush().expect("the reader must flush");

        let exported = exporter
            .get_finished_metrics()
            .expect("metrics must export");
        let bounds_of = |name: &str| -> Vec<f64> {
            let metric = exported
                .iter()
                .flat_map(|resource| resource.scope_metrics())
                .flat_map(|scope| scope.metrics())
                .find(|metric| metric.name() == name)
                .unwrap_or_else(|| panic!("{name} must be exported"));
            let AggregatedMetrics::F64(MetricData::Histogram(data)) = metric.data() else {
                panic!("{name} must export as an f64 histogram");
            };
            data.data_points()
                .next()
                .expect("one measurement was recorded")
                .bounds()
                .collect()
        };

        assert_eq!(bounds_of("probe.latency"), DURATION_BUCKETS_MS.to_vec());
        assert_eq!(bounds_of("probe.count"), TOKEN_BOUNDARIES.to_vec());
    }

    /// Named for the review finding on adelie-ai/adelie-telemetry#20: the
    /// `for view in config.histogram_views()` loop was untested through the public
    /// `Config` surface - the two tests above build a provider by hand and call
    /// `value_view` directly, so deleting that loop in `Pipelines::build_from`, or
    /// hardcoding `DURATION_BUCKETS_MS` in place of `view.boundaries`, left every test
    /// green. This one drives [`attach_histogram_views`], the exact function
    /// `Pipelines::build_from` calls, from a real `Config::with_histogram_view`
    /// registration, with an in-memory reader standing in for the live collector `init`
    /// would otherwise need.
    #[cfg(feature = "otel-testing")]
    #[test]
    fn config_with_histogram_view_reaches_a_real_meter_provider_through_the_real_attach_path() {
        use opentelemetry::metrics::MeterProvider as _;
        use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
        use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader};

        const TOKEN_BOUNDARIES: &[f64] = &[10.0, 20.0, 30.0];

        let config = Config::new("test").with_histogram_view("{token}", TOKEN_BOUNDARIES);
        let exporter = InMemoryMetricExporter::default();
        let builder = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build());
        let provider = attach_histogram_views(builder, &config).build();

        let histogram = crate::metrics::otel_bridge::build_value_histogram(
            &provider.meter("test"),
            "gen_ai.client.token.usage",
            "{token}",
        );
        histogram.record(25.0, &[]);

        provider.force_flush().expect("the reader must flush");

        let exported = exporter
            .get_finished_metrics()
            .expect("metrics must export");
        let metric = exported
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == "gen_ai.client.token.usage")
            .expect("the histogram must be exported");
        let AggregatedMetrics::F64(MetricData::Histogram(data)) = metric.data() else {
            panic!("a value histogram must export as an f64 histogram");
        };
        let point = data
            .data_points()
            .next()
            .expect("one measurement was recorded");

        assert_eq!(
            point.bounds().collect::<Vec<f64>>(),
            TOKEN_BOUNDARIES.to_vec(),
            "the boundaries registered through Config::with_histogram_view must reach the \
             exported histogram, via the same attach_histogram_views the real pipeline \
             calls"
        );
    }
}
