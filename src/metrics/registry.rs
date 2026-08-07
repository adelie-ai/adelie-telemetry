//! The in-process metrics registry.
//!
//! Why this exists at all: with the `otel` feature off there is no collector and no
//! exporter, and a facade that no-opped would make metrics the one signal that simply
//! vanishes without a backend. Logs still print and spans still decorate the lines, so
//! metrics accumulate here and get written out periodically. A desktop install from
//! `cargo install` runs a default-feature build, and this is the only way it ever
//! reports a number.
//!
//! The dump stays on when a backend *is* configured. The two paths report the same
//! numbers over the same bucket boundaries, so the local dump is a cross-check rather
//! than a substitute.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::clock::Clock;
use crate::metrics::histogram::{DURATION_BUCKETS_MS, Histogram, HistogramSnapshot};

/// How many distinct label sets one metric may have before the rest are folded together.
///
/// Why a cap: a label taken from a model name, a tool name or a provider is attacker- or
/// config-controlled in practice, and an unbounded label set is an unbounded memory leak
/// in a process that runs for weeks.
pub const DEFAULT_CARDINALITY_CAP: usize = 64;

/// How often the registry writes a summary when nothing says otherwise.
pub const DEFAULT_DUMP_INTERVAL: Duration = Duration::from_secs(600);

/// The label value that everything past the cardinality cap is folded into.
pub const OVERFLOW_LABEL_VALUE: &str = "other";

/// The label key that carries [`OVERFLOW_LABEL_VALUE`].
pub const OVERFLOW_LABEL_KEY: &str = "cardinality";

/// The most bytes a label value keeps.
///
/// The cardinality cap bounds how many series exist; this bounds what each one retains.
/// Without it a metric with 64 values of a megabyte each holds 64 megabytes for the life
/// of the process.
pub const MAX_LABEL_VALUE_BYTES: usize = 128;

/// What replaces a control character in a label value.
const REPLACEMENT: char = '\u{fffd}';

/// One dimension of a metric.
///
/// Keys are `'static` because a metric's dimensions are a fixed vocabulary chosen at the
/// call site. Values are owned because they come from data.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Label {
    key: &'static str,
    value: String,
}

impl Label {
    /// A label with this key and value.
    pub fn new(key: &'static str, value: impl Into<String>) -> Self {
        Self {
            key,
            value: sanitize(value.into()),
        }
    }

    /// This label's key.
    pub fn key(&self) -> &'static str {
        self.key
    }

    /// This label's value.
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// What the registry is allowed to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Settings {
    /// How long between summaries. [`Duration::ZERO`] turns the summary off entirely.
    pub dump_interval: Duration,
    /// How many distinct label sets one metric may have.
    pub cardinality_cap: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dump_interval: DEFAULT_DUMP_INTERVAL,
            cardinality_cap: DEFAULT_CARDINALITY_CAP,
        }
    }
}

/// One counter, as of a dump.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CounterSummary {
    /// The metric name the call site used.
    pub name: &'static str,
    /// The label set, sorted by key.
    pub labels: Vec<Label>,
    /// How much the counter rose during the window that just closed.
    pub window_delta: u64,
    /// The counter's value over the whole life of the process.
    pub total: u64,
}

/// One duration histogram, as of a dump.
#[derive(Clone, PartialEq, Debug)]
pub struct HistogramSummary {
    /// The metric name the call site used.
    pub name: &'static str,
    /// The label set, sorted by key.
    pub labels: Vec<Label>,
    /// Only the measurements taken during the window that just closed.
    pub window: HistogramSnapshot,
    /// Every measurement taken over the whole life of the process.
    pub total: HistogramSnapshot,
}

/// Everything the registry holds at one moment.
///
/// Why both a window and a total: on a pod that has run for a month, a cumulative number
/// is dominated by history and stops moving, so a fault that started an hour ago is
/// invisible in it. The window shows what is happening now, the total shows what the
/// process has done.
#[derive(Clone, PartialEq, Debug)]
pub struct Summary {
    /// How long the window that just closed lasted.
    pub window: Duration,
    /// How long the registry has been collecting.
    pub uptime: Duration,
    /// Every counter, sorted by name and then by label set.
    pub counters: Vec<CounterSummary>,
    /// Every duration histogram, sorted by name and then by label set.
    pub histograms: Vec<HistogramSummary>,
}

