//! Two tests in one binary must be able to record the same instrument and assert exact
//! counts, running in parallel, without a mutex and without flaking.
//!
//! Every test in this file records through the **facade** - `metrics::increment`,
//! `metrics::add`, `metrics::record_duration` - because that is what a consumer's
//! production code calls. A test that recorded into a registry it built by hand would
//! prove nothing about the shared one.
//!
//! They all use the same instrument names on purpose. Names chosen to be unique per test
//! would hide the race rather than remove it, and the next consumer would meet it again
//! the first time two tests happened to agree on a name.

use std::sync::Arc;
use std::time::Duration;

use adelie_telemetry::clock::{Clock, ManualClock};
use adelie_telemetry::metrics::{self, Label, Settings, TestScope};

/// The one instrument every test here records into.
const REQUESTS: &str = "llm.requests";

/// How many times each test increments it. All different, so a leak between two tests
/// shows up as a wrong number rather than as a coincidence.
const COUNTS: [u64; 8] = [1, 2, 3, 5, 8, 13, 21, 34];

/// Record `times` increments and read back what this thread's registry holds.
fn count_after(scope: &TestScope, times: u64) -> u64 {
    for _ in 0..times {
        metrics::increment(REQUESTS, &[Label::new("provider", "example")]);
    }

    scope
        .snapshot()
        .counters
        .iter()
        .find(|counter| counter.name == REQUESTS)
        .map(|counter| counter.total)
        .unwrap_or_default()
}

/// One test per entry in `COUNTS`, all recording the same instrument at the same time.
///
/// Written out rather than generated, so a failure names which one disagreed.
macro_rules! isolated_count_test {
    ($name:ident, $index:expr) => {
        #[test]
        fn $name() {
            let scope = TestScope::new();
            let expected = COUNTS[$index];

            assert_eq!(
                count_after(&scope, expected),
                expected,
                "a test must see its own measurements and nobody else's"
            );
        }
    };
}

isolated_count_test!(a_test_sees_only_its_own_measurements_1, 0);
isolated_count_test!(a_test_sees_only_its_own_measurements_2, 1);
isolated_count_test!(a_test_sees_only_its_own_measurements_3, 2);
isolated_count_test!(a_test_sees_only_its_own_measurements_4, 3);
isolated_count_test!(a_test_sees_only_its_own_measurements_5, 4);
isolated_count_test!(a_test_sees_only_its_own_measurements_6, 5);
isolated_count_test!(a_test_sees_only_its_own_measurements_7, 6);
isolated_count_test!(a_test_sees_only_its_own_measurements_8, 7);

/// A scope covers every instrument the facade offers, not only counters.
#[test]
fn a_scope_covers_counters_and_histograms() {
    let scope = TestScope::new();

    metrics::add("llm.tokens.input", 120, &[]);
    metrics::add("llm.tokens.input", 80, &[]);
    metrics::record_duration("llm.latency", Duration::from_millis(300), &[]);

    let summary = scope.snapshot();
    let tokens = summary
        .counters
        .iter()
        .find(|counter| counter.name == "llm.tokens.input")
        .expect("the counter must be in this scope");
    assert_eq!(tokens.total, 200);

    let latency = summary
        .histograms
        .iter()
        .find(|histogram| histogram.name == "llm.latency")
        .expect("the histogram must be in this scope");
    assert_eq!(latency.total.count, 1);
}

/// Nothing recorded inside a scope reaches the process registry.
///
/// Without this the scope could be a second copy rather than a replacement, and the
/// process registry would still accumulate every test's measurements - which is the leak,
/// only harder to see.
///
/// The assertion names one instrument rather than counting the process registry's series.
/// A count is the flake this whole file exists to remove: `recording_returns_to_the_...`
/// below records into the process registry on purpose, at the same time, so any total
/// taken here is another test's business as much as this one's.
#[test]
fn a_scope_keeps_its_measurements_out_of_the_process_registry() {
    {
        let _scope = TestScope::new();
        metrics::increment("scope.only.counter", &[]);
    }

    let names: Vec<&str> = metrics::global()
        .snapshot()
        .counters
        .iter()
        .map(|counter| counter.name)
        .collect();
    assert!(
        !names.contains(&"scope.only.counter"),
        "a scoped measurement must not reach the process registry, found {names:?}"
    );
}

/// When the scope ends, the facade goes back to the process registry.
///
/// A scope that did not restore would silently discard everything a binary recorded after
/// its first test, which is worse than the race it replaced.
#[test]
fn recording_returns_to_the_process_registry_when_the_scope_ends() {
    {
        let _scope = TestScope::new();
        metrics::increment("inside.the.scope", &[]);
    }

    metrics::increment("outside.the.scope", &[]);

    let names: Vec<&str> = metrics::global()
        .snapshot()
        .counters
        .iter()
        .map(|counter| counter.name)
        .collect();
    assert!(
        names.contains(&"outside.the.scope"),
        "the facade must record into the process registry again, found {names:?}"
    );
}

/// A scope takes a clock and settings of its own, so a test can drive a window without
/// waiting for real time.
#[test]
fn a_scope_takes_its_own_clock_and_settings() {
    let clock = Arc::new(ManualClock::new());
    let scope = TestScope::with_settings(
        Settings {
            dump_interval: Duration::from_secs(60),
            cardinality_cap: 2,
        },
        Arc::clone(&clock) as Arc<dyn Clock>,
    );

    metrics::increment("windowed.counter", &[]);
    assert!(
        scope.registry().dump_if_due().is_none(),
        "nothing is due before the injected clock passes the interval"
    );

    clock.advance(Duration::from_secs(60));
    let summary = scope
        .registry()
        .dump_if_due()
        .expect("the window is due once the clock has passed the interval");
    assert_eq!(summary.window, Duration::from_secs(60));
}

/// The cardinality cap applies inside a scope exactly as it does outside.
///
/// The scope must be the same `Registry` with a different owner, not a second
/// implementation that drifts from it.
#[test]
fn a_scope_applies_the_cardinality_cap() {
    let scope = TestScope::with_settings(
        Settings {
            dump_interval: Duration::ZERO,
            cardinality_cap: 2,
        },
        Arc::new(ManualClock::new()) as Arc<dyn Clock>,
    );

    for index in 0..5 {
        metrics::increment("capped.counter", &[Label::new("index", index.to_string())]);
    }

    let series = scope
        .snapshot()
        .counters
        .iter()
        .filter(|counter| counter.name == "capped.counter")
        .count();
    assert_eq!(
        series, 3,
        "two label sets fit under the cap and the rest fold into one overflow series"
    );
}
