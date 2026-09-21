//! Tests for the catalogue types: models, providers and tools.

use super::{Model, ModelLimitsSource, Provider};

/// A model entry as the catalogue holds it.
fn entry() -> crate::commands::serve::types::ModelEntry {
    crate::commands::serve::types::ModelEntry {
        id: "gpt-5.6".to_string(),
        provider: "openai".to_string(),
        display_name: Some("GPT-5.6".to_string()),
        max_context_tokens: 400_000,
        max_output_tokens: 64_000,
        limits_source: "api".to_string(),
        supports_tools: true,
        supports_temperature: false,
        learned: true,
        released: Some(1_788_000_000),
        retires: Some("2027-01-01".to_string()),
        pricing: Some(leviath_providers::ModelPricing {
            input_per_mtok: 1.25,
            cached_input_per_mtok: 0.125,
            cache_write_per_mtok: 1.5,
            output_per_mtok: 10.0,
            long_context: None,
            unit: None,
        }),
        input_types: vec!["text/*".to_string(), "image/*".to_string()],
        output_types: vec!["text/*".to_string()],
    }
}

/// Every field of a model entry reaches the schema, prices included.
#[test]
fn a_model_carries_its_limits_and_its_prices() {
    let model = Model::from(&entry());
    assert_eq!(model.id, "gpt-5.6");
    assert_eq!(model.provider, "openai");
    assert_eq!(model.display_name.as_deref(), Some("GPT-5.6"));
    assert_eq!(model.max_context_tokens, 400_000);
    assert_eq!(model.max_output_tokens, 64_000);
    assert_eq!(model.limits_source, ModelLimitsSource::Api);
    assert!(model.supports_tools);
    assert!(!model.supports_temperature);
    assert!(model.learned);
    assert_eq!(model.released.map(|t| t.0), Some(1_788_000_000));
    assert_eq!(model.retires.as_deref(), Some("2027-01-01"));
    let pricing = model.pricing.expect("a priced model");
    assert_eq!(pricing.input_per_mtok.0, 1.25);
    assert_eq!(pricing.cached_input_per_mtok.0, 0.125);
    assert_eq!(pricing.cache_write_per_mtok.0, 1.5);
    assert_eq!(pricing.output_per_mtok.0, 10.0);
    assert_eq!(model.input_types, vec!["text/*", "image/*"]);
}

/// A model nobody has priced says so with an absent price, which is why a run
/// can report an unpriced call rather than a cost of zero.
#[test]
fn an_unpriced_model_has_no_pricing() {
    let mut unpriced = entry();
    unpriced.pricing = None;
    unpriced.display_name = None;
    unpriced.released = None;
    unpriced.retires = None;
    let model = Model::from(&unpriced);
    assert!(model.pricing.is_none());
    assert!(model.display_name.is_none());
    assert!(model.released.is_none());
    assert!(model.retires.is_none());
}

/// Where a model's limits came from, including a word this build does not
/// know: the limits are still the daemon's, and only their provenance is
/// unrecognised.
#[test]
fn every_limits_source_maps_to_one_value() {
    assert_eq!(ModelLimitsSource::from("api"), ModelLimitsSource::Api);
    assert_eq!(
        ModelLimitsSource::from("builtin"),
        ModelLimitsSource::Builtin
    );
    assert_eq!(
        ModelLimitsSource::from("override"),
        ModelLimitsSource::Override
    );
    assert_eq!(
        ModelLimitsSource::from("something-new"),
        ModelLimitsSource::Unknown
    );
}

/// A provider carries what is configured and what is signed in, which are two
/// different questions with two different answers.
#[test]
fn a_provider_tells_configured_from_signed_in() {
    let info = crate::commands::serve::providers::ProviderInfo {
        id: "codex".to_string(),
        display: "Codex".to_string(),
        enabled: true,
        signed_in: false,
        account: Some("someone@example.com".to_string()),
        plan: Some("pro".to_string()),
        expires_at: Some(1_788_000_000),
        signin: None,
        quota: None,
    };
    let provider = Provider::from(&info);
    assert_eq!(provider.id, "codex");
    assert_eq!(provider.display, "Codex");
    assert!(provider.enabled, "turned on in the config");
    assert!(!provider.signed_in, "with no credential stored");
    assert_eq!(provider.account.as_deref(), Some("someone@example.com"));
    assert_eq!(provider.plan.as_deref(), Some("pro"));
    assert_eq!(provider.expires_at.map(|t| t.0), Some(1_788_000_000));

    let bare = crate::commands::serve::providers::ProviderInfo {
        id: "openai".to_string(),
        display: "OpenAI".to_string(),
        enabled: false,
        signed_in: true,
        account: None,
        plan: None,
        expires_at: None,
        signin: None,
        quota: None,
    };
    let provider = Provider::from(&bare);
    assert!(!provider.enabled, "not in the config");
    assert!(provider.signed_in, "and still holding a credential");
    assert!(provider.account.is_none());
    assert!(provider.plan.is_none());
    assert!(provider.expires_at.is_none());
}

/// Each inventory entry reads back as the kind of tool it is, and a script
/// carries the three things only a script has.
///
/// The interface is the point: a client asking for `path` gets a field that is
/// always there, rather than a nullable one it has to test.
#[test]
fn an_inventory_entry_reads_back_as_its_own_kind() {
    use super::Tool;
    use crate::tool_inventory::{ToolEntry, ToolSource};

    let entry = |source, path: Option<&str>| ToolEntry {
        name: "t".to_string(),
        source,
        description: "does a thing".to_string(),
        arguments: serde_json::json!({ "type": "object" }),
        path: path.map(std::path::PathBuf::from),
        agent: Some("coder".to_string()),
        requires: vec!["network".to_string()],
    };

    assert!(matches!(
        Tool::of(entry(ToolSource::Builtin, None)),
        Tool::Builtin(_)
    ));
    assert!(matches!(
        Tool::of(entry(ToolSource::Subagent, None)),
        Tool::Subagent(_)
    ));
    let Tool::Script(script) = Tool::of(entry(ToolSource::Agent, Some("/a/tools/t.rhai"))) else {
        panic!("an agent script is a script tool");
    };
    assert_eq!(script.path, "/a/tools/t.rhai");
    assert_eq!(script.blueprint.as_deref(), Some("coder"));
    assert_eq!(script.requires, vec!["network".to_string()]);
    assert_eq!(script.description, "does a thing");
    assert!(matches!(
        Tool::of(entry(ToolSource::Global, Some("/g/t.rhai"))),
        Tool::Script(_)
    ));

    // A script source with no file cannot come out of discovery, which only
    // makes one from a file it read. Carried as a built-in rather than dropped:
    // a tool missing from the listing is worse than one in the wrong arm.
    assert!(matches!(
        Tool::of(entry(ToolSource::Global, None)),
        Tool::Builtin(_)
    ));
}

/// Every source the inventory can report has an origin here.
///
/// The inventory's own words are what REST carries, so the two lists have to
/// stay the same length: a source with no origin would be a tool this schema
/// could not describe.
#[test]
fn every_tool_source_has_an_origin() {
    use super::ToolOrigin;
    use crate::tool_inventory::ToolSource;
    assert_eq!(ToolOrigin::from(ToolSource::Builtin), ToolOrigin::Builtin);
    assert_eq!(ToolOrigin::from(ToolSource::Subagent), ToolOrigin::Subagent);
    assert_eq!(
        ToolOrigin::from(ToolSource::Agent),
        ToolOrigin::BlueprintScript
    );
    assert_eq!(
        ToolOrigin::from(ToolSource::Global),
        ToolOrigin::GlobalScript
    );
}
