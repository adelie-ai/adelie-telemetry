//! Metrics-registry acceptance criteria.
//!
//! Every test drives a registry it owns, with a clock it owns, so nothing here depends on
//! real time or on the order the tests run in.

use std::sync::Arc;
use std::time::Duration;

use adelie_telemetry::clock::ManualClock;
use adelie_telemetry::metrics::{
    DURATION_BUCKETS_MS, Label, OVERFLOW_LABEL_KEY, OVERFLOW_LABEL_VALUE, Registry, Settings,
};

fn registry(dump_interval: Duration, cardinality_cap: usize) -> (Registry, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new());
    let registry = Registry::new(
        Settings {
            dump_interval,
            cardinality_cap,
        },
        Arc::clone(&clock) as Arc<dyn adelie_telemetry::clock::Clock>,
    );
    (registry, clock)
}

/// With default features, recording instruments and advancing the injected clock past the
/// interval produces a summary with a window delta and a running total.
#[test]
fn metrics_accumulate_without_otel() {
    let (registry, clock) = registry(Duration::from_secs(600), 64);
    let labels = [Label::new("provider", "example")];

    registry.add("llm.tokens.input", 120, &labels);
    registry.add("llm.tokens.input", 80, &labels);
    registry.increment("llm.requests", &labels);
    registry.record_duration("llm.latency", Duration::from_millis(300), &labels);

    assert!(
        registry.dump_if_due().is_none(),
        "nothing is due before the interval has passed"
    );

    clock.advance(Duration::from_secs(600));
    let first = registry.dump_if_due().expect("the first window is due");

    assert_eq!(first.window, Duration::from_secs(600));
    let tokens = first
        .counters
        .iter()
        .find(|counter| counter.name == "llm.tokens.input")
        .expect("the counter must appear in the summary");
    assert_eq!(tokens.window_delta, 200);
    assert_eq!(tokens.total, 200);

    let latency = first
        .histograms
        .iter()
        .find(|histogram| histogram.name == "llm.latency")
        .expect("the histogram must appear in the summary");
    assert_eq!(latency.window.count, 1);
    assert_eq!(latency.total.count, 1);

    // A second window must report only what happened in it, alongside a total that keeps
    // rising. A cumulative-only number stops moving on a long-lived process.
    registry.add("llm.tokens.input", 50, &labels);
    registry.record_duration("llm.latency", Duration::from_millis(90_000), &labels);
    clock.advance(Duration::from_secs(600));
    let second = registry.dump_if_due().expect("the second window is due");

    let tokens = second
        .counters
        .iter()
        .find(|counter| counter.name == "llm.tokens.input")
        .expect("the counter must still appear");
    assert_eq!(
        tokens.window_delta, 50,
        "the window must report only this window's traffic"
    );
    assert_eq!(
        tokens.total, 250,
        "the total must carry every measurement since the process started"
    );

    let latency = second
        .histograms
        .iter()
        .find(|histogram| histogram.name == "llm.latency")
        .expect("the histogram must still appear");
    assert_eq!(latency.window.count, 1);
    assert_eq!(latency.total.count, 2);
    assert_eq!(
        second.uptime,
        Duration::from_secs(1_200),
        "uptime must span every window, not just the last one"
    );
}

/// A dump interval of zero means no summary is ever emitted.
#[test]
fn metrics_dump_interval_zero_disables() {
    let (registry, clock) = registry(Duration::ZERO, 64);
    registry.increment("llm.requests", &[]);

    for _ in 0..5 {
        clock.advance(Duration::from_secs(86_400));
        assert!(
            registry.dump_if_due().is_none(),
            "a zero interval must never produce a summary, however much time passes"
        );
    }

    assert_eq!(
        registry.snapshot().counters.len(),
        1,
        "the registry still accumulates; only the periodic write is off"
    );
}

/// Past the cap, further labels land in `other` and the registry stops growing.
#[test]
fn metrics_cardinality_cap_folds_into_other() {
    let cap = 4;
    let (registry, _clock) = registry(Duration::from_secs(600), cap);

    for index in 0..100 {
        registry.increment("tool.calls", &[Label::new("tool", format!("tool-{index}"))]);
    }

    assert_eq!(
        registry.series_count(),
        cap + 1,
        "the registry must hold the capped label sets plus exactly one overflow series"
    );

    let summary = registry.snapshot();
    let overflow = summary
        .counters
        .iter()
        .find(|counter| {
            counter
                .labels
                .iter()
                .any(|label| label.key() == OVERFLOW_LABEL_KEY)
        })
        .expect("an overflow series must exist once the cap is passed");

    assert_eq!(overflow.labels[0].value(), OVERFLOW_LABEL_VALUE);
    assert_eq!(
        overflow.total, 96,
        "every measurement past the cap must be counted, not dropped"
    );

    let counted: u64 = summary.counters.iter().map(|counter| counter.total).sum();
    assert_eq!(counted, 100, "no measurement may be lost to the cap");
}