impl Summary {
    /// Whether anything at all has been recorded.
    pub fn is_empty(&self) -> bool {
        self.counters.is_empty() && self.histograms.is_empty()
    }
}

/// Counters and duration histograms, accumulated in process.
#[derive(Debug)]
pub struct Registry {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    settings: Settings,
    clock: Arc<dyn Clock>,
    started_at: Duration,
    last_dump_at: Duration,
    counters: HashMap<SeriesKey, CounterSeries>,
    histograms: HashMap<SeriesKey, HistogramSeries>,
    /// How many distinct label sets each metric name has, so the cap is per metric.
    label_sets: HashMap<&'static str, usize>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct SeriesKey {
    name: &'static str,
    labels: Vec<Label>,
}

#[derive(Clone, Debug)]
struct CounterSeries {
    total: u64,
    window: u64,
}

#[derive(Clone, Debug)]
struct HistogramSeries {
    total: Histogram,
    window: Histogram,
}

impl Registry {
    /// An empty registry.
    pub fn new(settings: Settings, clock: Arc<dyn Clock>) -> Self {
        let started_at = clock.now();
        Self {
            inner: Mutex::new(Inner {
                settings,
                clock,
                started_at,
                last_dump_at: started_at,
                counters: HashMap::new(),
                histograms: HashMap::new(),
                label_sets: HashMap::new(),
            }),
        }
    }

    /// Change the settings and the clock without losing what has been recorded.
    ///
    /// Why not replace the registry: a call site may record before the binary calls
    /// `init`, and throwing those measurements away would make the first window wrong.
    pub fn reconfigure(&self, settings: Settings, clock: Arc<dyn Clock>) {
        let mut inner = self.lock();
        // The new clock has its own origin, so the window and the uptime restart from it.
        // Keeping the old readings would mix two unrelated timelines.
        let now = clock.now();
        inner.settings = settings;
        inner.clock = clock;
        inner.started_at = now;
        inner.last_dump_at = now;

        // The window clock restarts here, so the window counts restart with it. Leaving
        // them would report measurements taken before `init` as though they happened in
        // the first window, which is the one an operator reads first. The running totals
        // keep them.
        for series in inner.counters.values_mut() {
            series.window = 0;
        }
        for series in inner.histograms.values_mut() {
            series.window.reset();
        }
    }

    /// The settings in force.
    pub fn settings(&self) -> Settings {
        self.lock().settings
    }

    /// Add to a counter.
    pub fn add(&self, name: &'static str, value: u64, labels: &[Label]) {
        let resolved = {
            let mut inner = self.lock();
            let key = inner.key_for(name, labels);
            let series = inner.counters.entry(key.clone()).or_insert(CounterSeries {
                total: 0,
                window: 0,
            });
            series.total = series.total.saturating_add(value);
            series.window = series.window.saturating_add(value);
            key
        };

        // The lock is released first. The OTLP path is another process's problem once the
        // measurement is buffered, but it is not this lock's problem at all.
        #[cfg(feature = "otel")]
        crate::metrics::otel_bridge::add(resolved.name, value, &resolved.labels);
        #[cfg(not(feature = "otel"))]
        let _ = resolved;
    }

    /// Add one to a counter.
    pub fn increment(&self, name: &'static str, labels: &[Label]) {
        self.add(name, 1, labels);
    }

    /// Record one duration measurement.
    pub fn record_duration(&self, name: &'static str, value: Duration, labels: &[Label]) {
        let resolved = {
            let mut inner = self.lock();
            let key = inner.key_for(name, labels);
            let series = inner
                .histograms
                .entry(key.clone())
                .or_insert_with(|| HistogramSeries {
                    total: Histogram::new(DURATION_BUCKETS_MS),
                    window: Histogram::new(DURATION_BUCKETS_MS),
                });
            series.total.record(value);
            series.window.record(value);
            key
        };

        #[cfg(feature = "otel")]
        crate::metrics::otel_bridge::record_duration_ms(
            resolved.name,
            value.as_secs_f64() * 1_000.0,
            &resolved.labels,
        );
        #[cfg(not(feature = "otel"))]
        let _ = resolved;
    }

