//! What a binary tells this crate about itself.

use std::sync::Arc;
use std::time::Duration;

use crate::clock::{Clock, SystemClock};
use crate::metrics::{DEFAULT_CARDINALITY_CAP, DEFAULT_DUMP_INTERVAL};

/// The filter used when neither `RUST_LOG` nor the caller says otherwise.
pub const DEFAULT_FILTER: &str = "info";

/// How long the shutdown guard may spend flushing before it gives up.
///
/// Kubernetes defaults `terminationGracePeriodSeconds` to 30. A shutdown that overruns it
/// is killed part way through, so the budget has to leave the rest of the process room to
/// stop as well.
pub const DEFAULT_SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);

/// How a binary configures its telemetry.
///
/// Everything past the service name has a working default, so the common case is
/// `Config::new("adele-daemon")`. Runtime configuration of the exporters comes from the
/// standard `OTEL_*` environment variables, not from here, so an operator changes where
/// telemetry goes without a rebuild and without a new flag on every binary.
#[derive(Clone, Debug)]
pub struct Config {
    service_name: String,
    default_filter: String,
    metrics_dump_interval: Duration,
    cardinality_cap: usize,
    span_close_events: bool,
    shutdown_budget: Duration,
    clock: Arc<dyn Clock>,
}

impl Config {
    /// A configuration for a binary with this service name.
    ///
    /// The service name reaches the backend as `service.name` and is what separates one
    /// binary's telemetry from another's, so it should be the binary's own name.
    pub fn new(service_name: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
            default_filter: DEFAULT_FILTER.to_owned(),
            metrics_dump_interval: DEFAULT_DUMP_INTERVAL,
            cardinality_cap: DEFAULT_CARDINALITY_CAP,
            span_close_events: false,
            shutdown_budget: DEFAULT_SHUTDOWN_BUDGET,
            clock: Arc::new(SystemClock::new()),
        }
    }

    /// Use this filter when `RUST_LOG` is unset or unparseable.
    ///
    /// A binary that must stay silent unless asked passes something quieter than `info`.
    pub fn with_default_filter(mut self, filter: impl Into<String>) -> Self {
        self.default_filter = filter.into();
        self
    }

    /// Write a metrics summary this often. [`Duration::ZERO`] turns the summary off.
    pub fn with_metrics_dump_interval(mut self, interval: Duration) -> Self {
        self.metrics_dump_interval = interval;
        self
    }

    /// Allow one metric this many distinct label sets before folding the rest together.
    pub fn with_cardinality_cap(mut self, cap: usize) -> Self {
        self.cardinality_cap = cap;
        self
    }

    /// Write a line when a span closes, carrying how long it was open.
    ///
    /// Why this is a knob rather than always off: a closing span is what makes turn
    /// timing visible to somebody reading the log of a running container, where there is
    /// no trace backend to open. It is noisy under a debug filter, so a binary chooses.
    pub fn with_span_close_events(mut self, enabled: bool) -> Self {
        self.span_close_events = enabled;
        self
    }

    /// Spend at most this long flushing telemetry when the guard drops.
    ///
    /// Raise it where losing buffered telemetry matters more than stopping quickly, and
    /// keep it below the termination grace period of whatever runs the process.
    pub fn with_shutdown_budget(mut self, budget: Duration) -> Self {
        self.shutdown_budget = budget;
        self
    }

    /// Measure time with this clock instead of the platform monotonic clock.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The service name this binary reports.
    pub fn service_name(&self) -> &str {
        &self.service_name
    }

    /// The filter used when `RUST_LOG` says nothing.
    pub fn default_filter(&self) -> &str {
        &self.default_filter
    }

    /// How long between metrics summaries.
    pub fn metrics_dump_interval(&self) -> Duration {
        self.metrics_dump_interval
    }

    /// How many distinct label sets one metric may have.
    pub fn cardinality_cap(&self) -> usize {
        self.cardinality_cap
    }

    /// Whether a closing span writes a line.
    pub fn span_close_events(&self) -> bool {
        self.span_close_events
    }

    /// How long the guard may spend flushing.
    pub fn shutdown_budget(&self) -> Duration {
        self.shutdown_budget
    }

    /// The clock in use.
    pub fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }
}
