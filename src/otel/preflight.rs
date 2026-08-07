//! What must be true before an exporter is built.
//!
//! Both checks here exist to turn a bad diagnosis into a good one. The failures they
//! catch do surface without them, but as a panic from inside `hyper-util` or as
//! `error="network error"`, and neither names the variable an operator has to change.

use crate::Error;

/// Whether a TLS backend is compiled in.
///
/// On unless the consumer took `default-features = false`. Without one an `https`
/// endpoint cannot work at all. The exporter fails closed rather than falling back to
/// plaintext, so nothing leaks; it just fails for a reason that reads like a network
/// fault, which is why [`check`] refuses it by name first.
pub const TLS_AVAILABLE: bool = cfg!(feature = "otel-tls");

/// The value of `OTEL_EXPORTER_OTLP_PROTOCOL` that selects gRPC.
const PROTOCOL_GRPC: &str = "grpc";

/// The generic protocol variable.
const PROTOCOL_VAR: &str = "OTEL_EXPORTER_OTLP_PROTOCOL";

/// The generic endpoint variable.
const ENDPOINT_VAR: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// Whether this signal ends up on the gRPC transport.
///
/// Mirrors the resolution order the exporter uses: the per-signal variable, then the
/// generic one, then the crate's compiled default, which is `http/protobuf`.
pub(crate) fn resolves_to_grpc(signal_protocol_var: &str) -> bool {
    for variable in [signal_protocol_var, PROTOCOL_VAR] {
        if let Ok(value) = std::env::var(variable) {
            let value = value.trim();
            if !value.is_empty() {
                return value.eq_ignore_ascii_case(PROTOCOL_GRPC);
            }
        }
    }
    false
}

