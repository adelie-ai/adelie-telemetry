//! Fixed-bucket histograms.
//!
//! Why buckets rather than a count and a sum: a mean hides the tail. "The average turn
//! took 3 seconds" and "one turn in twenty took four minutes" are the same mean, and only
//! the second one is the report an operator receives. Buckets keep the shape.
//!
//! The boundaries are fixed and shared. The in-process dump and the OTLP export read the
//! same [`DURATION_BUCKETS_MS`], so the two paths never disagree about which bucket a
//! measurement fell in.

use std::time::Duration;

/// The bucket boundaries, in milliseconds, for every duration this crate records.
///
/// The range runs from a fast in-process call to a five-minute turn, because both ends
/// are real: a tool round can finish in a millisecond, and the user report that started
/// this work was a four-minute answer. A measurement above the last boundary lands in the
/// overflow bucket.
pub const DURATION_BUCKETS_MS: &[f64] = &[
    1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0,
    30_000.0, 60_000.0, 120_000.0, 300_000.0,
];

/// One bucket of a histogram.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Bucket {
    /// The inclusive upper bound of this bucket, in milliseconds. The overflow bucket
    /// reports [`f64::INFINITY`].
    pub upper_bound_ms: f64,
    /// How many measurements fell at or below the bound and above the one before it.
    pub count: u64,
}

/// A histogram as it stood at one moment.
#[derive(Clone, PartialEq, Debug)]
pub struct HistogramSnapshot {
    /// How many measurements the histogram holds.
    pub count: u64,
    /// The sum of every measurement, in milliseconds.
    pub sum_ms: f64,
    /// Every bucket, in ascending bound order, ending with the overflow bucket.
    pub buckets: Vec<Bucket>,
}

impl HistogramSnapshot {
    /// The bucket boundaries this snapshot was built with, overflow bucket included.
    pub fn bounds(&self) -> Vec<f64> {
        todo!()
    }

    /// The upper bound of the bucket the given quantile falls in, in milliseconds.
    ///
    /// A bucketed histogram cannot give an exact quantile, only the bucket that contains
    /// it. Reporting the bound is honest about that. Returns `None` when nothing has been
    /// recorded.
    pub fn quantile_ms(&self, quantile: f64) -> Option<f64> {
        todo!()
    }

    /// The arithmetic mean, in milliseconds, or `None` when nothing has been recorded.
    pub fn mean_ms(&self) -> Option<f64> {
        todo!()
    }
}

/// A histogram with fixed bucket boundaries.
#[derive(Clone, Debug)]
pub(crate) struct Histogram {
    bounds: &'static [f64],
    counts: Vec<u64>,
    count: u64,
    sum_ms: f64,
}

impl Histogram {
    /// An empty histogram over the given boundaries.
    pub(crate) fn new(bounds: &'static [f64]) -> Self {
        todo!()
    }

    /// Add one measurement.
    pub(crate) fn record(&mut self, value: Duration) {
        todo!()
    }

    /// This histogram as a snapshot, leaving it unchanged.
    pub(crate) fn snapshot(&self) -> HistogramSnapshot {
        todo!()
    }

    /// Forget every measurement, keeping the boundaries.
    pub(crate) fn reset(&mut self) {
        todo!()
    }
}
