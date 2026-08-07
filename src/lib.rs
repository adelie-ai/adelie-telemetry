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
pub mod trace_context;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub use config::{Config, DEFAULT_FILTER};
pub use guard::Guard;
#[cfg(feature = "otel")]
pub use otel::duration_bucket_boundaries as otel_duration_bucket_boundaries;
pub use trace_context::{
    SpanId, TraceContextError, TraceId, TraceOrigin, TraceParent, extract_traceparent,
    inject_traceparent, resolve_trace, trace_id_from_uuid,
};

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
/// With the `otel` feature on, the OTLP layers are added beside the console layer rather
/// than in place of it, and are configured from the standard `OTEL_*` environment
/// variables. With the feature off, the metrics registry still accumulates and still
/// writes its periodic summary.
///
/// Calling this a second time in one process is a no-op that returns an inert guard. A
/// library must not call it at all; the binary owns the subscriber.
pub fn init(config: Config) -> Result<Guard, Error> {
    if !guard::claim_init() {
        return Ok(Guard::inert());
    }

    metrics::global().reconfigure(
        metrics::Settings {
            dump_interval: config.metrics_dump_interval(),
            cardinality_cap: config.cardinality_cap(),
        },
        config.clock(),
    );

    #[cfg(feature = "otel")]
    let pipelines = otel::Pipelines::build(&config)?;

    let subscriber = tracing_subscriber::registry()
        .with(console::env_filter(&config))
        .with(console::console_layer(&config, std::io::stderr));

    #[cfg(feature = "otel")]
    let subscriber = subscriber.with(pipelines.layers());

    // A foreign subscriber may already be installed, in which case this process is not
    // ours to configure. Take nothing over, and hand back a guard that owns nothing.
    if subscriber.try_init().is_err() {
        #[cfg(feature = "otel")]
        pipelines.shutdown();
        return Ok(Guard::inert());
    }

    let dump = guard::DumpThread::spawn(config.metrics_dump_interval());

    Ok(Guard::new(
        dump,
        #[cfg(feature = "otel")]
        Some(pipelines),
    ))
}
