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
#[cfg(feature = "otel")]
pub(crate) mod otel_bridge;
mod registry;

use std::cell::RefCell;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use crate::clock::{Clock, SystemClock};

pub use histogram::{Bucket, DURATION_BUCKETS_MS, HistogramSnapshot};
/// Used by the OTLP failure report, which is only compiled with the feature on.
#[cfg(feature = "otel")]
pub(crate) use registry::sanitize;
pub use registry::{
    CounterSummary, DEFAULT_CARDINALITY_CAP, DEFAULT_DUMP_INTERVAL, HistogramSummary, Label,
    MAX_LABEL_VALUE_BYTES, OVERFLOW_LABEL_KEY, OVERFLOW_LABEL_VALUE, Registry, Settings, Summary,
};

/// The registry the free functions in this module record into.
///
/// It is created on first use rather than by `init`, so a call site that records before
/// the binary installs telemetry still has its measurement counted. `init` reconfigures
/// this registry; it never replaces it.
static GLOBAL: LazyLock<Registry> =
    LazyLock::new(|| Registry::new(Settings::default(), Arc::new(SystemClock::new())));

/// The registry this process reports from.
///
/// A [`TestScope`] can point the free functions below at a registry of its own instead,
/// for one thread. Nothing else can: the process registry is the process's.
pub fn global() -> &'static Registry {
    &GLOBAL
}

thread_local! {
    /// The registry this thread records into while a [`TestScope`] is held.
    static SCOPED: RefCell<Option<Arc<Registry>>> = const { RefCell::new(None) };
}

/// The registry this thread records into, or `None` for the process registry.
///
/// `try_with` rather than `with`: a value dropped during thread teardown may record a
/// measurement after this thread-local has already been destroyed, and that must send the
/// measurement to the process registry rather than panic.
fn scoped() -> Option<Arc<Registry>> {
    SCOPED.try_with(|scoped| scoped.borrow().clone()).ok()?
}

/// Add to a counter.
pub fn add(name: &'static str, value: u64, labels: &[Label]) {
    match scoped() {
        Some(registry) => registry.add(name, value, labels),
        None => global().add(name, value, labels),
    }
}

/// Add one to a counter.
pub fn increment(name: &'static str, labels: &[Label]) {
    match scoped() {
        Some(registry) => registry.increment(name, labels),
        None => global().increment(name, labels),
    }
}

/// Record one duration measurement into a fixed-bucket histogram.
pub fn record_duration(name: &'static str, value: Duration, labels: &[Label]) {
    match scoped() {
        Some(registry) => registry.record_duration(name, value, labels),
        None => global().record_duration(name, value, labels),
    }
}

/// A registry that only the thread holding it records into.
///
/// # The problem it solves
///
/// `cargo test` runs the tests in one binary on several threads at once. Without this,
/// every one of them records into the process registry, so two tests that use the same
/// instrument see each other's measurements and an assertion on an exact count fails
/// about half the time. Two consumers met that within an hour of each other and both
/// reached for a file-local mutex, which works and costs every later test its
/// parallelism.
///
/// A scope removes the shared state rather than scheduling access to it. Hold one, record
/// through the ordinary facade functions, and read back what this thread recorded:
///
/// ```
/// use adelie_telemetry::metrics::{self, Label, TestScope};
///
/// let scope = TestScope::new();
/// metrics::increment("llm.requests", &[Label::new("provider", "example")]);
///
/// let summary = scope.snapshot();
/// assert_eq!(summary.counters[0].total, 1);
/// ```
///
/// Bind it. `let _ = TestScope::new()` drops it at once and records nothing.
///
/// # What it covers, and what it does not
///
/// It covers **the thread that holds it**. Code under test that records from a thread it
/// spawned itself, or from a multi-threaded async runtime, records into the process
/// registry as usual. A `#[test]` and a current-thread `#[tokio::test]` both stay on one
/// thread, which is what a consumer's test of its own recording does.
///
/// It changes nothing about the OTLP bridge. A measurement taken inside a scope still
/// reaches whatever meter provider is installed, which in a test is the no-op one.
///
/// # Why it is not behind a feature
///
/// For the reason [`ManualClock`](crate::clock::ManualClock) is not: a consumer's test is
/// a different crate, and a feature it can only turn on through `dev-dependencies` is
/// unified into its normal build anyway. The cost when nothing is scoped is one
/// thread-local read per measurement, beside a mutex this code already takes.
#[derive(Debug)]
pub struct TestScope {
    registry: Arc<Registry>,
    /// Restored on drop, so scopes can nest and the process registry comes back.
    previous: Option<Arc<Registry>>,
}

impl TestScope {
    /// A scope with the default settings and the platform clock.
    #[must_use]
    pub fn new() -> Self {
        Self::with_settings(Settings::default(), Arc::new(SystemClock::new()))
    }

    /// A scope with settings and a clock of its own.
    ///
    /// Pass a [`ManualClock`](crate::clock::ManualClock) to drive a window without waiting
    /// for real time.
    #[must_use]
    pub fn with_settings(settings: Settings, clock: Arc<dyn Clock>) -> Self {
        let registry = Arc::new(Registry::new(settings, clock));
        let previous = SCOPED.with(|scoped| scoped.borrow_mut().replace(Arc::clone(&registry)));
        Self { registry, previous }
    }

    /// The registry this scope owns.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Everything this thread has recorded since the scope began.
    pub fn snapshot(&self) -> Summary {
        self.registry.snapshot()
    }
}

impl Default for TestScope {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TestScope {
    fn drop(&mut self) {
        // Restored rather than cleared, so a nested scope gives the outer one back and a
        // binary keeps recording into the process registry once the last scope ends.
        SCOPED.with(|scoped| {
            *scoped.borrow_mut() = self.previous.take();
        });
    }
}
