//! The shutdown guard, and the thread that writes the metrics summary.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::metrics;

/// Keeps telemetry alive, and shuts it down when it is dropped.
///
/// Hold it for as long as the process should be reporting, usually by binding it in
/// `main`. Dropping it stops the metrics summary thread, writes one final summary, and
/// flushes and shuts down the OTLP pipelines in the order traces, metrics, logs.
///
/// Why the order matters: the batch exporters buffer, and a process that exits without a
/// flush loses whatever was still in the buffer. That is usually the part an operator
/// wanted, because a crash is what they were investigating.
#[derive(Debug)]
pub struct Guard {
    dump: Option<DumpThread>,
    #[cfg(feature = "otel")]
    pipelines: Option<crate::otel::Pipelines>,
}

impl Guard {
    /// A guard that owns nothing, returned by a second call to `init`.
    pub(crate) fn inert() -> Self {
        Self {
            dump: None,
            #[cfg(feature = "otel")]
            pipelines: None,
        }
    }

    /// A guard owning the metrics summary thread and, with the `otel` feature on, the
    /// three pipelines.
    pub(crate) fn new(
        dump: Option<DumpThread>,
        #[cfg(feature = "otel")] pipelines: Option<crate::otel::Pipelines>,
    ) -> Self {
        Self {
            dump,
            #[cfg(feature = "otel")]
            pipelines,
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(dump) = self.dump.take() {
            dump.stop();
        }

        #[cfg(feature = "otel")]
        if let Some(pipelines) = self.pipelines.take() {
            pipelines.shutdown();
        }
    }
}

/// The thread that writes the metrics summary on a fixed interval.
///
/// It is a plain thread rather than a task, so this crate needs no async runtime and a
/// binary can install telemetry before it starts one.
#[derive(Debug)]
pub(crate) struct DumpThread {
    stop: Arc<Stop>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Debug, Default)]
struct Stop {
    stopped: Mutex<bool>,
    changed: Condvar,
}

impl DumpThread {
    /// Start writing a summary every `interval`, or nothing at all when the interval is
    /// zero.
    pub(crate) fn spawn(interval: Duration) -> Option<Self> {
        if interval.is_zero() {
            return None;
        }

        let stop = Arc::new(Stop::default());
        let worker_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("adelie-telemetry-metrics".to_owned())
            .spawn(move || run(&worker_stop, interval))
            .ok()?;

        Some(Self {
            stop,
            handle: Some(handle),
        })
    }

    fn stop(mut self) {
        {
            let mut stopped = self
                .stop
                .stopped
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *stopped = true;
        }
        self.stop.changed.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run(stop: &Stop, interval: Duration) {
    loop {
        let mut stopped = stop
            .stopped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *stopped {
            break;
        }
        let (guard, _timeout) = stop
            .changed
            .wait_timeout(stopped, interval)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stopped = guard;
        let should_stop = *stopped;
        drop(stopped);

        if should_stop {
            break;
        }
        metrics::global().dump_if_due();
    }

    // One last summary, so the window that was open when the process shut down is not
    // simply lost. A restart is exactly when the numbers matter.
    metrics::global().dump_now();
}

/// Whether `init` has already run in this process.
///
/// Why an explicit flag as well as `try_init`: `try_init` stops a second call from
/// panicking, but it does not stop a second call from having already built a second set
/// of OTLP exporters and a second summary thread on the way there.
pub(crate) static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Claim the right to install telemetry. `true` means this call is the first.
pub(crate) fn claim_init() -> bool {
    !INITIALIZED.swap(true, Ordering::SeqCst)
}
