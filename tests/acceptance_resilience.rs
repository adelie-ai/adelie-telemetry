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

/// An `https` endpoint works, or is refused by name. Never anything in between.
///
/// TLS ships in the default features, so a normal build reaches an https collector. A
/// build that took `default-features = false` has no TLS backend, and must say so: the
/// alternative is reaching the socket and failing with "network error", which sends an
/// operator to debug DNS and firewalls for a problem that is neither.
#[test]
fn https_is_either_usable_or_refused_by_name() {
    let (stderr, success) = run_probe(&[(
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "https://collector.example.com:4318",
    )]);

    assert!(success, "the process must not die. stderr was: {stderr}");

    if cfg!(feature = "otel-tls") {
        assert!(
            !stderr.contains("no TLS backend"),
            "the default features include TLS, so an https endpoint must be accepted. \
             stderr was: {stderr}"
        );
    } else {
        assert!(
            stderr.contains("no TLS backend"),
            "without TLS the refusal must name the cause, not look like a network fault. \
             stderr was: {stderr}"
        );
        assert!(
            stderr.contains("default-features"),
            "the refusal must name what to change. stderr was: {stderr}"
        );
    }

    assert!(
        stderr.contains("a line that must still reach the console"),
        "console logging must survive either way. stderr was: {stderr}"
    );
}

/// A build without TLS still exports over plaintext.
///
/// Dropping the TLS backend must cost only `https`, not telemetry altogether.
#[test]
fn a_build_without_tls_still_exports_over_plaintext() {
    let (stderr, success) = run_probe(&[(
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "http://collector.example.com:4318",
    )]);

    assert!(success);
    assert!(
        !stderr.contains("telemetry export is off"),
        "a plaintext endpoint needs no TLS backend. stderr was: {stderr}"
    );
}

/// gRPC over TLS must actually speak TLS, not merely compile.
///
/// `tls-webpki-roots` supplies tonic with trust anchors and no crypto provider, so a build
/// carrying only that one refuses an `https` gRPC endpoint at exporter-build time and
/// exports nothing at all. A constant saying a TLS backend exists is not evidence that a
/// transport can use it, so this drives a real connection and looks at the bytes.
///
/// The handshake is not completed: the listener presents no certificate, and the webpki
/// roots would not trust it. Reaching a TLS `ClientHello` is the whole assertion, because
/// that is the step the missing provider prevented.
#[cfg(feature = "otel-tls")]
#[test]
fn grpc_over_tls_reaches_a_tls_handshake() {
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::mpsc;

    let listener = TcpListener::bind("127.0.0.1:0").expect("a local listener must bind");
    let port = listener.local_addr().expect("the port is known").port();

    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut first = [0u8; 8];
            let read = stream.read(&mut first).unwrap_or(0);
            let _ = sender.send(first[..read].to_vec());
        }
    });

    let endpoint = format!("https://127.0.0.1:{port}");
    let (stderr, success) = run_probe(&[
        ("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
        ("PROBE_RUNTIME", "tokio"),
    ]);

    assert!(success, "the probe must exit cleanly. stderr was: {stderr}");
    assert!(
        !stderr.contains("no TLS feature is enabled"),
        "the gRPC transport needs a TLS provider as well as trust anchors, or an https \
         endpoint is refused before a single connection is made. stderr was: {stderr}"
    );
    assert!(
        !stderr.contains("telemetry export is off"),
        "the pipeline must build. stderr was: {stderr}"
    );

    let bytes = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the exporter must open a connection to the endpoint");

    assert!(
        matches!(bytes.first(), Some(0x16)),
        "the first byte must be a TLS handshake record, found {bytes:?}"
    );
    assert_eq!(
        bytes.get(1),
        Some(&0x03),
        "a TLS ClientHello carries protocol version 3.x, found {bytes:?}"
    );
}
