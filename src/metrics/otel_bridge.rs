//! Feeds the metrics facade through to OTLP.
//!
//! Only compiled with the `otel` feature.
//!
//! Call sites never touch an opentelemetry meter. They record through the facade, the
//! registry resolves the series (which is where the cardinality cap is applied), and the
//! resolved series arrives here. Both paths therefore report the same name, the same
//! labels and the same overflow folding, rather than two different views of one
//! measurement.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};

use crate::metrics::Label;

/// The unit every duration histogram this crate creates reports in.
///
/// The OTLP view selects on this, so the shared bucket boundaries apply to every duration
/// histogram without this crate having to know what any of them are called.
pub(crate) const DURATION_UNIT: &str = "ms";

type Instruments<T> = OnceLock<Mutex<HashMap<&'static str, T>>>;

static COUNTERS: Instruments<Counter<u64>> = OnceLock::new();
static HISTOGRAMS: Instruments<Histogram<f64>> = OnceLock::new();

fn counters() -> &'static Mutex<HashMap<&'static str, Counter<u64>>> {
    COUNTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn histograms() -> &'static Mutex<HashMap<&'static str, Histogram<f64>>> {
    HISTOGRAMS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Forget every cached instrument.
///
/// An instrument built before the meter provider was installed is bound to the no-op
/// provider for as long as it is held, and would silently record nothing. `init` calls
/// this once the real provider is in place, so a metric recorded before `init` does not
/// poison the instrument for the rest of the process.
pub(crate) fn reset_instruments() {
    lock(counters()).clear();
    lock(histograms()).clear();
}

/// Add to the OTLP counter of this name.
pub(crate) fn add(name: &'static str, value: u64, labels: &[Label]) {
    let attributes = attributes(labels);
    let mut cache = lock(counters());
    let counter = cache
        .entry(name)
        .or_insert_with(|| meter().u64_counter(name).build());
    counter.add(value, &attributes);
}

/// Record a duration, in milliseconds, into the OTLP histogram of this name.
pub(crate) fn record_duration_ms(name: &'static str, millis: f64, labels: &[Label]) {
    record_value(name, millis, DURATION_UNIT, labels);
}

/// Record one measurement into the OTLP histogram of this name, built with this unit.
///
/// Shares the instrument cache with the duration path: a metric name is either a duration
/// histogram or a value histogram for the life of the process, never both, so caching by
/// name alone cannot confuse the two. The boundaries a caller wants for `unit` are applied
/// by a `View` registered at meter-provider construction
/// ([`crate::Config::with_histogram_view`]), not by anything here - an instrument built
/// through this function reports the SDK's default buckets until one is registered for its
/// unit.
pub(crate) fn record_value(name: &'static str, value: f64, unit: &'static str, labels: &[Label]) {
    let attributes = attributes(labels);
    let mut cache = lock(histograms());
    let histogram = cache
        .entry(name)
        .or_insert_with(|| build_value_histogram(&meter(), name, unit));
    histogram.record(value, &attributes);
}

/// Build a histogram over the given unit, for a `View` matching that unit to select.
///
/// Every histogram this bridge creates - duration or value - is built through this one
/// function, and a test builds its instrument the same way, so dropping the unit fails
/// that test instead of silently moving a histogram onto the SDK's default buckets. A
/// duration histogram is `build_value_histogram(meter, name, DURATION_UNIT)`.
pub(crate) fn build_value_histogram(
    meter: &opentelemetry::metrics::Meter,
    name: &'static str,
    unit: &'static str,
) -> Histogram<f64> {
    meter.f64_histogram(name).with_unit(unit).build()
}

fn meter() -> opentelemetry::metrics::Meter {
    opentelemetry::global::meter(env!("CARGO_PKG_NAME"))
}

fn attributes(labels: &[Label]) -> Vec<KeyValue> {
    labels
        .iter()
        .map(|label| KeyValue::new(label.key(), label.value().to_owned()))
        .collect()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A panic while holding this lock would otherwise stop every metric for the rest of
    // the process. Losing telemetry is worse than reusing a cache that is intact anyway.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
