//! What the environment is allowed to change about export, and what it is not.
//!
//! Every test here runs a child process. `init` installs a process-global subscriber and
//! reads the process environment, so the question being asked - what a whole process ends
//! up doing - can only be answered by a whole process. Setting a variable in this process
//! instead would need `unsafe`, and would race with every test running beside it.
//!
//! Each signal is given an endpoint of its own, pointing at a local listener that records
//! the connection and closes it. That is what makes "the pipeline is absent" an assertion
//! about behaviour rather than about a log line: a signal that was switched off opens no
//! connection at all.

#![cfg(feature = "otel")]

use std::io::Read;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

/// How long to wait for a connection that should arrive, and to be sure one that should
/// not has not.
///
/// The probe has already exited by the time this is read, so an export that was going to
/// happen has happened. The wait covers the accept queue, not the export.
const SETTLE: Duration = Duration::from_secs(2);

/// Every variable that could reach the child from the developer's own shell.
///
/// Removed on the child rather than trusted to be unset, so a run on a workstation with a
/// collector configured gives the same answer as a run without one.
const INHERITED: &[&str] = &[
    "OTEL_SDK_DISABLED",
    "OTEL_TRACES_EXPORTER",
    "OTEL_METRICS_EXPORTER",
    "OTEL_LOGS_EXPORTER",
    "OTEL_SERVICE_NAME",
    "OTEL_RESOURCE_ATTRIBUTES",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_PROTOCOL",
    "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
    "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
    "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_EXPORTER_OTLP_COMPRESSION",
    "OTEL_EXPORTER_OTLP_TIMEOUT",
    "ADELIE_TELEMETRY_SHUTDOWN_BUDGET_MS",
    "ADELIE_TELEMETRY_METRICS_SUMMARY_INTERVAL_MS",
    "PROBE_SHUTDOWN_BUDGET_MS",
    "PROBE_SUMMARY_INTERVAL_MS",
    "PROBE_RUNTIME",
];

fn probe_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("a test binary knows its own path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("examples");
    path.push("signals_probe");
    path
}

/// A local endpoint that records every connection made to it.
///
/// It answers nothing. The exporter's request fails, which costs the test nothing: the
/// assertion is that a connection was opened, and refusing it early keeps the probe's
/// shutdown short.
struct Endpoint {
    port: u16,
    connections: Receiver<()>,
}

impl Endpoint {
    fn open() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a local listener must bind");
        let port = listener.local_addr().expect("the port is known").port();
        let (sender, connections) = channel();

        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let mut discard = [0u8; 64];
                let _ = stream.read(&mut discard);
                if sender.send(()).is_err() {
                    break;
                }
            }
        });

        Self { port, connections }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Whether the exporter for this signal opened a connection.
    fn was_used(&self) -> bool {
        self.connections.recv_timeout(SETTLE).is_ok()
    }
}

/// What one probe run produced.
struct Run {
    stderr: String,
    success: bool,
    traces: bool,
    metrics: bool,
    logs: bool,
}

