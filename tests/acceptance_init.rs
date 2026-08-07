//! Init acceptance criteria.
//!
//! `init` installs a process-global subscriber, so this file gets a test binary of its
//! own and is the only place that calls it.

use std::sync::Arc;
use std::time::Duration;

use adelie_telemetry::clock::ManualClock;
use adelie_telemetry::{Config, metrics};

/// Two `init` calls in one process both return; the second does not panic.
#[test]
fn init_is_idempotent() {
    let first = adelie_telemetry::init(
        Config::new("acceptance-init").with_metrics_dump_interval(Duration::ZERO),
    );
    assert!(first.is_ok(), "the first init must install telemetry");

    let second = adelie_telemetry::init(
        Config::new("acceptance-init-again").with_metrics_dump_interval(Duration::ZERO),
    );
    assert!(
        second.is_ok(),
        "a second init must be a no-op that returns, not a panic: libraries hosted in \
         process cannot know whether the binary already installed a subscriber"
    );

    // Both guards drop here. Dropping the inert one must not disturb the live one.
    drop(second);
    drop(first);

    tracing::info!("the subscriber still works after the inert guard was dropped");
}

/// `init` configures the global registry rather than replacing it, so a measurement taken
/// before the binary installed telemetry is still counted.
#[test]
fn init_configures_the_global_registry_without_losing_measurements() {
    metrics::increment("test.before.init", &[]);

    let clock = Arc::new(ManualClock::new());
    let _guard = adelie_telemetry::init(
        Config::new("acceptance-init-registry")
            .with_metrics_dump_interval(Duration::from_secs(60))
            .with_clock(Arc::clone(&clock) as Arc<dyn adelie_telemetry::clock::Clock>),
    )
    .expect("init must succeed");

    metrics::increment("test.after.init", &[]);

    clock.advance(Duration::from_secs(60));
    let summary = metrics::global()
        .dump_if_due()
        .expect("the window is due once the injected clock has passed the interval");

    let names: Vec<_> = summary.counters.iter().map(|counter| counter.name).collect();
    assert!(
        names.contains(&"test.before.init"),
        "a measurement recorded before init must survive it, found {names:?}"
    );
    assert!(names.contains(&"test.after.init"));
}
