//! One telemetry setup for every Adelie Rust binary: traces, metrics and logs,
//! configured the same way everywhere.
//!
//! Every Adelie binary must produce the same diagnostics, with the same knobs, so an
//! operator can take one identifier from a user report and follow that turn through every
//! process that touched it. This crate holds that setup once. It depends on no other
//! Adelie crate, so any binary can take it without taking anything else.
//!
//! Console output is the default and needs no collector. Export to an OpenTelemetry
//! collector is additional, not a replacement, and is available behind the off-by-default
//! `otel` feature.
//!
//! # What this crate owns
//!
//! - Subscriber construction. One `tracing_subscriber` stack, built the same way for
//!   every binary.
//! - The three OTLP pipelines: traces, metrics and log records.
//! - The metrics facade and the in-process registry behind it.
//! - The shutdown guard that flushes the pipelines before the process exits.
//! - Trace-context helpers: a trace id derived from a request id, and `traceparent`
//!   inject and extract.
//!
//! # What this crate refuses
//!
//! - Deciding what to instrument. The call sites choose their spans, their events and
//!   their instruments. This crate names none of them.
//! - Owning any domain vocabulary. It knows nothing about turns, tools, models or
//!   providers. Those names live in the binaries that emit them.
//! - Installing itself. No constructor runs on load and no library calls [`init`].
//!   A binary calls `init` or nothing happens.
//!
//! Anything outside that list belongs to the binary that needs it.
//!
//! # Installing
//!
//! ```no_run
//! # fn main() -> Result<(), adelie_telemetry::Error> {
//! let _guard = adelie_telemetry::init(adelie_telemetry::Config::new("adele-daemon"))?;
//! tracing::info!("started");
//! # Ok(())
//! # }
//! ```
//!
//! Hold the guard for as long as the process should report. Dropping it flushes.
//!
//! # Correlating without installing anything
//!
//! The trace-context helpers are free functions. They need no [`Config`], no [`init`] and
//! no [`Guard`], and they work with the `otel` feature off, so a desktop client that
//! exports nothing can still mint the id that the daemon adopts.
//!
//! ```
//! # use adelie_telemetry::trace_context;
//! let request_id = [7u8; 16]; // in practice, `uuid.into_bytes()`
//! let trace_id = trace_context::trace_id_from_uuid(request_id)?;
//! println!("turn {trace_id}");
//! # Ok::<(), trace_context::TraceContextError>(())
//! ```

pub mod clock;
mod config;
mod console;
mod guard;
pub mod metrics;
#[cfg(feature = "otel")]
mod otel;
mod safe;
pub mod trace_context;

use std::time::Duration;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub use config::{Config, DEFAULT_FILTER, HistogramView};
pub use guard::Guard;
pub use safe::{MAX_MESSAGE_BYTES, MAX_NAME_BYTES, REPLACEMENT, Safe, TRUNCATED};
pub use trace_context::{
    SpanId, TraceContextError, TraceId, TraceOrigin, TraceParent, extract_traceparent,
    inject_traceparent, resolve_trace, resolve_trace_or_mint, trace_id_from_uuid,
};

/// How often the metrics summary is written, and what decided that.
///
/// Anything that chose an interval wins. When nothing did, the OTLP metrics pipeline
/// decides: with it running, those series are already exported as metrics, and a summary
/// would be a second copy of the same numbers in the log signal from every binary in the
/// fleet. With nothing exporting them, the summary is the only place a number appears.
fn metrics_summary_interval(config: &Config, metrics_exporting: bool) -> (Duration, &'static str) {
    match (config.metrics_dump_interval(), metrics_exporting) {
        (Some(interval), _) => (
            interval,
            config
                .metrics_dump_interval_source()
                .unwrap_or("the binary or its environment"),
        ),
        (None, true) => (
            Duration::ZERO,
            "the OTLP metrics pipeline exports the same series",
        ),
        (None, false) => (
            metrics::DEFAULT_DUMP_INTERVAL,
            "no metrics exporter is configured",
        ),
    }
}

/// Why telemetry could not be installed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An OTLP pipeline could not be built. The process is still usable; it simply has
    /// no exporter for that signal.
    #[error("could not build the OTLP {signal} pipeline: {message}")]
    Pipeline {
        /// Which of `traces`, `metrics` or `logs` failed.
        signal: &'static str,
        /// What the exporter builder reported.
        message: String,
    },
}