impl Run {
    /// The value of a field on a log line, with any surrounding quotes removed.
    ///
    /// The formatter decides whether to quote, and that is not what any test here is
    /// about, so the assertions read the value rather than a rendering of it.
    fn field(&self, key: &str) -> Option<String> {
        let needle = format!("{key}=");
        let rest = &self.stderr[self.stderr.find(&needle)? + needle.len()..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        Some(rest[..end].trim_matches('"').to_owned())
    }
}

/// Run the probe with these variables set, and report which signals exported.
fn run_probe(env: &[(&str, &str)]) -> Run {
    let probe = probe_binary();
    assert!(
        probe.is_file(),
        "the signals probe example must be built before this test can prove anything; \
         expected it at {}",
        probe.display()
    );

    let traces = Endpoint::open();
    let metrics = Endpoint::open();
    let logs = Endpoint::open();

    let mut command = Command::new(&probe);
    for name in INHERITED {
        command.env_remove(name);
    }
    command
        .env("RUST_LOG", "info")
        .env("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf")
        .env(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            traces.url("/v1/traces"),
        )
        .env(
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
            metrics.url("/v1/metrics"),
        )
        .env("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", logs.url("/v1/logs"));
    for (key, value) in env {
        command.env(key, value);
    }

    let output = command.output().expect("the probe must run");

    Run {
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        success: output.status.success(),
        traces: traces.was_used(),
        metrics: metrics.was_used(),
        logs: logs.was_used(),
    }
}

/// Run the probe with no endpoint variable at all, so nothing is listening for it.
fn run_probe_without_endpoints(env: &[(&str, &str)]) -> Run {
    let probe = probe_binary();
    assert!(probe.is_file(), "the signals probe example must be built");

    let mut command = Command::new(&probe);
    for name in INHERITED {
        command.env_remove(name);
    }
    command.env("RUST_LOG", "info");
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().expect("the probe must run");

    Run {
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        success: output.status.success(),
        traces: false,
        metrics: false,
        logs: false,
    }
}

// ---------------------------------------------------------------------------------
// Which value names the service
// ---------------------------------------------------------------------------------

/// The name the binary passed to `Config::new` is what a backend sees when the
/// environment says nothing.
#[test]
fn the_configured_service_name_applies_when_no_variable_is_set() {
    let run = run_probe(&[]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert_eq!(
        run.field("service_name").as_deref(),
        Some("signals-probe"),
        "with nothing set, the binary's own name must reach the backend. \
         stderr was: {}",
        run.stderr
    );
}

/// `OTEL_SERVICE_NAME` is the variable an operator reaches for first to tell two
/// deployments of one binary apart, and it must work.
///
/// It is safe to honour because it is per process: `desktop-assistant` builds the
/// environment of every MCP server it spawns from an allowlist, and this variable is
/// deliberately not on it, so a spawned server never inherits the daemon's value.
#[test]
fn otel_service_name_env_beats_the_configured_service_name() {
    let run = run_probe(&[("OTEL_SERVICE_NAME", "named-by-the-operator")]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert_eq!(
        run.field("service_name").as_deref(),
        Some("named-by-the-operator"),
        "a variable an operator sets must change something. stderr was: {}",
        run.stderr
    );
}

/// `service.name` inside `OTEL_RESOURCE_ATTRIBUTES` must not override the configured
/// name.
///
/// The specification requires it: user-provided resource information has higher priority
/// than that variable. The fleet depends on it as well. `desktop-assistant` passes
/// `OTEL_RESOURCE_ATTRIBUTES` down to every MCP server it spawns, so that a server span
/// carries the pod, the namespace and the node. If a `service.name` entry in it won, all
/// thirteen servers and the daemon would report as one service and every trace would be
/// unreadable.
#[test]
fn configured_service_name_beats_service_name_in_otel_resource_attributes() {
    let run = run_probe(&[(
        "OTEL_RESOURCE_ATTRIBUTES",
        "service.name=named-by-the-resource-variable,k8s.pod.name=pod-7",
    )]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert_eq!(
        run.field("service_name").as_deref(),
        Some("signals-probe"),
        "a propagated variable must not rename every process that inherits it. \
         stderr was: {}",
        run.stderr
    );
}

/// Fixing the service name must not be done by ignoring the resource variable.
///
/// That is the obvious wrong answer, and it would take the pod, namespace and node
/// attributes with it - which is the only reason `desktop-assistant` propagates the
/// variable to every server it spawns. This is the criterion that stops that fix.
#[test]
fn other_otel_resource_attributes_still_reach_the_resource() {
    let run = run_probe(&[(
        "OTEL_RESOURCE_ATTRIBUTES",
        "service.name=named-by-the-resource-variable,k8s.pod.name=pod-7,k8s.node.name=node-3",
    )]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);

    let keys = run
        .field("resource_attributes")
        .unwrap_or_else(|| panic!("startup must list the resource keys: {}", run.stderr));
    assert!(
        keys.contains("k8s.pod.name"),
        "the pod attribute must survive, found {keys}. stderr was: {}",
        run.stderr
    );
    assert!(
        keys.contains("k8s.node.name"),
        "the node attribute must survive, found {keys}. stderr was: {}",
        run.stderr
    );
    assert_eq!(
        run.field("service_name").as_deref(),
        Some("signals-probe"),
        "and the service name is still the configured one. stderr was: {}",
        run.stderr
    );
}

/// `OTEL_SERVICE_NAME` beats a `service.name` entry in `OTEL_RESOURCE_ATTRIBUTES`, which
/// the specification states outright.
#[test]
fn otel_service_name_beats_service_name_in_otel_resource_attributes() {
    let run = run_probe(&[
        ("OTEL_SERVICE_NAME", "named-by-the-operator"),
        (
            "OTEL_RESOURCE_ATTRIBUTES",
            "service.name=named-by-the-resource-variable",
        ),
    ]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert_eq!(
        run.field("service_name").as_deref(),
        Some("named-by-the-operator"),
        "stderr was: {}",
        run.stderr
    );
}

/// Startup says which value won, so a variable that was overridden is never silent.
///
/// This is the whole cost the issue names: a documented variable that produces no error
/// and changes nothing costs an operator the time it takes to work out that it was never
/// wired.
#[test]
fn startup_says_where_the_service_name_came_from() {
    let from_config = run_probe(&[]);
    assert_eq!(
        from_config.field("service_name_from").as_deref(),
        Some("Config::new"),
        "stderr was: {}",
        from_config.stderr
    );

    let from_env = run_probe(&[("OTEL_SERVICE_NAME", "named-by-the-operator")]);
    assert_eq!(
        from_env.field("service_name_from").as_deref(),
        Some("OTEL_SERVICE_NAME"),
        "stderr was: {}",
        from_env.stderr
    );
}

// ---------------------------------------------------------------------------------
// Which signals export
// ---------------------------------------------------------------------------------

/// The control. With nothing switched off, all three signals reach their endpoint.
///
/// Without this, every assertion below that a signal did not connect would pass just as
/// well if the probe exported nothing at all.
#[test]
fn all_three_signals_export_when_nothing_is_switched_off() {
    let run = run_probe(&[]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert!(run.traces, "traces must export. stderr was: {}", run.stderr);
    assert!(
        run.metrics,
        "metrics must export. stderr was: {}",
        run.stderr
    );
    assert!(run.logs, "logs must export. stderr was: {}", run.stderr);
}

/// `OTEL_SDK_DISABLED=true` builds no pipeline at all.
#[test]
fn otel_sdk_disabled_builds_no_pipeline() {
    let run = run_probe(&[("OTEL_SDK_DISABLED", "true")]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert!(!run.traces, "stderr was: {}", run.stderr);
    assert!(!run.metrics, "stderr was: {}", run.stderr);
    assert!(!run.logs, "stderr was: {}", run.stderr);
}

/// Turning export off must not cost the process its console logging or its metrics
/// summary. Those are what an operator falls back on when there is no backend.
#[test]
fn otel_sdk_disabled_keeps_console_logging_and_the_metrics_summary() {
    let run = run_probe(&[("OTEL_SDK_DISABLED", "true")]);

    assert!(
        run.stderr
            .contains("a line that must still reach the console"),
        "stderr was: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("metrics summary window_seconds"),
        "the summary itself must be written, not merely announced. stderr was: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("probe.requests"),
        "stderr was: {}",
        run.stderr
    );
}

/// Export being off by request is said at startup, so it does not look like a fault.
#[test]
fn otel_sdk_disabled_is_reported_at_startup() {
    let run = run_probe(&[("OTEL_SDK_DISABLED", "true")]);

    assert!(
        run.stderr.contains("export is off by request"),
        "an operator who set the variable must see that it took effect. \
         stderr was: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("OTEL_SDK_DISABLED"),
        "the report must name the variable. stderr was: {}",
        run.stderr
    );
}

/// Any value other than `true` leaves the SDK enabled, which is what the specification
/// requires of a boolean variable.
#[test]
fn otel_sdk_disabled_with_any_other_value_leaves_export_on() {
    let run = run_probe(&[("OTEL_SDK_DISABLED", "yes")]);

    assert!(run.traces, "stderr was: {}", run.stderr);
    assert!(
        run.stderr.contains("OTEL_SDK_DISABLED"),
        "a value that is neither true nor false must be named, not silently ignored. \
         stderr was: {}",
        run.stderr
    );
}

/// `OTEL_TRACES_EXPORTER=none` switches off traces and leaves the other two working.
#[test]
fn otel_traces_exporter_none_switches_off_only_traces() {
    let run = run_probe(&[("OTEL_TRACES_EXPORTER", "none")]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert!(!run.traces, "stderr was: {}", run.stderr);
    assert!(run.metrics, "stderr was: {}", run.stderr);
    assert!(run.logs, "stderr was: {}", run.stderr);
}

/// `OTEL_METRICS_EXPORTER=none` switches off metrics and leaves the other two working.
#[test]
fn otel_metrics_exporter_none_switches_off_only_metrics() {
    let run = run_probe(&[("OTEL_METRICS_EXPORTER", "none")]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert!(run.traces, "stderr was: {}", run.stderr);
    assert!(!run.metrics, "stderr was: {}", run.stderr);
    assert!(run.logs, "stderr was: {}", run.stderr);
}

/// `OTEL_LOGS_EXPORTER=none` switches off log records and leaves the other two working.
#[test]
fn otel_logs_exporter_none_switches_off_only_logs() {
    let run = run_probe(&[("OTEL_LOGS_EXPORTER", "none")]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert!(run.traces, "stderr was: {}", run.stderr);
    assert!(run.metrics, "stderr was: {}", run.stderr);
    assert!(!run.logs, "stderr was: {}", run.stderr);
}

/// An exporter this crate does not implement is named at startup and ignored, rather
/// than silently switching the signal off.
///
/// The specification requires the warning: an unrecognised enum value MUST be reported
/// and MUST otherwise be ignored.
#[test]
fn an_unsupported_exporter_is_named_and_otlp_still_applies() {
    let run = run_probe(&[("OTEL_TRACES_EXPORTER", "zipkin")]);

    assert!(
        run.traces,
        "an unrecognised value must be ignored, not treated as none. stderr was: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("OTEL_TRACES_EXPORTER"),
        "the report must name the variable and its value. stderr was: {}",
        run.stderr
    );
    assert!(run.stderr.contains("zipkin"), "stderr was: {}", run.stderr);
}

/// With no endpoint set, the OTLP default applies and startup says so.
///
/// The alternative was to refuse or to export nothing, and both would surprise somebody
/// running a collector on the default port. Naming the default costs nothing and removes
/// the surprise, and the same line names the three ways to turn export off.
#[test]
fn the_default_endpoint_is_named_when_no_variable_sets_one() {
    let run = run_probe_without_endpoints(&[]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert!(
        run.stderr.contains("http://localhost:4318"),
        "an operator must be told where telemetry is going. stderr was: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("OTEL_SDK_DISABLED"),
        "the same line must name how to turn export off. stderr was: {}",
        run.stderr
    );
}

// ---------------------------------------------------------------------------------
// How long shutdown may take
// ---------------------------------------------------------------------------------

/// The budget comes from the environment, so an operator can follow a shorter
/// termination grace period without rebuilding every binary in the fleet.
#[test]
fn the_environment_sets_the_shutdown_budget() {
    let run = run_probe(&[("ADELIE_TELEMETRY_SHUTDOWN_BUDGET_MS", "250")]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert_eq!(
        run.field("shutdown_budget_ms").as_deref(),
        Some("250"),
        "stderr was: {}",
        run.stderr
    );
}

/// The in-code setter outranks the variable, so a binary that must have a particular
/// budget still gets it.
#[test]
fn with_shutdown_budget_beats_the_environment() {
    let run = run_probe(&[
        ("ADELIE_TELEMETRY_SHUTDOWN_BUDGET_MS", "250"),
        ("PROBE_SHUTDOWN_BUDGET_MS", "1750"),
    ]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert_eq!(
        run.field("shutdown_budget_ms").as_deref(),
        Some("1750"),
        "stderr was: {}",
        run.stderr
    );
}

/// A value that is not a whole number of milliseconds is named and the default applies.
///
/// Silence here would be the same fault the service-name variable had: a setting an
/// operator made that changes nothing and says nothing.
#[test]
fn an_unparseable_shutdown_budget_is_named_and_the_default_applies() {
    let run = run_probe(&[("ADELIE_TELEMETRY_SHUTDOWN_BUDGET_MS", "5s")]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert!(
        run.stderr.contains("ADELIE_TELEMETRY_SHUTDOWN_BUDGET_MS"),
        "the refusal must name the variable. stderr was: {}",
        run.stderr
    );
    assert_eq!(
        run.field("shutdown_budget_ms").as_deref(),
        Some("5000"),
        "the default must apply when the value cannot be honoured. stderr was: {}",
        run.stderr
    );
}

/// A negative value is refused by name rather than wrapping into an enormous budget.
#[test]
fn a_negative_shutdown_budget_is_named_and_the_default_applies() {
    let run = run_probe(&[("ADELIE_TELEMETRY_SHUTDOWN_BUDGET_MS", "-1")]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert!(
        run.stderr.contains("ADELIE_TELEMETRY_SHUTDOWN_BUDGET_MS"),
        "stderr was: {}",
        run.stderr
    );
    assert_eq!(
        run.field("shutdown_budget_ms").as_deref(),
        Some("5000"),
        "stderr was: {}",
        run.stderr
    );
}

/// Zero means do not wait at all: the flush is not attempted and the process stops.
///
/// The alternative was letting `Duration::ZERO` reach the flush, where it times out
/// immediately and warns that buffered telemetry may have been lost - on every single
/// stop, for an operator who asked for exactly this.
#[test]
fn a_zero_shutdown_budget_does_not_wait_for_the_flush() {
    let run = run_probe(&[("ADELIE_TELEMETRY_SHUTDOWN_BUDGET_MS", "0")]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert!(
        run.stderr.contains("the shutdown budget is zero"),
        "the deliberate loss of buffered telemetry must be stated. stderr was: {}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("did not shut down within their budget"),
        "a budget of zero is what the operator asked for, so it is not a warning. \
         stderr was: {}",
        run.stderr
    );
}

// ---------------------------------------------------------------------------------
// How often the in-process metrics summary is written
// ---------------------------------------------------------------------------------

/// Whether the summary itself was written.
///
/// Matched on a field only the summary carries. A startup line that names the interval
/// says the summary is on; it is not the summary.
fn wrote_a_summary(run: &Run) -> bool {
    run.stderr.contains("metrics summary window_seconds")
}

/// With the OTLP metrics pipeline running, the summary is off.
///
/// Those series are already exported as metrics. The log dump would be a second copy of
/// the same numbers in a different signal, from every binary in the fleet, every ten
/// minutes.
#[test]
fn the_summary_is_off_when_the_otlp_metrics_pipeline_is_active() {
    let run = run_probe(&[]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert!(run.metrics, "the control: metrics must be exporting");
    assert!(
        !wrote_a_summary(&run),
        "the same numbers must not be paid for twice. stderr was: {}",
        run.stderr
    );
}

/// With no metrics exporter, the summary still writes. The no-backend case is unchanged.
#[test]
fn the_summary_writes_when_no_metrics_exporter_is_active() {
    let run = run_probe(&[("OTEL_METRICS_EXPORTER", "none")]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert!(!run.metrics, "the control: metrics must not be exporting");
    assert!(
        wrote_a_summary(&run),
        "with nothing exporting the series, the summary is the only place a number \
         appears. stderr was: {}",
        run.stderr
    );
}

/// Switching the whole SDK off turns the summary back on for the same reason.
#[test]
fn the_summary_writes_when_the_sdk_is_disabled() {
    let run = run_probe(&[("OTEL_SDK_DISABLED", "true")]);

    assert!(wrote_a_summary(&run), "stderr was: {}", run.stderr);
}

/// The interval comes from the environment, so it needs no rebuild.
#[test]
fn the_environment_sets_the_summary_interval() {
    let run = run_probe(&[("ADELIE_TELEMETRY_METRICS_SUMMARY_INTERVAL_MS", "30000")]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert_eq!(
        run.field("interval_ms").as_deref(),
        Some("30000"),
        "stderr was: {}",
        run.stderr
    );
    assert!(
        wrote_a_summary(&run),
        "an interval asked for by name must beat the pipeline default. stderr was: {}",
        run.stderr
    );
}

/// Zero turns the summary off whatever the pipeline state is.
///
/// Set together with `OTEL_METRICS_EXPORTER=none`, which would otherwise turn the summary
/// on, so the test fails if the variable is read only as a fallback.
#[test]
fn zero_turns_the_summary_off_whatever_the_pipeline_state() {
    let run = run_probe(&[
        ("OTEL_METRICS_EXPORTER", "none"),
        ("ADELIE_TELEMETRY_METRICS_SUMMARY_INTERVAL_MS", "0"),
    ]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert!(!wrote_a_summary(&run), "stderr was: {}", run.stderr);
}

/// The in-code setter outranks the variable.
#[test]
fn with_metrics_dump_interval_beats_the_environment() {
    let run = run_probe(&[
        ("ADELIE_TELEMETRY_METRICS_SUMMARY_INTERVAL_MS", "30000"),
        ("PROBE_SUMMARY_INTERVAL_MS", "45000"),
    ]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert_eq!(
        run.field("interval_ms").as_deref(),
        Some("45000"),
        "stderr was: {}",
        run.stderr
    );
}

/// A value the crate cannot parse is named, and neither enables nor disables the summary
/// by accident: the pipeline decides, exactly as it would with the variable unset.
#[test]
fn an_unparseable_summary_interval_is_named_and_decides_nothing() {
    let run = run_probe(&[("ADELIE_TELEMETRY_METRICS_SUMMARY_INTERVAL_MS", "10m")]);

    assert!(run.success, "the probe must exit cleanly: {}", run.stderr);
    assert!(
        run.stderr
            .contains("ADELIE_TELEMETRY_METRICS_SUMMARY_INTERVAL_MS"),
        "the refusal must name the variable. stderr was: {}",
        run.stderr
    );
    assert!(
        !wrote_a_summary(&run),
        "with metrics exporting, an unusable value must leave the summary off, which is \
         what it would have been. stderr was: {}",
        run.stderr
    );
}

/// Startup states the resolved interval and why, so an operator who expected a summary
/// and does not see one can tell which rule took it away.
#[test]
fn startup_states_the_resolved_interval_and_the_reason() {
    let exporting = run_probe(&[]);
    assert_eq!(
        exporting.field("interval_ms").as_deref(),
        Some("0"),
        "stderr was: {}",
        exporting.stderr
    );
    assert!(
        exporting.stderr.contains("the OTLP metrics pipeline"),
        "the reason must name the pipeline that took it away. stderr was: {}",
        exporting.stderr
    );

    let not_exporting = run_probe(&[("OTEL_METRICS_EXPORTER", "none")]);
    assert_eq!(
        not_exporting.field("interval_ms").as_deref(),
        Some("600000"),
        "stderr was: {}",
        not_exporting.stderr
    );
    assert!(
        not_exporting.stderr.contains("no metrics exporter"),
        "stderr was: {}",
        not_exporting.stderr
    );
}
