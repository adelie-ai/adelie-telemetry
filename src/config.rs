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

/// The variable that sets the shutdown budget, in whole milliseconds.
///
/// **Not one of the `OTEL_*` variables, and deliberately outside that namespace.** The
/// `OTEL_EXPORTER_OTLP_TIMEOUT` family is read by the SDK and means the per-export
/// timeout, which is a different thing. This is not exporter configuration at all: it is
/// how long the process is willing to wait before it stops.
///
/// `0` means do not wait: nothing is flushed and the process stops at once. A value that
/// is not a whole number of milliseconds, or is negative, is named at startup and
/// ignored. [`Config::with_shutdown_budget`] outranks it.
pub const SHUTDOWN_BUDGET_VAR: &str = "ADELIE_TELEMETRY_SHUTDOWN_BUDGET_MS";

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
    /// What the environment asked for and could not have.
    ///
    /// Collected here rather than written where it is found, because `Config::new` runs
    /// before any subscriber exists, so a warning from it would go nowhere. `init`
    /// reports these once the console is there to report them on.
    faults: Vec<String>,
    clock: Arc<dyn Clock>,
}

impl Config {
    /// A configuration for a binary with this service name.
    ///
    /// The service name reaches the backend as `service.name` and is what separates one
    /// binary's telemetry from another's, so it should be the binary's own name.
    ///
    /// `OTEL_SERVICE_NAME` outranks it. An operator sets that variable to tell two
    /// deployments of one binary apart, and it is read per process, so honouring it
    /// renames this process and nothing else.
    ///
    /// The shutdown budget is read from [`SHUTDOWN_BUDGET_VAR`] here, so every binary in
    /// the fleet picks a new one up without a code change.
    pub fn new(service_name: impl Into<String>) -> Self {
        let (shutdown_budget, fault) = resolve_shutdown_budget(|name| std::env::var(name).ok());
        Self {
            service_name: service_name.into(),
            default_filter: DEFAULT_FILTER.to_owned(),
            metrics_dump_interval: DEFAULT_DUMP_INTERVAL,
            cardinality_cap: DEFAULT_CARDINALITY_CAP,
            span_close_events: false,
            shutdown_budget,
            faults: fault.into_iter().collect(),
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
    ///
    /// This outranks [`SHUTDOWN_BUDGET_VAR`], so a binary that must have a particular
    /// budget still gets it. [`Duration::ZERO`] means do not wait at all: nothing is
    /// flushed and the process stops at once.
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

    /// What the environment asked for and could not have, for `init` to report.
    pub(crate) fn faults(&self) -> &[String] {
        &self.faults
    }
}

/// The shutdown budget the environment asks for, and what could not be honoured.
///
/// A value that cannot be honoured is named rather than degraded silently. The whole
/// point of the variable is that an operator can shorten the budget to match a
/// termination grace period, and a typo that quietly leaves the default in place would
/// cost exactly the time the variable exists to save.
///
/// The source is a parameter rather than `std::env` for the same reason the metrics
/// registry takes a clock: a test cannot set up process-global state without disturbing
/// another test running beside it.
fn resolve_shutdown_budget(lookup: impl Fn(&str) -> Option<String>) -> (Duration, Option<String>) {
    let Some(value) = lookup(SHUTDOWN_BUDGET_VAR)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        return (DEFAULT_SHUTDOWN_BUDGET, None);
    };

    // Parsed as signed, so a negative value is refused by name rather than failing to
    // parse as unsigned and being reported as though somebody had written a word.
    let Ok(millis) = value.parse::<i64>() else {
        return (
            DEFAULT_SHUTDOWN_BUDGET,
            Some(format!(
                "{SHUTDOWN_BUDGET_VAR}={value} is not a whole number of milliseconds, so \
                 it was ignored"
            )),
        );
    };

    match u64::try_from(millis) {
        Ok(millis) => (Duration::from_millis(millis), None),
        Err(_) => (
            DEFAULT_SHUTDOWN_BUDGET,
            Some(format!(
                "{SHUTDOWN_BUDGET_VAR}={value} is negative, so it was ignored. Use 0 to \
                 stop without flushing at all"
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lookup over a fixed set of values, touching no process-global state.
    ///
    /// The environment is the worst thing a test can mutate: `std::env::set_var` is
    /// `unsafe` in edition 2024 because `setenv` rewrites a shared array while any other
    /// thread may be reading it. Injecting the lookup removes the mutation rather than
    /// scheduling around it, so every test here runs beside every other.
    fn lookup(value: Option<&str>) -> impl Fn(&str) -> Option<String> + use<> {
        let value = value.map(str::to_owned);
        move |name: &str| {
            if name == SHUTDOWN_BUDGET_VAR {
                value.clone()
            } else {
                None
            }
        }
    }

    #[test]
    fn the_default_applies_when_the_variable_is_unset() {
        let (budget, fault) = resolve_shutdown_budget(lookup(None));

        assert_eq!(budget, DEFAULT_SHUTDOWN_BUDGET);
        assert_eq!(fault, None);
    }

    /// The specification's rule for every variable: an empty value reads as unset.
    #[test]
    fn an_empty_value_reads_as_unset() {
        let (budget, fault) = resolve_shutdown_budget(lookup(Some("   ")));

        assert_eq!(budget, DEFAULT_SHUTDOWN_BUDGET);
        assert_eq!(fault, None);
    }

    #[test]
    fn the_variable_sets_the_budget_in_milliseconds() {
        let (budget, fault) = resolve_shutdown_budget(lookup(Some("1750")));

        assert_eq!(budget, Duration::from_millis(1_750));
        assert_eq!(fault, None);
    }

    /// Zero is a value, not a fault. It means do not wait at all.
    #[test]
    fn zero_is_accepted_and_means_do_not_wait() {
        let (budget, fault) = resolve_shutdown_budget(lookup(Some("0")));

        assert_eq!(budget, Duration::ZERO);
        assert_eq!(fault, None, "zero is a deliberate choice, not a mistake");
    }

    /// A value that is not a whole number of milliseconds is named, not degraded in
    /// silence.
    #[test]
    fn an_unparseable_value_is_named_and_the_default_applies() {
        let (budget, fault) = resolve_shutdown_budget(lookup(Some("5s")));

        assert_eq!(budget, DEFAULT_SHUTDOWN_BUDGET);
        let fault = fault.expect("an unusable value must be reported");
        assert!(fault.contains(SHUTDOWN_BUDGET_VAR));
        assert!(
            fault.contains("5s"),
            "the report must quote the value: {fault}"
        );
    }

    /// A negative value must not wrap into an enormous budget.
    #[test]
    fn a_negative_value_is_named_and_the_default_applies() {
        let (budget, fault) = resolve_shutdown_budget(lookup(Some("-1")));

        assert_eq!(budget, DEFAULT_SHUTDOWN_BUDGET);
        let fault = fault.expect("a negative value must be reported");
        assert!(fault.contains(SHUTDOWN_BUDGET_VAR));
        assert!(fault.contains("negative"), "{fault}");
    }

    /// A number too large for `i64` is a mistake, not a very patient operator.
    #[test]
    fn a_value_too_large_to_parse_is_named_and_the_default_applies() {
        let (budget, fault) = resolve_shutdown_budget(lookup(Some("99999999999999999999")));

        assert_eq!(budget, DEFAULT_SHUTDOWN_BUDGET);
        assert!(fault.is_some());
    }

    /// The in-code setter outranks the variable, so a binary that must have a particular
    /// budget still gets it.
    #[test]
    fn with_shutdown_budget_outranks_whatever_new_resolved() {
        let config = Config::new("test").with_shutdown_budget(Duration::from_millis(120));

        assert_eq!(config.shutdown_budget(), Duration::from_millis(120));
    }
}