/// The endpoint this signal will use, if any variable sets one.
fn endpoint(signal_endpoint_var: &str) -> Option<String> {
    for variable in [signal_endpoint_var, ENDPOINT_VAR] {
        if let Ok(value) = std::env::var(variable) {
            let value = value.trim().to_owned();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Refuse a configuration that cannot work, naming what to change.
///
/// Returns `Ok(())` when the signal can be exported as configured.
pub(crate) fn check(
    signal: &'static str,
    signal_protocol_var: &str,
    signal_endpoint_var: &str,
) -> Result<(), Error> {
    if resolves_to_grpc(signal_protocol_var) && !runtime_is_running() {
        return Err(Error::Pipeline {
            signal,
            message: format!(
                "{PROTOCOL_VAR} selects gRPC, and the gRPC transport needs a Tokio runtime. \
                 Call init from inside a runtime, or set {PROTOCOL_VAR}=http/protobuf, which \
                 needs no runtime"
            ),
        });
    }

    if !TLS_AVAILABLE
        && let Some(endpoint) = endpoint(signal_endpoint_var)
        && endpoint.starts_with("https://")
    {
        return Err(Error::Pipeline {
            signal,
            message: format!(
                "{ENDPOINT_VAR} uses https, and this build has no TLS backend compiled \
                 in. Something took `default-features = false` on adelie-telemetry, and \
                 the TLS backend is one of those defaults. Use an http endpoint, or \
                 restore the default features"
            ),
        });
    }

    Ok(())
}

/// An endpoint with any userinfo removed.
///
/// `https://user:password@host:4318` is a documented way to authenticate to several OTLP
/// backends, so the password is genuinely there to be printed. The same reasoning that
/// keeps `OTEL_EXPORTER_OTLP_HEADERS` out of the log applies to it: this line goes to
/// `kubectl logs`, and from there into the telemetry backend itself.
fn redact_userinfo(endpoint: &str) -> String {
    let Some((scheme, rest)) = endpoint.split_once("://") else {
        return endpoint.to_owned();
    };
    // Userinfo ends at the first `@`, and only counts inside the authority, which ends at
    // the first `/`, `?` or `#`.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    match authority.rsplit_once('@') {
        Some((_userinfo, host)) => format!("{scheme}://<redacted>@{host}{tail}"),
        None => endpoint.to_owned(),
    }
}

/// Whether a Tokio runtime is running on this thread.
fn runtime_is_running() -> bool {
    tokio::runtime::Handle::try_current().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default is `http/protobuf`, which needs no runtime, so an unset variable must
    /// not be read as gRPC.
    #[test]
    fn an_unset_protocol_is_not_grpc() {
        assert!(!resolves_to_grpc(
            "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL_UNSET_FOR_TEST"
        ));
    }
}

/// The OTLP variables that are set, for a failure report.
///
/// Values are included so an operator can see the typo without going to look, except for
/// `OTEL_EXPORTER_OTLP_HEADERS` and its per-signal forms, which routinely carry an API
/// key. Those are reported as set or unset and never by value.
pub(crate) fn configuration_summary() -> String {
    summarize(|name| std::env::var(name).ok())
}

/// The report, built from a given source of values.
///
/// The source is a parameter rather than `std::env` for the same reason the metrics
/// registry takes a clock: a test cannot set up process-global state without disturbing
/// another test running beside it.
///
/// The environment is the worst example of that. `std::env::set_var` is `unsafe` in
/// edition 2024 because `setenv` rewrites a shared array while any other thread may be
/// reading it, and that holds whichever variable is named - so giving each test its own
/// variable would not make the mutation sound, it would only hide the collision. A lock
/// would serialise this crate's own tests and still not cover a read from a runtime
/// thread. Injecting the lookup removes the mutation rather than scheduling around it,
/// and the tests below need no `unsafe` at all.
fn summarize(lookup: impl Fn(&str) -> Option<String>) -> String {
    const VALUE_VARS: &[&str] = &[
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
        "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
        "OTEL_EXPORTER_OTLP_PROTOCOL",
        "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
        "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
        "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
        "OTEL_EXPORTER_OTLP_COMPRESSION",
        "OTEL_EXPORTER_OTLP_TIMEOUT",
    ];
    const SECRET_VARS: &[&str] = &[
        "OTEL_EXPORTER_OTLP_HEADERS",
        "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
        "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
        "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
    ];

    // Sanitised with the same function the metric labels use. These values come from a
    // deployment overlay, and a YAML block scalar or a stray carriage return puts a line
    // break in one by accident long before anybody does it on purpose. One forged line in
    // `kubectl logs` reads exactly like a real one.
    let mut parts: Vec<String> = VALUE_VARS
        .iter()
        .filter_map(|name| {
            lookup(name).map(|value| {
                format!(
                    "{name}={}",
                    crate::metrics::sanitize(redact_userinfo(&value))
                )
            })
        })
        .collect();
    parts.extend(
        SECRET_VARS
            .iter()
            .filter(|name| lookup(name).is_some())
            .map(|name| format!("{name}=<set>")),
    );

    if parts.is_empty() {
        "no OTEL_EXPORTER_OTLP_* variable is set".to_owned()
    } else {
        parts.join(" ")
    }
}

#[cfg(test)]
mod summary_tests {
    use super::*;

    /// A report built from the given values, touching no process-global state.
    ///
    /// Every test here can run beside every other, because none of them writes to the
    /// environment. That is the point of `summarize` taking its source as a parameter.
    fn summary_of(values: &[(&str, &str)]) -> String {
        let owned: Vec<(String, String)> = values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        summarize(|name| {
            owned
                .iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, value)| value.clone())
        })
    }

    /// The failure report cannot reverse what an operator reads.
    ///
    /// It is the other place caller-controlled text reaches a log field, and it goes
    /// through the same sanitiser, so this holds the two together.
    #[test]
    fn the_summary_strips_bidi_controls() {
        let summary = summary_of(&[("OTEL_EXPORTER_OTLP_COMPRESSION", "gzip\u{202e}desrever")]);

        assert!(
            !summary.contains('\u{202e}'),
            "a bidi override reverses the rest of the line for a reader: {summary:?}"
        );
        assert!(summary.contains("gzip"), "the readable part must survive");
    }

    /// A password in the endpoint must never be printed.
    ///
    /// Basic auth in the URL is a documented pattern for several OTLP backends, so the
    /// value really does carry one.
    #[test]
    fn the_summary_redacts_credentials_in_an_endpoint() {
        let summary = summary_of(&[(
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "https://svcuser:S3cretInUrl@collector.example.com:4318/v1/traces",
        )]);

        assert!(
            !summary.contains("S3cretInUrl"),
            "a password in the endpoint must not reach the log: {summary}"
        );
        assert!(!summary.contains("svcuser"));
        assert!(
            summary.contains("collector.example.com:4318"),
            "the host must survive, or the report stops being useful: {summary}"
        );
        assert!(summary.contains("<redacted>"));
    }

    /// An endpoint without credentials is printed unchanged.
    #[test]
    fn the_summary_leaves_a_plain_endpoint_alone() {
        assert_eq!(
            redact_userinfo("https://collector.example.com:4318/v1/traces"),
            "https://collector.example.com:4318/v1/traces"
        );
        assert_eq!(redact_userinfo("not-a-url"), "not-a-url");
    }

    /// A value cannot forge a log line, for the same reason a metric label cannot: the
    /// report goes into a log field, and a newline in a field ends the line early.
    #[test]
    fn the_summary_cannot_forge_a_log_line() {
        let summary = summary_of(&[(
            "OTEL_EXPORTER_OTLP_COMPRESSION",
            "gzip\n2026-08-07T00:00:00.000000Z  INFO probe: FORGED user=root",
        )]);

        assert!(
            !summary.contains('\n'),
            "a newline would end the log line and start a forged one: {summary:?}"
        );
        assert!(!summary.contains('\u{2028}'));
        assert!(summary.contains("gzip"), "the readable part must survive");
    }

    /// A header value is a credential often enough that it is never printed.
    #[test]
    fn the_summary_never_prints_a_header_value() {
        let summary = summary_of(&[("OTEL_EXPORTER_OTLP_HEADERS", "api-key=super-secret-value")]);

        assert!(summary.contains("OTEL_EXPORTER_OTLP_HEADERS=<set>"));
        assert!(
            !summary.contains("super-secret-value"),
            "a header value is a credential and must never reach the log: {summary}"
        );
    }

    /// With nothing set, the report says so rather than being blank.
    #[test]
    fn an_empty_environment_is_reported_as_such() {
        assert_eq!(summary_of(&[]), "no OTEL_EXPORTER_OTLP_* variable is set");
    }

    /// Every variable that is set appears, so the report is not silently partial.
    #[test]
    fn every_set_variable_appears() {
        let summary = summary_of(&[
            (
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                "http://collector.example.com:4318",
            ),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ("OTEL_EXPORTER_OTLP_LOGS_HEADERS", "api-key=secret"),
        ]);

        assert!(summary.contains("OTEL_EXPORTER_OTLP_ENDPOINT=http://collector.example.com:4318"));
        assert!(summary.contains("OTEL_EXPORTER_OTLP_PROTOCOL=grpc"));
        assert!(summary.contains("OTEL_EXPORTER_OTLP_LOGS_HEADERS=<set>"));
        assert!(!summary.contains("secret"));
    }
}
