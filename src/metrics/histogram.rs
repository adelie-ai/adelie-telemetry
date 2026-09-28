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
    1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0, 30_000.0,
    60_000.0, 120_000.0, 300_000.0,
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
        self.buckets
            .iter()
            .map(|bucket| bucket.upper_bound_ms)
            .collect()
    }

    /// The upper bound of the bucket the given quantile falls in, in milliseconds.
    ///
    /// A bucketed histogram cannot give an exact quantile, only the bucket that contains
    /// it. Reporting the bound is honest about that. Returns `None` when nothing has been
    /// recorded.
    pub fn quantile_ms(&self, quantile: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let quantile = quantile.clamp(0.0, 1.0);
        // The rank of the measurement we are looking for, counting from one.
        let target = (quantile * self.count as f64).ceil().max(1.0) as u64;

        let mut seen = 0;
        for bucket in &self.buckets {
            seen += bucket.count;
            if seen >= target {
                return Some(bucket.upper_bound_ms);
            }
        }
        self.buckets.last().map(|bucket| bucket.upper_bound_ms)
    }

    /// The arithmetic mean, in milliseconds, or `None` when nothing has been recorded.
    pub fn mean_ms(&self) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        Some(self.sum_ms / self.count as f64)
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
        Self {
            bounds,
            // One slot per boundary, plus the overflow bucket past the last one.
            counts: vec![0; bounds.len() + 1],
            count: 0,
            sum_ms: 0.0,
        }
    }

    /// Add one measurement.
    pub(crate) fn record(&mut self, value: Duration) {
        let millis = value.as_secs_f64() * 1_000.0;
        let index = self
            .bounds
            .iter()
            .position(|bound| millis <= *bound)
            .unwrap_or(self.bounds.len());
        self.counts[index] += 1;
        self.count += 1;
        self.sum_ms += millis;
    }

    /// This histogram as a snapshot, leaving it unchanged.
    pub(crate) fn snapshot(&self) -> HistogramSnapshot {
        let buckets = self
            .counts
            .iter()
            .enumerate()
            .map(|(index, count)| Bucket {
                upper_bound_ms: self.bounds.get(index).copied().unwrap_or(f64::INFINITY),
                count: *count,
            })
            .collect();

        HistogramSnapshot {
            count: self.count,
            sum_ms: self.sum_ms,
            buckets,
        }
    }

    /// Forget every measurement, keeping the boundaries.
    pub(crate) fn reset(&mut self) {
        self.counts.iter_mut().for_each(|count| *count = 0);
        self.count = 0;
        self.sum_ms = 0.0;
    }
}

// ---------------------------------------------------------------------------
// A fixed-bucket histogram generic over unit and boundaries, for a
// measurement that is not a duration - a per-request token count, for
// example. [`Histogram`] above stays duration-only, unchanged in name and
// shape: several consumer repos already depend on it, `HistogramSnapshot`
// and `quantile_ms` included. This is the same bucketing algorithm, kept as
// a separate type rather than a generalization of that one, so nothing that
// builds only against the duration path sees any change at all.
// ---------------------------------------------------------------------------

/// One bucket of a [`ValueHistogram`].
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ValueBucket {
    /// The inclusive upper bound of this bucket. The overflow bucket reports
    /// [`f64::INFINITY`].
    pub upper_bound: f64,
    /// How many measurements fell at or below the bound and above the one before it.
    pub count: u64,
}

/// A [`ValueHistogram`] as it stood at one moment.
#[derive(Clone, PartialEq, Debug)]
pub struct ValueHistogramSnapshot {
    /// How many measurements the histogram holds.
    pub count: u64,
    /// The sum of every measurement, in the metric's own unit.
    pub sum: f64,
    /// Every bucket, in ascending bound order, ending with the overflow bucket.
    pub buckets: Vec<ValueBucket>,
}

impl ValueHistogramSnapshot {
    /// The bucket boundaries this snapshot was built with, overflow bucket included.
    pub fn bounds(&self) -> Vec<f64> {
        self.buckets
            .iter()
            .map(|bucket| bucket.upper_bound)
            .collect()
    }

    /// The upper bound of the bucket the given quantile falls in.
    ///
    /// A bucketed histogram cannot give an exact quantile, only the bucket that contains
    /// it. Reporting the bound is honest about that. Returns `None` when nothing has been
    /// recorded.
    pub fn quantile(&self, quantile: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let quantile = quantile.clamp(0.0, 1.0);
        let target = (quantile * self.count as f64).ceil().max(1.0) as u64;

        let mut seen = 0;
        for bucket in &self.buckets {
            seen += bucket.count;
            if seen >= target {
                return Some(bucket.upper_bound);
            }
        }
        self.buckets.last().map(|bucket| bucket.upper_bound)
    }

    /// The arithmetic mean, or `None` when nothing has been recorded.
    pub fn mean(&self) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        Some(self.sum / self.count as f64)
    }
}

/// A histogram with fixed bucket boundaries, over a value in the caller's own unit.
#[derive(Clone, Debug)]
pub(crate) struct ValueHistogram {
    bounds: &'static [f64],
    counts: Vec<u64>,
    count: u64,
    sum: f64,
}

impl ValueHistogram {
    /// An empty histogram over the given boundaries.
    pub(crate) fn new(bounds: &'static [f64]) -> Self {
        Self {
            bounds,
            counts: vec![0; bounds.len() + 1],
            count: 0,
            sum: 0.0,
        }
    }

    /// Add one measurement.
    pub(crate) fn record(&mut self, value: f64) {
        let index = self
            .bounds
            .iter()
            .position(|bound| value <= *bound)
            .unwrap_or(self.bounds.len());
        self.counts[index] += 1;
        self.count += 1;
        self.sum += value;
    }

    /// This histogram as a snapshot, leaving it unchanged.
    pub(crate) fn snapshot(&self) -> ValueHistogramSnapshot {
        let buckets = self
            .counts
            .iter()
            .enumerate()
            .map(|(index, count)| ValueBucket {
                upper_bound: self.bounds.get(index).copied().unwrap_or(f64::INFINITY),
                count: *count,
            })
            .collect();

        ValueHistogramSnapshot {
            count: self.count,
            sum: self.sum,
            buckets,
        }
    }

    /// Forget every measurement, keeping the boundaries.
    pub(crate) fn reset(&mut self) {
        self.counts.iter_mut().for_each(|count| *count = 0);
        self.count = 0;
        self.sum = 0.0;
    }
}