/// The cap is per metric name, so one noisy metric cannot crowd out another.
#[test]
fn metrics_cardinality_cap_is_per_metric() {
    let (registry, _clock) = registry(Duration::from_secs(600), 2);

    for index in 0..10 {
        registry.increment("tool.calls", &[Label::new("tool", format!("tool-{index}"))]);
        registry.increment(
            "llm.requests",
            &[Label::new("model", format!("model-{index}"))],
        );
    }

    assert_eq!(
        registry.series_count(),
        6,
        "each metric gets its own cap of 2 plus its own overflow series"
    );
}

/// The in-process path and the OTLP path report the same bucket boundaries.
#[test]
fn histogram_buckets_match_otlp_export() {
    let (registry, _clock) = registry(Duration::from_secs(600), 64);
    registry.record_duration("llm.latency", Duration::from_millis(300), &[]);

    let summary = registry.snapshot();
    let histogram = &summary.histograms[0];

    let mut expected: Vec<f64> = DURATION_BUCKETS_MS.to_vec();
    expected.push(f64::INFINITY);

    assert_eq!(
        histogram.total.bounds(),
        expected,
        "the in-process dump must report the shared boundaries, ending in the overflow bucket"
    );
    assert_eq!(histogram.window.bounds(), expected);
}

/// A measurement lands in the bucket its value belongs to, and the tail is not lost.
#[test]
fn histogram_places_measurements_in_the_right_buckets() {
    let (registry, _clock) = registry(Duration::from_secs(600), 64);

    registry.record_duration("turn.duration", Duration::from_millis(3), &[]);
    registry.record_duration("turn.duration", Duration::from_millis(300), &[]);
    registry.record_duration("turn.duration", Duration::from_secs(240), &[]);
    registry.record_duration("turn.duration", Duration::from_secs(3_600), &[]);

    let summary = registry.snapshot();
    let snapshot = &summary.histograms[0].total;

    assert_eq!(snapshot.count, 4);
    let overflow = snapshot
        .buckets
        .last()
        .expect("there is always an overflow bucket");
    assert_eq!(overflow.upper_bound_ms, f64::INFINITY);
    assert_eq!(
        overflow.count, 1,
        "an hour-long measurement must be visible, not silently clamped"
    );

    let five_ms = snapshot
        .buckets
        .iter()
        .find(|bucket| bucket.upper_bound_ms == 5.0)
        .expect("5ms is a boundary");
    assert_eq!(five_ms.count, 1);

    // The tail is what a mean hides, which is the reason for buckets at all.
    assert_eq!(snapshot.quantile_ms(0.5), Some(500.0));
    assert_eq!(snapshot.quantile_ms(0.99), Some(f64::INFINITY));
    assert_eq!(
        snapshot.mean_ms(),
        Some((3.0 + 300.0 + 240_000.0 + 3_600_000.0) / 4.0)
    );
}

/// An empty histogram reports nothing rather than a misleading zero.
#[test]
fn histogram_reports_nothing_when_empty() {
    let (registry, clock) = registry(Duration::from_secs(60), 64);
    clock.advance(Duration::from_secs(60));

    let summary = registry.dump_if_due().expect("the window is due");
    assert!(summary.is_empty());
    assert!(summary.counters.is_empty());
    assert!(summary.histograms.is_empty());
}

/// Label order at the call site must not create two series for one thing.
#[test]
fn label_order_does_not_split_a_series() {
    let (registry, _clock) = registry(Duration::from_secs(600), 64);

    registry.increment(
        "tool.calls",
        &[Label::new("tool", "search"), Label::new("outcome", "ok")],
    );
    registry.increment(
        "tool.calls",
        &[Label::new("outcome", "ok"), Label::new("tool", "search")],
    );

    assert_eq!(registry.series_count(), 1);
    assert_eq!(registry.snapshot().counters[0].total, 2);
}

/// The summary is ordered, so a test and an operator both read the same thing twice.
#[test]
fn summary_is_deterministically_ordered() {
    let (registry, _clock) = registry(Duration::from_secs(600), 64);

    registry.increment("z.metric", &[Label::new("k", "b")]);
    registry.increment("a.metric", &[Label::new("k", "b")]);
    registry.increment("a.metric", &[Label::new("k", "a")]);

    let names: Vec<_> = registry
        .snapshot()
        .counters
        .iter()
        .map(|counter| (counter.name, counter.labels[0].value().to_owned()))
        .collect();

    assert_eq!(
        names,
        vec![
            ("a.metric", "a".to_owned()),
            ("a.metric", "b".to_owned()),
            ("z.metric", "b".to_owned()),
        ]
    );
}

