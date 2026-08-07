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
//! - Installing itself. No constructor runs on load and no library calls `init`.
//!   A binary calls `init` or nothing happens.
//!
//! Anything outside that list belongs to the binary that needs it.
