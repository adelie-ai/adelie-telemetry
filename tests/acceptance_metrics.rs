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

/// The in-process dump reports the shared bucket boundaries.
///
/// The half of the criterion that needs no collector. `histogram_buckets_match_otlp_export`
/// in `src/otel/mod.rs` holds the other half, and compares the two paths directly.
#[test]
fn histogram_buckets_are_the_shared_boundaries() {
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

    // char::is_control covers C0, C1 and DEL, but the Unicode line separators are
    // categories Zl and Zp and slip past it. Every JSON consumer treats them as a break.
    for separator in ['\u{2028}', '\u{2029}'] {
        let label = Label::new("tool", format!("search{separator}forged"));
        assert!(
            !label.value().contains(separator),
            "U+{:04X} is a line break to a JSON consumer and must not survive",
            separator as u32
        );
    }

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

/// Every bidi control the fleet strips, with the name a reader would look it up by.
///
/// Built here as Rust escapes rather than pasted as literals. These characters are
/// invisible and several tools eat them silently, so a test that carried them as text
/// could assert against something other than what it appears to.
const BIDI_CONTROLS: &[(char, &str)] = &[
    ('\u{061c}', "ARABIC LETTER MARK"),
    ('\u{200e}', "LEFT-TO-RIGHT MARK"),
    ('\u{200f}', "RIGHT-TO-LEFT MARK"),
    ('\u{202a}', "LEFT-TO-RIGHT EMBEDDING"),
    ('\u{202b}', "RIGHT-TO-LEFT EMBEDDING"),
    ('\u{202c}', "POP DIRECTIONAL FORMATTING"),
    ('\u{202d}', "LEFT-TO-RIGHT OVERRIDE"),
    ('\u{202e}', "RIGHT-TO-LEFT OVERRIDE"),
    ('\u{2066}', "LEFT-TO-RIGHT ISOLATE"),
    ('\u{2067}', "RIGHT-TO-LEFT ISOLATE"),
    ('\u{2068}', "FIRST STRONG ISOLATE"),
    ('\u{2069}', "POP DIRECTIONAL ISOLATE"),
];

/// A label value cannot reverse what an operator reads.
///
/// `char::is_control` covers category Cc only. The bidi controls are category Cf, so
/// they went through the sanitiser untouched. They leave the line structure alone and
/// the bytes honest, which is why this is deception rather than forgery: a tool named
/// with U+202E renders in a terminal with everything after it visually reversed, so the
/// name in `kubectl logs` is not the name that was called.
#[test]
fn label_value_cannot_reverse_what_a_reader_sees() {
    for (control, name) in BIDI_CONTROLS {
        let label = Label::new("tool", format!("search{control}reversed"));
        assert!(
            !label.value().contains(*control),
            "U+{:04X} {name} survived the sanitiser: {:?}",
            *control as u32,
            label.value()
        );
    }
}

/// The classic Trojan-source shape, end to end through a real series.
#[test]
fn a_bidi_override_cannot_disguise_a_tool_name() {
    let (registry, _clock) = registry(Duration::from_secs(600), 64);
    let disguised = format!("delete_all{}dnetxe_", '\u{202e}');

    registry.increment("tool.calls", &[Label::new("tool", disguised)]);

    let summary = registry.snapshot();
    let value = summary.counters[0].labels[0].value().to_owned();
    assert!(
        !value.chars().any(|character| BIDI_CONTROLS
            .iter()
            .any(|(control, _)| character == *control)),
        "the recorded label still carries a bidi control: {value:?}"
    );
}

// ---------------------------------------------------------------------------
// `record_value`: a fixed-bucket histogram generic over unit and boundaries,
// for a measurement that is not a duration. adelie-ai/adelie-telemetry#19.
// ---------------------------------------------------------------------------

/// One call to `record_value` produces a value-histogram summary carrying the
/// unit and the boundaries it was recorded with, distinct from the duration
/// histograms in `summary.histograms`.
#[test]
fn record_value_produces_a_value_histogram_summary_with_its_own_unit() {
    const BOUNDARIES: &[f64] = &[10.0, 100.0, 1_000.0];
    let (registry, _clock) = registry(Duration::from_secs(600), 64);

    registry.record_value(
        "gen_ai.client.token.usage",
        42.0,
        "{token}",
        BOUNDARIES,
        &[Label::new("gen_ai.token.type", "input")],
    );

    let summary = registry.snapshot();
    assert!(
        summary.histograms.is_empty(),
        "a value histogram must not be reported as a duration histogram"
    );
    let value_histogram = summary
        .value_histograms
        .iter()
        .find(|histogram| histogram.name == "gen_ai.client.token.usage")
        .expect("the value histogram must appear in the summary");

    assert_eq!(value_histogram.unit, "{token}");
    assert_eq!(value_histogram.total.count, 1);
    assert_eq!(value_histogram.total.sum, 42.0);
    assert_eq!(value_histogram.window.count, 1);
}

/// Two value histograms recorded under different bucket sets keep their own
/// boundaries; recording into one must not reshape the other.
#[test]
fn two_value_histograms_keep_their_own_bucket_boundaries() {
    const NARROW: &[f64] = &[1.0, 2.0];
    const WIDE: &[f64] = &[1_000.0, 1_000_000.0];
    let (registry, _clock) = registry(Duration::from_secs(600), 64);

    registry.record_value("metric.narrow", 1.5, "{unit}", NARROW, &[]);
    registry.record_value("metric.wide", 500_000.0, "{unit}", WIDE, &[]);

    let summary = registry.snapshot();
    let narrow = summary
        .value_histograms
        .iter()
        .find(|histogram| histogram.name == "metric.narrow")
        .expect("the narrow histogram must be present");
    let wide = summary
        .value_histograms
        .iter()
        .find(|histogram| histogram.name == "metric.wide")
        .expect("the wide histogram must be present");

    let mut expected_narrow = NARROW.to_vec();
    expected_narrow.push(f64::INFINITY);
    let mut expected_wide = WIDE.to_vec();
    expected_wide.push(f64::INFINITY);

    assert_eq!(narrow.total.bounds(), expected_narrow);
    assert_eq!(wide.total.bounds(), expected_wide);
}

/// A value lands in the bucket whose upper bound it does not exceed, exactly
/// like the duration histogram - this is the mechanism the token histogram's
/// "over 25000" report depends on.
#[test]
fn value_histogram_places_a_measurement_at_its_exact_boundary() {
    const BOUNDARIES: &[f64] = &[0.0, 64.0, 25_000.0, 32_768.0];
    let (registry, _clock) = registry(Duration::from_secs(600), 64);

    registry.record_value(
        "gen_ai.client.token.usage",
        25_000.0,
        "{token}",
        BOUNDARIES,
        &[],
    );
    registry.record_value(
        "gen_ai.client.token.usage",
        25_001.0,
        "{token}",
        BOUNDARIES,
        &[],
    );

    let summary = registry.snapshot();
    let snapshot = &summary
        .value_histograms
        .iter()
        .find(|histogram| histogram.name == "gen_ai.client.token.usage")
        .expect("the histogram must be present")
        .total;

    let at_boundary = snapshot
        .buckets
        .iter()
        .find(|bucket| bucket.upper_bound == 25_000.0)
        .expect("25000 must be one of the bucket boundaries");
    assert_eq!(
        at_boundary.count, 1,
        "a value equal to the boundary belongs in that bucket, not the next one"
    );

    let next = snapshot
        .buckets
        .iter()
        .find(|bucket| bucket.upper_bound == 32_768.0)
        .expect("32768 must be the next boundary");
    assert_eq!(
        next.count, 1,
        "a value one over the boundary belongs in the next bucket"
    );
}

/// A value histogram with nothing recorded reports nothing, the same rule the
/// duration histogram already holds to.
#[test]
fn value_histogram_reports_nothing_when_empty() {
    let (registry, clock) = registry(Duration::from_secs(60), 64);
    clock.advance(Duration::from_secs(60));

    let summary = registry.dump_if_due().expect("the window is due");
    assert!(summary.is_empty());
    assert!(summary.value_histograms.is_empty());
}

/// A value-histogram series shares the cardinality budget with counters and
/// duration histograms under the same metric name, the same rule
/// `cardinality_cap_counts_a_label_set_once_across_instruments` already holds
/// duration histograms to.
#[test]
fn value_histogram_shares_the_cardinality_budget() {
    let cap = 2;
    let (registry, _clock) = registry(Duration::from_secs(600), cap);
    const BOUNDARIES: &[f64] = &[10.0];

    for index in 0..cap {
        let labels = [Label::new("model", format!("model-{index}"))];
        registry.increment("llm.requests", &labels);
        registry.record_value(
            "gen_ai.client.token.usage",
            1.0,
            "{token}",
            BOUNDARIES,
            &labels,
        );
    }

    assert_eq!(
        registry.series_count(),
        cap * 2,
        "each of the {cap} label sets holds one counter series and one value-histogram series"
    );
}

/// Named for the review finding on adelie-ai/adelie-telemetry#20: a series' unit and
/// boundaries were fixed by whichever call recorded first, guarded only by a doc comment.
/// A later call for the same metric name and label set that disagrees on `unit` must be
/// caught in a debug build rather than silently recording under the first call's unit.
#[test]
#[should_panic(expected = "unit")]
fn record_value_panics_in_a_debug_build_when_a_later_call_disagrees_on_unit() {
    let (registry, _clock) = registry(Duration::from_secs(600), 64);
    registry.record_value("metric.mismatched_unit", 1.0, "{token}", &[10.0], &[]);
    registry.record_value("metric.mismatched_unit", 1.0, "{item}", &[10.0], &[]);
}

/// The same finding, for `boundaries` rather than `unit`: a later call that disagrees
/// would otherwise leave the OTLP export and the in-process histogram silently reading two
/// different bucket sets for what a backend treats as one series.
#[test]
#[should_panic(expected = "boundaries")]
fn record_value_panics_in_a_debug_build_when_a_later_call_disagrees_on_boundaries() {
    let (registry, _clock) = registry(Duration::from_secs(600), 64);
    registry.record_value("metric.mismatched_boundaries", 1.0, "{token}", &[10.0], &[]);
    registry.record_value("metric.mismatched_boundaries", 1.0, "{token}", &[20.0], &[]);
}

/// A zero-width joiner is also category Cf and must survive.
///
/// The boundary is deliberate: the fleet strips the bidi controls, not all of Cf. A
/// joiner carries emoji sequences a person legitimately wants to read, and hiding text
/// is a weaker problem than reversing it.
#[test]
fn a_zero_width_joiner_survives_the_sanitiser() {
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    let label = Label::new("provider", family);
    assert_eq!(
        label.value(),
        family,
        "U+200D ZERO WIDTH JOINER is Cf but not a bidi control, and must be left alone"
    );
}
