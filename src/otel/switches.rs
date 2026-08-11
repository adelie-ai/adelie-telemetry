//! Which signals this process exports, as the environment asks.
//!
//! Only compiled with the `otel` feature.
//!
//! Nothing here changes *where* telemetry goes; the exporter builders read those
//! variables themselves. These four decide whether a pipeline is built at all, which no
//! layer below can decide, because a pipeline that exists always exports somewhere.

/// The variable that switches every signal off at once.
pub(crate) const SDK_DISABLED_VAR: &str = "OTEL_SDK_DISABLED";

/// The variable that chooses the trace exporter.
pub(crate) const TRACES_EXPORTER_VAR: &str = "OTEL_TRACES_EXPORTER";

/// The variable that chooses the metric exporter.
pub(crate) const METRICS_EXPORTER_VAR: &str = "OTEL_METRICS_EXPORTER";

/// The variable that chooses the log-record exporter.
pub(crate) const LOGS_EXPORTER_VAR: &str = "OTEL_LOGS_EXPORTER";

/// The exporter value that switches one signal off.
const EXPORTER_NONE: &str = "none";

/// The only exporter this crate builds. Also the specification's default.
const EXPORTER_OTLP: &str = "otlp";

/// The only value that means true, per the specification's boolean rule.
const TRUE: &str = "true";

/// The other value a boolean variable may hold without complaint.
const FALSE: &str = "false";

/// Which signals export, and what the environment asked for that could not be honoured.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Switches {
    /// `OTEL_SDK_DISABLED` asked for every signal to be off.
    pub(crate) sdk_disabled: bool,
    /// Whether the trace pipeline is built.
    pub(crate) traces: bool,
    /// Whether the metric pipeline is built.
    pub(crate) metrics: bool,
    /// Whether the log-record pipeline is built.
    pub(crate) logs: bool,
    /// One whole sentence per value that could not be honoured, for the log.
    ///
    /// Collected rather than written where they are found, because none of them can be
    /// written until the subscriber exists, and the subscriber cannot be built until the
    /// pipelines are.
    pub(crate) complaints: Vec<String>,
}

impl Switches {
    /// What the environment asks for.
    pub(crate) fn resolve(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let mut complaints = Vec::new();
        let sdk_disabled = disabled(&lookup, &mut complaints);

        if sdk_disabled {
            // The per-signal variables are not read at all in this case. Reading them
            // could only produce a complaint about a value that changes nothing, which
            // is the shape of noise this crate is trying to remove.
            return Self {
                sdk_disabled,
                traces: false,
                metrics: false,
                logs: false,
                complaints,
            };
        }

        Self {
            sdk_disabled,
            traces: exports(TRACES_EXPORTER_VAR, &lookup, &mut complaints),
            metrics: exports(METRICS_EXPORTER_VAR, &lookup, &mut complaints),
            logs: exports(LOGS_EXPORTER_VAR, &lookup, &mut complaints),
            complaints,
        }
    }
}

/// Whether `OTEL_SDK_DISABLED` switches export off.
///
/// The specification is strict about this one: only the case-insensitive string `true`
/// means true, an implementation must not extend that set, and any other value is false
/// and should be reported. So `1`, `yes` and `on` all leave export running, and the
/// operator who wrote one is told why rather than left to find out from a backend.
fn disabled(lookup: impl Fn(&str) -> Option<String>, complaints: &mut Vec<String>) -> bool {
    let Some(value) = value_of(SDK_DISABLED_VAR, lookup) else {
        return false;
    };
    if value.eq_ignore_ascii_case(TRUE) {
        return true;
    }
    if !value.eq_ignore_ascii_case(FALSE) {
        complaints.push(format!(
            "{SDK_DISABLED_VAR}={value} is not a boolean. Only the value `true` switches \
             export off, so this run keeps exporting"
        ));
    }
    false
}

/// Whether this signal's pipeline is built.
///
/// `none` switches the signal off and `otlp` is the only exporter compiled in. Anything
/// else is reported and ignored, which is what the specification requires of an
/// unrecognised enum value - and it is the safer way round, because reading an
/// unrecognised value as `none` would silently stop a signal an operator asked for.
fn exports(
    variable: &str,
    lookup: impl Fn(&str) -> Option<String>,
    complaints: &mut Vec<String>,
) -> bool {
    let Some(value) = value_of(variable, lookup) else {
        return true;
    };
    if value.eq_ignore_ascii_case(EXPORTER_NONE) {
        return false;
    }
    if !value.eq_ignore_ascii_case(EXPORTER_OTLP) {
        complaints.push(format!(
            "{variable}={value} names an exporter this build does not have. Only `otlp` \
             and `none` are implemented, so this signal keeps exporting over OTLP"
        ));
    }
    true
}