    /// Everything recorded so far, leaving the window open.
    pub fn snapshot(&self) -> Summary {
        self.lock().summarize()
    }

    /// How many distinct series the registry holds, counters and histograms together.
    pub fn series_count(&self) -> usize {
        let inner = self.lock();
        inner.counters.len() + inner.histograms.len()
    }

    /// A summary if one is due, closing the window and starting a new one.
    ///
    /// Returns `None` when the dump interval is [`Duration::ZERO`], or when not enough
    /// time has passed. The summary is written to the log as well as returned.
    pub fn dump_if_due(&self) -> Option<Summary> {
        let summary = {
            let mut inner = self.lock();
            if inner.settings.dump_interval.is_zero() {
                return None;
            }
            if inner.clock.now().saturating_sub(inner.last_dump_at) < inner.settings.dump_interval {
                return None;
            }
            inner.close_window()
        };
        emit(&summary);
        Some(summary)
    }

    /// A summary now, whatever the interval says, closing the window.
    ///
    /// The guard calls this on the way out so the window that was open at shutdown is not
    /// lost. A restart is exactly when those numbers matter.
    pub fn dump_now(&self) -> Summary {
        let summary = self.lock().close_window();
        if !summary.is_empty() {
            emit(&summary);
        }
        summary
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while holding this lock leaves the counters readable and only slightly
        // wrong. Losing every metric for the life of the process would be worse.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Write a summary to the log.
///
/// Every field is a name, a label, a count or a duration. No measurement carries content,
/// so this stays at INFO.
pub(crate) fn emit(summary: &Summary) {
    if summary.is_empty() {
        return;
    }

    tracing::info!(
        window_seconds = summary.window.as_secs(),
        uptime_seconds = summary.uptime.as_secs(),
        counters = summary.counters.len(),
        histograms = summary.histograms.len(),
        "metrics summary"
    );

    for counter in &summary.counters {
        tracing::info!(
            metric = counter.name,
            labels = %render_labels(&counter.labels),
            window = counter.window_delta,
            total = counter.total,
            "counter"
        );
    }

    for histogram in &summary.histograms {
        tracing::info!(
            metric = histogram.name,
            labels = %render_labels(&histogram.labels),
            window_count = histogram.window.count,
            window_p50_ms = histogram.window.quantile_ms(0.50),
            window_p95_ms = histogram.window.quantile_ms(0.95),
            total_count = histogram.total.count,
            total_p95_ms = histogram.total.quantile_ms(0.95),
            "duration"
        );
    }
}

/// A label value that is safe to print and bounded in size.
///
/// A label value is not ours to trust. A remote MCP server names its own tools, and a
/// model name can come from a config file somebody else wrote. The value reaches the
/// console inside a log field, so a newline in it would produce what reads as a second
/// genuine log line, with a real timestamp column, level and target. An ANSI escape would
/// survive too: `with_ansi(false)` turns off the formatter's own colour, not escapes
/// embedded in a field value.
///
/// Control characters are replaced rather than dropped, so the value still shows that
/// something was there.
pub(crate) fn sanitize(value: String) -> String {
    let mut cleaned: String = value
        .chars()
        .map(|character| {
            if is_deceptive(character) {
                REPLACEMENT
            } else {
                character
            }
        })
        .collect();

    if cleaned.len() > MAX_LABEL_VALUE_BYTES {
        // Truncate on a character boundary. Cutting mid-character would panic.
        let mut end = MAX_LABEL_VALUE_BYTES;
        while end > 0 && !cleaned.is_char_boundary(end) {
            end -= 1;
        }
        cleaned.truncate(end);
    }
    cleaned
}

/// Whether this character could change what a person reads, rather than what was written.
///
/// Three groups, and they fail in different ways:
///
/// - `char::is_control` covers category Cc: C0, C1 and DEL. A newline ends the log line
///   early and starts one that reads as genuine; an escape drives the terminal.
/// - U+2028 LINE SEPARATOR and U+2029 PARAGRAPH SEPARATOR are categories Zl and Zp.
///   `is_control` does not cover them, and some log viewers and every JSON consumer treat
///   them as a line break.
/// - The bidi controls are category Cf. They leave the bytes honest and the line
///   structure intact, and reverse what a terminal shows: a tool named with U+202E
///   renders with everything after it backwards, so the name an operator reads in
///   `kubectl logs` is not the name that was called. Deception rather than forgery, and
///   the Trojan-source class.
///
/// # Why a list of bidi controls and not all of Cf
///
/// Cf also holds U+200D ZERO WIDTH JOINER, which carries the emoji sequences a person
/// legitimately wants to read. Hiding text is a weaker problem than reversing it, so the
/// set stops at the characters that change reading order. It matches what `mcp-core`
/// strips and what rustc's `text_direction_codepoint_in_literal` lint covers, so one
/// answer holds across the fleet.
fn is_deceptive(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{2028}'      // LINE SEPARATOR
            | '\u{2029}'    // PARAGRAPH SEPARATOR
            | '\u{061c}'    // ARABIC LETTER MARK
            | '\u{200e}'    // LEFT-TO-RIGHT MARK
            | '\u{200f}'    // RIGHT-TO-LEFT MARK
            | '\u{202a}'..='\u{202e}'  // the embeddings, the pop, and the overrides
            | '\u{2066}'..='\u{2069}'  // the isolates and the pop
        )
}

/// Labels as one `key=value,key=value` string.
fn render_labels(labels: &[Label]) -> String {
    labels
        .iter()
        .map(|label| format!("{}={}", label.key, label.value))
        .collect::<Vec<_>>()
        .join(",")
}

impl Inner {
    /// The series key for this measurement, folding into the overflow series once the
    /// metric has as many label sets as the cap allows.
    fn key_for(&mut self, name: &'static str, labels: &[Label]) -> SeriesKey {
        let mut sorted = labels.to_vec();
        // Sorted, so the order the call site wrote the labels in cannot split one series
        // into two.
        sorted.sort();
        sorted.dedup();

        let candidate = SeriesKey {
            name,
            labels: sorted,
        };

        // Checked against both maps, not just this instrument's. The budget is per metric
        // name, so a label set used as a counter and as a histogram is one label set and
        // must cost one slot.
        let known =
            self.counters.contains_key(&candidate) || self.histograms.contains_key(&candidate);
        if known {
            return candidate;
        }

        let seen = self.label_sets.entry(name).or_insert(0);
        if *seen >= self.settings.cardinality_cap {
            return SeriesKey {
                name,
                labels: vec![Label::new(OVERFLOW_LABEL_KEY, OVERFLOW_LABEL_VALUE)],
            };
        }
        *seen += 1;
        candidate
    }

