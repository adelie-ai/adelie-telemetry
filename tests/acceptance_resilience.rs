//! What must keep working when the OTLP side cannot be built.
//!
//! Every test here runs a child process, because `init` installs a process-global
//! subscriber and the question being asked is what a whole process ends up with.

#![cfg(feature = "otel")]

use std::path::PathBuf;
use std::process::Command;

fn probe_binary(name: &str) -> PathBuf {
    let mut path = std::env::current_exe().expect("a test binary knows its own path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("examples");
    path.push(name);
    path
}

fn run_probe(env: &[(&str, &str)]) -> (String, bool) {
    let probe = probe_binary("init_failure_probe");
    assert!(
        probe.is_file(),
        "the probe example must be built; expected it at {}",
        probe.display()
    );

    let mut command = Command::new(&probe);
    command.env("RUST_LOG", "info");
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().expect("the probe must run");

    (
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    )
}

/// The environment that makes an exporter fail to build.
///
/// An unsupported compression algorithm is rejected by every transport, so it does not
/// depend on which one a build resolves to. A malformed endpoint reaches the same path,
/// but only on gRPC: the HTTP exporter accepts any string that parses as a URI, so a
/// wrong scheme fails later, at export time, rather than here.
const FAILING_PIPELINE: &[(&str, &str)] = &[
    (
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "http://collector.example.com:4318",
    ),
    ("OTEL_EXPORTER_OTLP_COMPRESSION", "brotli"),
];

/// A pipeline that cannot be built must not cost the process its console logging.
///
/// One wrong value in a deployment overlay is enough to reach this. Losing the console as
/// well would leave `kubectl logs` empty, which is the worst possible moment to have no
/// logs, and there is no way back: a second `init` call returns an inert guard.
#[test]
fn a_failed_otlp_pipeline_leaves_console_logging_intact() {
    let (stderr, success) = run_probe(FAILING_PIPELINE);

    assert!(success, "a broken endpoint must not stop the process");
    assert!(
        stderr.contains("a line that must still reach the console"),
        "the console layer must be installed even when the OTLP pipelines fail. \
         stderr was: {stderr}"
    );
}

/// The failure must be reported, and it must name the variable that caused it.
#[test]
fn a_failed_otlp_pipeline_is_reported_by_name() {
    let (stderr, _) = run_probe(FAILING_PIPELINE);

    assert!(
        stderr.contains("telemetry export is off"),
        "the failure must be reported, not swallowed. stderr was: {stderr}"
    );
    assert!(
        stderr.contains("OTEL_EXPORTER_OTLP_COMPRESSION"),
        "the report must name the variable an operator has to fix. stderr was: {stderr}"
    );
}

/// A credential in the environment must not reach the log with the failure report.
#[test]
fn a_failure_report_withholds_header_values() {
    let mut env = FAILING_PIPELINE.to_vec();
    env.push(("OTEL_EXPORTER_OTLP_HEADERS", "api-key=super-secret-value"));
    let (stderr, _) = run_probe(&env);

    assert!(
        !stderr.contains("super-secret-value"),
        "OTEL_EXPORTER_OTLP_HEADERS routinely carries an API key. stderr was: {stderr}"
    );
    assert!(stderr.contains("OTEL_EXPORTER_OTLP_HEADERS=<set>"));
}

/// The metrics summary must survive a failed pipeline too.
///
/// The registry is supposed to work with no collector at all, so a collector that could
/// not be built must leave it exactly as it would have been.
#[test]
fn a_failed_otlp_pipeline_leaves_the_metrics_summary_running() {
    let (stderr, _) = run_probe(FAILING_PIPELINE);

    assert!(
        stderr.contains("metrics summary"),
        "the in-process summary must still be written. stderr was: {stderr}"
    );
    assert!(stderr.contains("probe.requests"));
}

/// Asking for gRPC outside a Tokio runtime must be reported, not panicked.
///
/// The transport calls into a reactor. Without one it panics from inside `hyper-util`,
/// naming neither this crate nor the variable that caused it, and a GUI binary has no
/// console to show it on.
#[test]
fn grpc_without_a_runtime_is_reported_not_panicked() {
    let (stderr, success) = run_probe(&[
        ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4317"),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
    ]);

    assert!(success, "the process must not die. stderr was: {stderr}");
    assert!(
        !stderr.contains("no reactor running"),
        "the hyper panic must be pre-empted. stderr was: {stderr}"
    );
    assert!(
        stderr.contains("OTEL_EXPORTER_OTLP_PROTOCOL"),
        "the report must name the variable to change. stderr was: {stderr}"
    );
    assert!(
        stderr.contains("a line that must still reach the console"),
        "console logging must survive this too. stderr was: {stderr}"
    );
}

/// The same request inside a runtime must build the pipeline rather than refuse it.
#[test]
fn grpc_inside_a_runtime_is_accepted() {
    let (stderr, success) = run_probe(&[
        ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4317"),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
        ("PROBE_RUNTIME", "tokio"),
    ]);

    assert!(success);
    assert!(
        !stderr.contains("OTEL_EXPORTER_OTLP_PROTOCOL"),
        "inside a runtime there is nothing to refuse. stderr was: {stderr}"
    );
}

/// An `https` endpoint must be usable, not refused.
///
/// The refusal path, for a build with no TLS backend, is covered by the unit tests in
/// `src/otel/preflight.rs`. This one holds the promise that the shipped build never takes
/// it.
#[test]
fn https_endpoints_are_accepted_by_the_shipped_build() {
    let (stderr, success) = run_probe(&[(
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "https://collector.example.com:4318",
    )]);

    assert!(success, "the process must not die. stderr was: {stderr}");
    assert!(
        !stderr.contains("no TLS backend"),
        "the otel feature compiles a TLS backend, so an https endpoint must be accepted \
         rather than refused. stderr was: {stderr}"
    );
    assert!(
        stderr.contains("a line that must still reach the console"),
        "console logging must survive an unreachable collector. stderr was: {stderr}"
    );
}