/// Install telemetry for this process and return the guard that shuts it down.
///
/// Always installs a console layer writing to **stderr**. Never stdout: the MCP stdio
/// transport frames JSON-RPC there, and a stray log line corrupts the protocol stream.
///
/// The console layer goes in whether or not the OTLP pipelines can be built. A collector
/// that cannot be reached costs the process its export and nothing else: it keeps its
/// console logging and its metrics summary, and the reason is written to the log at
/// ERROR. A typo in one environment variable must not be able to silence a process.
///
/// With the `otel` feature on, the OTLP layers are added beside the console layer rather
/// than in place of it, and are configured from the standard `OTEL_*` environment
/// variables. With the feature off, the metrics registry still accumulates and still
/// writes its periodic summary.
///
/// `OTEL_SDK_DISABLED=true` builds no pipeline at all, and `OTEL_TRACES_EXPORTER`,
/// `OTEL_METRICS_EXPORTER` and `OTEL_LOGS_EXPORTER` take `none` to switch one signal off.
/// The console layer and the metrics summary are installed either way.
///
/// One line at INFO says what this process will export: the service name and where it
/// came from, which signals are on, and how long shutdown may take. A variable that was
/// set and could not be honoured is named at WARN.
///
/// Calling this a second time in one process is a no-op that returns an inert guard. A
/// library must not call it at all; the binary owns the subscriber.
pub fn init(config: Config) -> Result<Guard, Error> {
    if !guard::claim_init() {
        return Ok(Guard::inert());
    }

    // The OTLP side is built before the subscriber, because its layers have to go in
    // beside the console layer and layers cannot be added afterwards. A failure is
    // carried, not returned: the console layer and the metrics summary must survive a
    // collector that could not be reached, and they are what an operator falls back on
    // when it cannot. The error is reported once the console exists to report it on.
    //
    // It is also built before the registry is configured, because whether the metrics
    // pipeline exists is what decides the summary interval.
    #[cfg(feature = "otel")]
    let (pipelines, pipeline_error) = match otel::Pipelines::build(&config) {
        Ok(pipelines) => (Some(pipelines), None),
        Err(error) => (None, Some(error)),
    };

    #[cfg(feature = "otel")]
    let metrics_exporting = pipelines
        .as_ref()
        .is_some_and(otel::Pipelines::metrics_active);
    #[cfg(not(feature = "otel"))]
    let metrics_exporting = false;

    let (dump_interval, dump_reason) = metrics_summary_interval(&config, metrics_exporting);

    metrics::global().reconfigure(
        metrics::Settings {
            dump_interval,
            cardinality_cap: config.cardinality_cap(),
        },
        config.clock(),
    );

    let subscriber = tracing_subscriber::registry()
        .with(console::env_filter(&config))
        .with(console::console_layer(&config, std::io::stderr));

    #[cfg(feature = "otel")]
    let subscriber = subscriber.with(pipelines.as_ref().and_then(otel::Pipelines::layers));

    // A foreign subscriber may already be installed, in which case this process is not
    // ours to configure. Take nothing over, and hand back a guard that owns nothing.
    if subscriber.try_init().is_err() {
        #[cfg(feature = "otel")]
        if let Some(pipelines) = pipelines {
            pipelines.shutdown(config.shutdown_budget());
        }
        return Ok(Guard::inert());
    }

    // What the environment asked for and could not have. Reported here rather than where
    // it was found, because `Config::new` runs before any subscriber exists.
    for fault in config.faults() {
        tracing::warn!(
            detail = %safe::Safe::message(fault),
            "a telemetry variable could not be honoured"
        );
    }

    #[cfg(feature = "otel")]
    if let Some(error) = pipeline_error {
        tracing::error!(
            %error,
            configuration = %otel::configuration_summary(),
            "telemetry export is off for this process; console logging and the metrics \
             summary are unaffected"
        );
    }

    // What this process will export, and why. A setting that changes nothing and says
    // nothing costs an operator the time it takes to find out it was never wired.
    #[cfg(feature = "otel")]
    if let Some(pipelines) = pipelines.as_ref() {
        pipelines.report().say();
    }

    // Said whether the summary is on or off. An operator who expected one and does not
    // see it can read which rule took it away instead of guessing.
    tracing::info!(
        interval_ms = u64::try_from(dump_interval.as_millis()).unwrap_or(u64::MAX),
        reason = dump_reason,
        "the metrics summary interval"
    );

    let dump = guard::DumpThread::spawn(dump_interval);

    Ok(Guard::new(
        dump,
        #[cfg(feature = "otel")]
        pipelines,
        #[cfg(feature = "otel")]
        config.shutdown_budget(),
    ))
}