    /// Close the current window and start a new one.
    fn close_window(&mut self) -> Summary {
        let summary = self.summarize();
        for series in self.counters.values_mut() {
            series.window = 0;
        }
        for series in self.histograms.values_mut() {
            series.window.reset();
        }
        self.last_dump_at = self.clock.now();
        summary
    }

    fn summarize(&self) -> Summary {
        let now = self.clock.now();

        let mut counters: Vec<CounterSummary> = self
            .counters
            .iter()
            .map(|(key, series)| CounterSummary {
                name: key.name,
                labels: key.labels.clone(),
                window_delta: series.window,
                total: series.total,
            })
            .collect();
        counters.sort_by(|left, right| {
            left.name
                .cmp(right.name)
                .then_with(|| left.labels.cmp(&right.labels))
        });

        let mut histograms: Vec<HistogramSummary> = self
            .histograms
            .iter()
            .map(|(key, series)| HistogramSummary {
                name: key.name,
                labels: key.labels.clone(),
                window: series.window.snapshot(),
                total: series.total.snapshot(),
            })
            .collect();
        histograms.sort_by(|left, right| {
            left.name
                .cmp(right.name)
                .then_with(|| left.labels.cmp(&right.labels))
        });

        Summary {
            window: now.saturating_sub(self.last_dump_at),
            uptime: now.saturating_sub(self.started_at),
            counters,
            histograms,
        }
    }
}
