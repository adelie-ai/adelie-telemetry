//! Which value names the service, and the resource every exported signal carries.
//!
//! Only compiled with the `otel` feature.

use opentelemetry::{Key, Value};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::resource::ResourceBuilder;

/// The variable that names the service on its own.
pub(crate) const SERVICE_NAME_VAR: &str = "OTEL_SERVICE_NAME";

/// The resource attribute a service name lives under.
const SERVICE_NAME_KEY: &str = "service.name";

/// Where the service name a backend sees came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ServiceNameSource {
    /// `OTEL_SERVICE_NAME`.
    Variable,
    /// The name the binary passed to `Config::new`.
    Configured,
}

impl ServiceNameSource {
    /// What startup calls this source.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Variable => SERVICE_NAME_VAR,
            Self::Configured => "Config::new",
        }
    }
}

/// The service name this process reports, and where it came from.
///
/// # The order, and why it is this one
///
/// 1. **`OTEL_SERVICE_NAME`.** It is the variable an operator reaches for first to tell
///    two deployments of one binary apart, and it is set per process. Nothing propagates
///    it: `desktop-assistant` builds the environment of every MCP server it spawns from
///    an allowlist, and this variable is deliberately not on that list, so honouring it
///    renames one process rather than a fleet.
/// 2. **The name the binary passed to `Config::new`.** Every Adelie binary passes one,
///    so the SDK's own `unknown_service:<executable>` fallback is never reached.
/// 3. **`service.name` inside `OTEL_RESOURCE_ATTRIBUTES`**, which never wins here. See
///    [`resource`] for why that one is different.
///
/// The specification ranks only two of these outright: `OTEL_SERVICE_NAME` beats a
/// `service.name` entry in `OTEL_RESOURCE_ATTRIBUTES`, and user-provided resource
/// information beats `OTEL_RESOURCE_ATTRIBUTES`. This order holds both.
pub(crate) fn resolve_service_name(
    configured: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> (String, ServiceNameSource) {
    match named_by(SERVICE_NAME_VAR, lookup) {
        Some(name) => (name, ServiceNameSource::Variable),
        None => (configured.to_owned(), ServiceNameSource::Configured),
    }
}

/// The value of a variable, treating empty as unset, which the specification requires.
fn named_by(variable: &str, lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    lookup(variable)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The resource, built over whatever the SDK's own detectors found.
///
/// `detected` is [`Resource::builder`] in a running process, which has already read
/// `OTEL_RESOURCE_ATTRIBUTES` and put every pair in it into the resource. The service
/// name goes on last, and `Resource::merge` gives the *other* side priority - "keys from
/// the `other` resource have priority over keys from this resource". So this value
/// replaces any `service.name` the detectors produced, and every other attribute they
/// produced survives untouched.
///
/// **That merge direction is the whole mechanism**, and it belongs to a third-party
/// crate, so a version bump could reverse it with no signal here. It is pinned by
/// `the_service_name_merges_over_one_the_detectors_found` below, which is the only thing
/// standing between the fleet and every process reporting under one name.
///
/// Why `service.name` from `OTEL_RESOURCE_ATTRIBUTES` must lose, when
/// `OTEL_SERVICE_NAME` wins: that variable *is* propagated. `desktop-assistant` passes it
/// down to every MCP server it spawns, deliberately, so a server span carries the pod,
/// the namespace and the node. If a `service.name` entry in it won, thirteen servers and
/// the daemon would all report as one service and every trace would be unreadable.
pub(crate) fn resource(detected: ResourceBuilder, service_name: impl Into<Value>) -> Resource {
    detected.with_service_name(service_name).build()
}

/// Every attribute key the resource carries, sorted, as one comma-separated string.
///
/// Keys and not values: a key is a semantic-convention name and is short, and the list
/// answers the question an operator actually has, which is whether the pairs they put in
/// `OTEL_RESOURCE_ATTRIBUTES` reached the resource at all.
pub(crate) fn attribute_keys(resource: &Resource) -> String {
    let mut keys: Vec<&str> = resource.iter().map(|(key, _)| key.as_str()).collect();
    keys.sort_unstable();
    keys.join(",")
}

/// The `service.name` a resource really carries.
///
/// Read back off the built resource rather than taken from the decision that went into
/// it, so what startup reports is what the SDK produced.
pub(crate) fn service_name_of(resource: &Resource) -> String {
    resource
        .get(&Key::from_static_str(SERVICE_NAME_KEY))
        .map(|value| value.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::KeyValue;

    /// A lookup over a fixed set of values, touching no process-global state.
    ///
    /// The environment is the worst thing a test can mutate: `std::env::set_var` is
    /// `unsafe` in edition 2024 because `setenv` rewrites a shared array while any other
    /// thread may be reading it. Injecting the lookup removes the mutation rather than
    /// scheduling around it.
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
    fn the_configured_name_applies_when_no_variable_is_set() {
        let (name, source) = resolve_service_name("adele-daemon", lookup(&[]));

        assert_eq!(name, "adele-daemon");
        assert_eq!(source, ServiceNameSource::Configured);
    }

    #[test]
    fn otel_service_name_outranks_the_configured_name() {
        let (name, source) = resolve_service_name(
            "adele-daemon",
            lookup(&[(SERVICE_NAME_VAR, "named-by-the-operator")]),
        );

        assert_eq!(name, "named-by-the-operator");
        assert_eq!(source, ServiceNameSource::Variable);
    }

    /// The specification requires an empty value to read as unset.
    #[test]
    fn an_empty_otel_service_name_reads_as_unset() {
        let (name, source) =
            resolve_service_name("adele-daemon", lookup(&[(SERVICE_NAME_VAR, "   ")]));

        assert_eq!(name, "adele-daemon");
        assert_eq!(source, ServiceNameSource::Configured);
    }

    /// A `service.name` the detectors found must not replace the name that goes on top.
    ///
    /// This is the test that pins the SDK's merge direction. It fails if `merge` ever
    /// gives priority to the base rather than to the other side, which is the version
    /// bump nothing else here would notice.
    #[test]
    fn the_service_name_merges_over_one_the_detectors_found() {
        let detected = Resource::builder_empty().with_attributes([KeyValue::new(
            SERVICE_NAME_KEY,
            "named-by-the-resource-variable",
        )]);

        let resource = resource(detected, "adele-daemon".to_owned());

        assert_eq!(
            service_name_of(&resource),
            "adele-daemon",
            "a variable that is propagated to child processes must not be able to rename \
             every one of them"
        );
    }

    /// The fix for the service name must not be "drop everything else in the resource".
    ///
    /// The end-to-end half of this - that the variable is read at all - is
    /// `other_otel_resource_attributes_still_reach_the_resource` in
    /// `tests/acceptance_environment.rs`, because this test supplies its own detected
    /// resource and so cannot see the variable being ignored.
    #[test]
    fn other_attributes_in_the_detected_resource_survive() {
        let detected = Resource::builder_empty().with_attributes([
            KeyValue::new(SERVICE_NAME_KEY, "named-by-the-resource-variable"),
            KeyValue::new("k8s.pod.name", "pod-7"),
            KeyValue::new("k8s.namespace.name", "adelie"),
        ]);

        let resource = resource(detected, "adele-daemon".to_owned());

        assert_eq!(
            resource
                .get(&Key::from_static_str("k8s.pod.name"))
                .map(|value| value.to_string()),
            Some("pod-7".to_owned())
        );
        assert_eq!(
            resource
                .get(&Key::from_static_str("k8s.namespace.name"))
                .map(|value| value.to_string()),
            Some("adelie".to_owned())
        );
    }

    /// A resource with no service name at all reports an empty one rather than panicking.
    #[test]
    fn a_resource_without_a_service_name_reports_an_empty_one() {
        assert_eq!(service_name_of(&Resource::builder_empty().build()), "");
    }

    /// The key list is what startup reports, so it has to be stable and complete.
    #[test]
    fn the_attribute_keys_are_listed_in_a_stable_order() {
        let detected = Resource::builder_empty().with_attributes([
            KeyValue::new("k8s.pod.name", "pod-7"),
            KeyValue::new("k8s.namespace.name", "adelie"),
        ]);

        let resource = resource(detected, "adele-daemon".to_owned());

        assert_eq!(
            attribute_keys(&resource),
            "k8s.namespace.name,k8s.pod.name,service.name"
        );
    }
}
