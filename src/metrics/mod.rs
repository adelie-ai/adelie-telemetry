//! The metrics facade, and the registry behind it.
//!
//! Call sites use the free functions in this module and nothing else. They never reach
//! for an opentelemetry meter directly, because that would make every crate that records
//! a metric depend on opentelemetry whether or not the `otel` feature is on, and a
//! default build must resolve no opentelemetry crate at all.
//!
//! ```
//! use adelie_telemetry::metrics::{self, Label};
//! use std::time::Duration;
//!
//! metrics::increment("llm.requests", &[Label::new("provider", "example")]);
//! metrics::record_duration(
//!     "llm.latency",
//!     Duration::from_millis(1_200),
//!     &[Label::new("provider", "example")],
//! );
//! ```
//!
//! # What may be a label
//!
//! Names, counts and durations. Never content. A label value becomes a series key, so a
//! prompt or a tool argument used as a label would be both a data leak and an unbounded
//! memory leak. The cardinality cap limits the damage; it is not permission.

mod histogram;
mod registry;

use std::sync::{Arc, LazyLock};
use std::time::Duration;

pub use histogram::{Bucket, DURATION_BUCKETS_MS, HistogramSnapshot};
pub use registry::{
    CounterSummary, DEFAULT_CARDINALITY_CAP, DEFAULT_DUMP_INTERVAL, HistogramSummary, Label,
    OVERFLOW_LABEL_KEY, OVERFLOW_LABEL_VALUE, Registry, Settings, Summary,
};

/// The registry the free functions in this module record into.
///
/// It is created on first use rather than by `init`, so a call site that records before
/// the binary installs telemetry still has its measurement counted. `init` reconfigures
/// this registry; it never replaces it.
static GLOBAL: LazyLock<Registry> = LazyLock::new(|| {
    Registry::new(
        Settings::default(),
        Arc::new(crate::clock::SystemClock::new()),
    )
});

/// The registry the free functions record into.
pub fn global() -> &'static Registry {
    &GLOBAL
}

/// Add to a counter.
pub fn add(name: &'static str, value: u64, labels: &[Label]) {
    global().add(name, value, labels);
}

/// Add one to a counter.
pub fn increment(name: &'static str, labels: &[Label]) {
    global().increment(name, labels);
}

/// Record one duration measurement into a fixed-bucket histogram.
pub fn record_duration(name: &'static str, value: Duration, labels: &[Label]) {
    global().record_duration(name, value, labels);
}