/// The value of a variable, treating empty as unset, which the specification requires.
fn value_of(variable: &str, lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    lookup(variable)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lookup over a fixed set of values, touching no process-global state.
    fn lookup(values: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        move |name: &str| {
            owned
                .iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn an_empty_environment_exports_all_three_signals() {
        let switches = Switches::resolve(lookup(&[]));

        assert!(switches.traces && switches.metrics && switches.logs);
        assert!(!switches.sdk_disabled);
        assert!(switches.complaints.is_empty());
    }

    #[test]
    fn otel_sdk_disabled_true_switches_every_signal_off() {
        let switches = Switches::resolve(lookup(&[(SDK_DISABLED_VAR, "TRUE")]));

        assert!(switches.sdk_disabled);
        assert!(!switches.traces && !switches.metrics && !switches.logs);
        assert!(switches.complaints.is_empty(), "`TRUE` is a valid value");
    }

    /// The specification forbids extending the set of values that mean true.
    #[test]
    fn a_boolean_value_other_than_true_leaves_export_on_and_is_named() {
        for value in ["yes", "1", "on", "disabled"] {
            let switches = Switches::resolve(lookup(&[(SDK_DISABLED_VAR, value)]));

            assert!(!switches.sdk_disabled, "{value} must not mean true");
            assert!(switches.traces);
            assert_eq!(
                switches.complaints.len(),
                1,
                "{value} must be reported, not silently ignored"
            );
            assert!(switches.complaints[0].contains(SDK_DISABLED_VAR));
            assert!(switches.complaints[0].contains(value));
        }
    }

    /// `false` is the documented other half of the boolean, so it draws no complaint.
    #[test]
    fn otel_sdk_disabled_false_is_accepted_silently() {
        let switches = Switches::resolve(lookup(&[(SDK_DISABLED_VAR, "False")]));

        assert!(!switches.sdk_disabled);
        assert!(switches.complaints.is_empty());
    }

    #[test]
    fn each_exporter_variable_switches_off_only_its_own_signal() {
        let traces_off = Switches::resolve(lookup(&[(TRACES_EXPORTER_VAR, "none")]));
        assert!(!traces_off.traces && traces_off.metrics && traces_off.logs);

        let metrics_off = Switches::resolve(lookup(&[(METRICS_EXPORTER_VAR, "NONE")]));
        assert!(metrics_off.traces && !metrics_off.metrics && metrics_off.logs);

        let logs_off = Switches::resolve(lookup(&[(LOGS_EXPORTER_VAR, "none")]));
        assert!(logs_off.traces && logs_off.metrics && !logs_off.logs);
    }

    #[test]
    fn otlp_is_accepted_silently_because_it_is_what_this_build_does() {
        let switches = Switches::resolve(lookup(&[(TRACES_EXPORTER_VAR, "otlp")]));

        assert!(switches.traces);
        assert!(switches.complaints.is_empty());
    }

    /// An exporter this build does not have must not be read as `none`.
    #[test]
    fn an_unsupported_exporter_is_named_and_the_signal_keeps_exporting() {
        let switches = Switches::resolve(lookup(&[(METRICS_EXPORTER_VAR, "prometheus")]));

        assert!(
            switches.metrics,
            "an unrecognised value must be ignored, not treated as a request to stop"
        );
        assert_eq!(switches.complaints.len(), 1);
        assert!(switches.complaints[0].contains(METRICS_EXPORTER_VAR));
        assert!(switches.complaints[0].contains("prometheus"));
    }

    /// A comma-separated list is not implemented, so it is reported rather than guessed
    /// at.
    #[test]
    fn a_list_of_exporters_is_named_rather_than_half_honoured() {
        let switches = Switches::resolve(lookup(&[(LOGS_EXPORTER_VAR, "otlp,console")]));

        assert!(switches.logs);
        assert_eq!(switches.complaints.len(), 1);
    }

    #[test]
    fn an_empty_value_reads_as_unset() {
        let switches = Switches::resolve(lookup(&[
            (SDK_DISABLED_VAR, "  "),
            (TRACES_EXPORTER_VAR, ""),
        ]));

        assert!(!switches.sdk_disabled);
        assert!(switches.traces);
        assert!(switches.complaints.is_empty());
    }
}
