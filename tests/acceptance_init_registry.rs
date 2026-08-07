//! `init` must configure the global metrics registry, not replace it.
//!
//! This gets a test binary of its own because it calls `init`, and `init` installs a
//! process-global subscriber. Two tests calling it in one process race for the claim, and
//! the loser silently gets an inert guard.

use std::sync::Arc;
use std::time::Duration;

use adelie_telemetry::clock::ManualClock;
use adelie_telemetry::{Config, metrics};

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

    let names: Vec<_> = summary
        .counters
        .iter()
        .map(|counter| counter.name)
        .collect();
    assert!(
        names.contains(&"test.before.init"),
        "a measurement recorded before init must survive it, found {names:?}"
    );
    assert!(names.contains(&"test.after.init"));
}
