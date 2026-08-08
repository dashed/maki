//! Which upstream a broker should pick when several serve the same model.
//!
//! Two layers, checked in order: a session override set by `/provider`, then
//! `providers.toml`. The session override lives here rather than travelling in
//! `RequestOptions` because it is one setting for the whole session, the same
//! shape as the model registry's overrides.

use maki_config::providers::{ProvidersConfig, RoutingConfig};
use serde_json::{Value, json};
use std::sync::{OnceLock, RwLock};

fn session_slot() -> &'static RwLock<Option<RoutingConfig>> {
    static SLOT: OnceLock<RwLock<Option<RoutingConfig>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// `None` clears the override and falls back to `providers.toml`.
pub fn set_session_override(routing: Option<RoutingConfig>) {
    *session_slot().write().unwrap() = routing;
}

pub fn session_override() -> Option<RoutingConfig> {
    session_slot().read().unwrap().clone()
}

/// What should actually be sent for `slug`, or `None` when nothing is asked for
/// and the broker should be left to its own defaults.
pub fn resolve(slug: &str) -> Option<RoutingConfig> {
    let routing = session_override().or_else(|| {
        ProvidersConfig::load()
            .get(slug)
            .and_then(|def| def.routing.clone())
    })?;
    (!routing.is_empty()).then_some(routing)
}

/// The `provider` field of an OpenRouter request body.
pub fn to_body_value(routing: &RoutingConfig) -> Value {
    let mut provider = json!({});
    if let Some(sort) = routing.sort {
        provider["sort"] = Value::String(sort.as_str().to_string());
    }
    if !routing.ignore.is_empty() {
        provider["ignore"] = json!(routing.ignore);
    }
    if !routing.only.is_empty() {
        provider["only"] = json!(routing.only);
    }
    if let Some(dc) = routing.data_collection {
        provider["data_collection"] = Value::String(dc.as_str().to_string());
    }
    provider
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_config::providers::{DataCollection, RoutingSort};
    use test_case::test_case;

    const UPSTREAM_A: &str = "together";
    const UPSTREAM_B: &str = "deepinfra";

    fn routing(sort: Option<RoutingSort>) -> RoutingConfig {
        RoutingConfig {
            sort,
            ..Default::default()
        }
    }

    #[test_case(RoutingSort::Price,      "price"      ; "price")]
    #[test_case(RoutingSort::Throughput, "throughput" ; "throughput")]
    #[test_case(RoutingSort::Latency,    "latency"    ; "latency")]
    fn sort_serializes_to_the_wire_name(sort: RoutingSort, expected: &str) {
        let body = to_body_value(&routing(Some(sort)));
        assert_eq!(body["sort"], expected);
    }

    /// Absent knobs must stay absent: sending `"sort": null` would override the
    /// broker's own default with nothing.
    #[test]
    fn empty_routing_emits_no_keys() {
        let body = to_body_value(&RoutingConfig::default());
        assert_eq!(body, json!({}));
    }

    #[test]
    fn lists_and_policy_round_trip() {
        let body = to_body_value(&RoutingConfig {
            sort: None,
            ignore: vec![UPSTREAM_A.into()],
            only: vec![UPSTREAM_B.into()],
            data_collection: Some(DataCollection::Deny),
        });
        assert_eq!(body["ignore"], json!([UPSTREAM_A]));
        assert_eq!(body["only"], json!([UPSTREAM_B]));
        assert_eq!(body["data_collection"], "deny");
    }

    #[test]
    fn is_empty_only_when_nothing_is_asked_for() {
        assert!(RoutingConfig::default().is_empty());
        assert!(!routing(Some(RoutingSort::Latency)).is_empty());
    }
}
