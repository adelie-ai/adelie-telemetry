//! Console acceptance criteria.
//!
//! The stdout check runs a separate process on purpose. A layer configured with a capture
//! writer proves what the layer was told to do; only a real process proves what actually
//! reached file descriptor 1. The MCP stdio transport frames JSON-RPC on that descriptor,
//! so the difference is the whole point.

use std::path::PathBuf;
use std::process::Command;

/// Where `cargo test` leaves the example binaries.
///
/// The test binary itself sits in `target/<profile>/deps/`, so the examples are one
/// directory across. `cargo test` builds examples, so it is always there by the time this
/// runs.
fn probe_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("a test binary knows its own path");
    path.pop(); // the test binary's file name
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("examples");
    path.push("stdout_probe");
    path
}

/// Nothing reaches stdout at any level.
#[test]
fn console_layer_writes_to_stderr_only() {
    let probe = probe_binary();
    assert!(
        probe.is_file(),
        "the stdout probe example must be built before this test can prove anything; \
         expected it at {}",
        probe.display()
    );

    let output = Command::new(&probe)
        .env("RUST_LOG", "trace")
        .output()
        .expect("the probe must run");

    assert!(
        output.status.success(),
        "the probe must exit cleanly, otherwise an empty stdout proves nothing"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        stdout.trim(),
        "STDOUT-MARKER",
        "only the probe's own marker may reach stdout; a log line there corrupts the \
         JSON-RPC stream of an MCP server. stdout was: {stdout:?}"
    );

    for level in ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"] {
        assert!(
            stderr.contains(level),
            "{level} must reach stderr, or the console layer is not doing its job. \
             stderr was: {stderr:?}"
        );
    }
}

/// With the knob on, a closing span emits a line with its elapsed time.
#[test]
fn span_close_events_carry_duration() {
    let probe = probe_binary();
    assert!(probe.is_file(), "the stdout probe example must be built");

    let output = Command::new(&probe)
        .env("RUST_LOG", "trace")
        .output()
        .expect("the probe must run");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stderr.contains("probe_span"),
        "the span must appear on the console. stderr was: {stderr:?}"
    );
    assert!(
        stderr.contains("close"),
        "a closing span must write a line, which is what makes turn timing visible to \
         somebody reading the log of a running container. stderr was: {stderr:?}"
    );
    assert!(
        stderr.contains("time.busy"),
        "the closing line must carry how long the span was open. stderr was: {stderr:?}"
    );
}
