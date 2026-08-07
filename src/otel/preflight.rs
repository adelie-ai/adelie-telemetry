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
fn resolves_to_grpc(signal_protocol_var: &str) -> bool {
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

    let mut parts: Vec<String> = VALUE_VARS
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| format!("{name}={value}"))
        })
        .collect();
    parts.extend(
        SECRET_VARS
            .iter()
            .filter(|name| std::env::var(name).is_ok())
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

    /// A header value is a credential often enough that it is never printed.
    #[test]
    fn the_summary_never_prints_a_header_value() {
        // SAFETY: this test owns these variables; no other test in this binary reads them.
        unsafe {
            std::env::set_var("OTEL_EXPORTER_OTLP_HEADERS", "api-key=super-secret-value");
        }
        let summary = configuration_summary();
        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_HEADERS");
        }

        assert!(summary.contains("OTEL_EXPORTER_OTLP_HEADERS=<set>"));
        assert!(
            !summary.contains("super-secret-value"),
            "a header value is a credential and must never reach the log: {summary}"
        );
    }
}