/// Reconfiguring keeps what has already been recorded.
#[test]
fn reconfigure_keeps_existing_measurements() {
    let (registry, _clock) = registry(Duration::from_secs(600), 64);
    registry.increment("early.metric", &[]);

    let clock = Arc::new(ManualClock::new());
    registry.reconfigure(
        Settings {
            dump_interval: Duration::from_secs(60),
            cardinality_cap: 8,
        },
        Arc::clone(&clock) as Arc<dyn adelie_telemetry::clock::Clock>,
    );

    assert_eq!(registry.settings().cardinality_cap, 8);
    assert_eq!(
        registry.snapshot().counters[0].total,
        1,
        "a measurement taken before init must survive init"
    );

    clock.advance(Duration::from_secs(60));
    assert!(registry.dump_if_due().is_some(), "the new interval applies");
}

/// A label value can never forge a log line.
///
/// The value reaches the console inside a field. A newline in it would produce what reads
/// as a second genuine line, with a real timestamp column, level and target. A remote MCP
/// server names its own tools, so this value is not ours to trust.
#[test]
fn label_value_cannot_forge_a_log_line() {
    let hostile = "search\n2026-08-07T00:00:00Z ERROR adele_daemon: database wiped";
    let label = Label::new("tool", hostile);

    assert!(
        !label.value().contains('\n'),
        "a newline in a label value would start a forged log line: {:?}",
        label.value()
    );
    assert!(!label.value().contains('\r'));
    assert!(
        !label.value().contains('\u{1b}'),
        "an ANSI escape survives with_ansi(false), which only disables the formatter's own colour"
    );

    // The readable part is kept, so sanitising does not destroy the diagnostic.
    assert!(label.value().starts_with("search"));
}

/// A label value cannot grow the registry without bound.
///
/// The cardinality cap bounds the number of series, not the bytes each one retains.
#[test]
fn label_value_is_truncated() {
    let huge = "x".repeat(4 * 1024 * 1024);
    let label = Label::new("model", huge);

    assert!(
        label.value().len() <= adelie_telemetry::metrics::MAX_LABEL_VALUE_BYTES,
        "retained bytes must be bounded, found {}",
        label.value().len()
    );
}

/// Truncation must not split a character in half.
#[test]
fn label_value_truncation_respects_character_boundaries() {
    let wide = "\u{1f600}".repeat(4 * 1024 * 1024);
    let label = Label::new("model", wide);
    assert!(label.value().len() <= adelie_telemetry::metrics::MAX_LABEL_VALUE_BYTES);
    assert!(
        label
            .value()
            .chars()
            .all(|character| character == '\u{1f600}')
    );
}

/// One label set costs one slot, whether it is used as a counter, a histogram or both.
#[test]
fn cardinality_cap_counts_a_label_set_once_across_instruments() {
    let cap = 4;
    let (registry, _clock) = registry(Duration::from_secs(600), cap);

    for index in 0..cap {
        let labels = [Label::new("tool", format!("tool-{index}"))];
        registry.increment("tool.calls", &labels);
        registry.record_duration("tool.calls", Duration::from_millis(10), &labels);
    }

    // Four label sets, each used twice. That is four slots, not eight, so a fifth still
    // fits under a cap of four... and the fifth is what proves the budget was not spent.
    assert_eq!(
        registry.series_count(),
        cap * 2,
        "each of the {cap} label sets should hold one counter series and one histogram series"
    );

    let summary = registry.snapshot();
    assert!(
        !summary.counters.iter().any(|counter| counter
            .labels
            .iter()
            .any(|label| label.key() == OVERFLOW_LABEL_KEY)),
        "no label set should have overflowed: the budget was spent twice per label set"
    );
}

/// Reconfiguring starts a fresh window, so the first delta is not inflated by history.
#[test]
fn reconfigure_starts_a_fresh_window() {
    let (registry, _clock) = registry(Duration::from_secs(600), 64);
    registry.add("early.metric", 40, &[]);

    let clock = Arc::new(ManualClock::new());
    registry.reconfigure(
        Settings {
            dump_interval: Duration::from_secs(60),
            cardinality_cap: 64,
        },
        Arc::clone(&clock) as Arc<dyn adelie_telemetry::clock::Clock>,
    );

    registry.add("early.metric", 2, &[]);
    clock.advance(Duration::from_secs(60));
    let summary = registry.dump_if_due().expect("the window is due");

    let counter = &summary.counters[0];
    assert_eq!(
        counter.window_delta, 2,
        "the window clock restarts at reconfigure, so the window count must restart with it"
    );
    assert_eq!(
        counter.total, 42,
        "the running total still carries what was recorded before init"
    );
}
