//! The time source, injected rather than read.
//!
//! Why: the metrics registry decides when a dump is due and measures how long a window
//! lasted. Both are time-dependent, and a test that waits for real time is slow and
//! flaky. Every duration in this crate comes from a [`Clock`], so a test can move time
//! by hand and get the same answer every run.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A monotonic time source.
///
/// The origin is arbitrary and implementation-defined. Only differences between two
/// readings mean anything.
pub trait Clock: Send + Sync + fmt::Debug {
    /// The time now, as a duration since this clock's origin.
    fn now(&self) -> Duration;
}

/// The clock a process uses in production: the platform monotonic clock.
///
/// Why not wall time: a wall clock can step backwards over an NTP correction, which would
/// make a window duration negative and a dump interval never expire.
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    /// A clock whose origin is the moment it was created.
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SystemClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SystemClock")
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// A clock that only moves when a test moves it.
///
/// Shared through an `Arc`, so a test holds one handle and the registry holds another.
#[derive(Debug, Default)]
pub struct ManualClock {
    nanos: AtomicU64,
}

impl ManualClock {
    /// A clock reading zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Move the clock forward.
    pub fn advance(&self, by: Duration) {
        let nanos = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        self.nanos.fetch_add(nanos, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn manual_clock_starts_at_zero_and_only_moves_when_advanced() {
        let clock = ManualClock::new();
        assert_eq!(clock.now(), Duration::ZERO);
        assert_eq!(
            clock.now(),
            Duration::ZERO,
            "reading the clock must not move it"
        );

        clock.advance(Duration::from_secs(30));
        assert_eq!(clock.now(), Duration::from_secs(30));
    }

    #[test]
    fn manual_clock_advances_are_cumulative_across_handles() {
        let clock = Arc::new(ManualClock::new());
        let other = Arc::clone(&clock);

        clock.advance(Duration::from_millis(400));
        other.advance(Duration::from_millis(600));

        assert_eq!(clock.now(), Duration::from_secs(1));
    }

    #[test]
    fn system_clock_never_goes_backwards() {
        let clock = SystemClock::new();
        let first = clock.now();
        let second = clock.now();
        assert!(second >= first);
    }
}
