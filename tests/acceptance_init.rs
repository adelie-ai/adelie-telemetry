//! Init acceptance criteria.
//!
//! `init` installs a process-global subscriber, so this file gets a test binary of its
//! own and is the only place that calls it.

use std::time::Duration;

use adelie_telemetry::Config;

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
