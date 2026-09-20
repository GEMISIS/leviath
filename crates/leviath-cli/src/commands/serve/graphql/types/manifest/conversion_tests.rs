//! The arms one manifest cannot take.
//!
//! Every enum here is a translation of one the daemon owns, and a manifest
//! exercises a few arms at a time: a region is sliding or compacting, never
//! both, and a sandbox is one kind. These go at the conversions directly, so
//! each arm is asserted once rather than a manifest being contorted into
//! reaching it.
//!
//! The point is not the mapping, which is obvious, but that it is total: a
//! daemon that gains a state has to name it here, and an arm that quietly
//! reported the wrong word would read as the feature not working.

use super::super::blueprint::HintSetting;
use super::interaction::{InteractionPointStyle, UnattendedPolicy};
use super::model::MaxOutputTokens;
use super::output::ValidatorErrorPolicy;
use super::region::{
    RegionAdmission, RegionEviction, RegionStrategy, RegionVolatility, SeedRefresh,
};
use super::runtime::{
    BlueprintSecurity, NudgePolicy, SandboxKind, SandboxUnavailable, TaintTracking,
    WorkerFailurePolicy,
};
use super::tools::{ToolPermissionRule, ToolRouting};
use super::transition::{MappingTransform, TransitionCondition, TransitionTransform};

/// Every condition an edge can carry has its own value.
#[test]
fn every_transition_condition_is_translated() {
    use leviath_core::blueprint::TransitionCondition as Core;

    let cases = [
        (Core::Always, TransitionCondition::Always),
        (Core::LlmChoice, TransitionCondition::LlmChoice),
        (Core::Error, TransitionCondition::Error),
        (Core::MaxIterations, TransitionCondition::MaxIterations),
        (Core::Stuck, TransitionCondition::Stuck),
        (Core::DeadEnd, TransitionCondition::DeadEnd),
    ];
    for (core, expected) in cases {
        assert_eq!(TransitionCondition::from(&core), expected, "{core:?}");
    }
}

/// And every transform, including the two that carry nothing.
#[test]
fn every_edge_transform_is_translated() {
    use leviath_core::blueprint::EdgeTransform as Core;

    assert_eq!(
        TransitionTransform::from(&Core::Direct),
        TransitionTransform::Direct
    );
    assert_eq!(
        TransitionTransform::from(&Core::Clear),
        TransitionTransform::Clear
    );
    assert_eq!(
        TransitionTransform::from(&Core::Compact { prompt: None }),
        TransitionTransform::Compact
    );
    assert_eq!(
        TransitionTransform::from(&Core::Custom {
            carry: Vec::new(),
            compact: Vec::new(),
            clear: Vec::new(),
            compact_prompt: None,
        }),
        TransitionTransform::Custom
    );
}

/// A gate carries every requirement it was given, including the ones a manifest
/// rarely sets together.
#[test]
fn a_gate_carries_every_requirement() {
    let gate = leviath_core::blueprint::TransitionGate {
        require_modifications: true,
        message: Some("write it first".to_string()),
        region: Some("plan".to_string()),
        tools: vec!["write_note".to_string()],
        max_attempts: Some(2),
        require_region_updated: Some("plan".to_string()),
        require_regions: vec!["plan".to_string(), "notes".to_string()],
        require_no_open_items: Some("todo".to_string()),
        require_region_entries: Some(leviath_core::blueprint::RegionCount {
            region: "views".to_string(),
            at_least: 4,
        }),
    };
    let mapped = super::transition::TransitionGate::from(&gate);
    assert!(mapped.require_modifications);
    assert_eq!(mapped.region.as_deref(), Some("plan"));
    assert_eq!(mapped.tools, vec!["write_note".to_string()]);
    assert_eq!(mapped.require_regions.len(), 2);
    assert_eq!(mapped.require_region_updated.as_deref(), Some("plan"));
    assert_eq!(mapped.require_no_open_items.as_deref(), Some("todo"));
    let counted = mapped.require_region_entries.expect("a count");
    assert_eq!(counted.region, "views");
    assert_eq!(counted.at_least, 4);
    assert_eq!(mapped.max_attempts, Some(2));
}

/// A handoff's mappings carry each content transform, and `EXTRACT` carries the
/// fields it keeps.
#[test]
fn every_mapping_transform_is_translated() {
    use leviath_core::blueprint::{ContentTransform, ContextTransform, RegionMapping};

    let transform = ContextTransform {
        from_blueprint: "a".to_string(),
        to_blueprint: "b".to_string(),
        mappings: vec![
            RegionMapping {
                from_region: "one".to_string(),
                to_region: "one".to_string(),
                transform: Some(ContentTransform::Direct),
            },
            RegionMapping {
                from_region: "two".to_string(),
                to_region: "two".to_string(),
                transform: Some(ContentTransform::Summarize),
            },
            RegionMapping {
                from_region: "three".to_string(),
                to_region: "three".to_string(),
                transform: Some(ContentTransform::Extract {
                    fields: vec!["title".to_string()],
                }),
            },
        ],
    };
    let mapped = super::transition::ContextTransform::from(&transform);
    assert_eq!(mapped.from_blueprint, "a");
    assert_eq!(mapped.mappings[0].transform, Some(MappingTransform::Direct));
    assert_eq!(
        mapped.mappings[1].transform,
        Some(MappingTransform::Summarize)
    );
    assert_eq!(
        mapped.mappings[2].transform,
        Some(MappingTransform::Extract)
    );
    assert_eq!(mapped.mappings[2].fields, vec!["title".to_string()]);
    // The fields belong to the extract and nowhere else: a client reading them
    // off a direct mapping would be reading a list nobody wrote.
    assert!(mapped.mappings[0].fields.is_empty());
}

/// A checkpoint's two enums, both arms each.
#[test]
fn a_checkpoints_words_are_translated() {
    use leviath_core::blueprint::{InteractionStyle, UnattendedPolicy as Core};

    assert_eq!(
        UnattendedPolicy::from(Core::AutoApprove),
        UnattendedPolicy::AutoApprove
    );
    assert_eq!(UnattendedPolicy::from(Core::Ask), UnattendedPolicy::Ask);
    assert_eq!(
        InteractionPointStyle::from(&InteractionStyle::FreeText),
        InteractionPointStyle::FreeText
    );
    assert_eq!(
        InteractionPointStyle::from(&InteractionStyle::MultipleChoice),
        InteractionPointStyle::MultipleChoice
    );
    assert_eq!(
        InteractionPointStyle::from(&InteractionStyle::Confirm),
        InteractionPointStyle::Confirm
    );
}

/// A checkpoint with no directives carries none, rather than an empty entry.
#[test]
fn a_checkpoint_with_no_directives_carries_none() {
    let point = leviath_core::blueprint::InteractionPoint {
        name: "review".to_string(),
        prompt: "ok?".to_string(),
        required: true,
        unattended: leviath_core::blueprint::UnattendedPolicy::AutoApprove,
        style: leviath_core::blueprint::InteractionStyle::Confirm,
        options: Vec::new(),
        directives: std::collections::HashMap::new(),
        abort_options: Vec::new(),
        edit_options: Vec::new(),
        document_region: None,
    };
    let mapped = super::interaction::InteractionPoint::from(&point);
    assert_eq!(mapped.name, "review");
    assert!(mapped.directives.is_empty());
    assert!(mapped.document_region.is_none());
}

/// Every region policy word, and the numbers each eviction strategy carries.
#[test]
fn every_region_policy_is_translated() {
    use leviath_core::region::{Admission, EvictionStrategy, Volatility};

    assert_eq!(
        RegionVolatility::from(Volatility::Stable),
        RegionVolatility::Stable
    );
    assert_eq!(
        RegionVolatility::from(Volatility::Grows),
        RegionVolatility::Grows
    );
    assert_eq!(
        RegionVolatility::from(Volatility::Rewritten),
        RegionVolatility::Rewritten
    );
    assert_eq!(
        RegionAdmission::from(Admission::Evict),
        RegionAdmission::Evict
    );
    assert_eq!(
        RegionAdmission::from(Admission::Reject),
        RegionAdmission::Reject
    );

    let per_item = RegionEviction::from(EvictionStrategy::PerItem);
    assert_eq!(per_item.strategy, RegionStrategy::PerItem);
    assert!(per_item.overflow.is_none() && per_item.compact_count.is_none());
    let bulk = RegionEviction::from(EvictionStrategy::Bulk { overflow: 3 });
    assert_eq!(bulk.strategy, RegionStrategy::Bulk);
    assert_eq!(bulk.overflow, Some(3));
    let compacting = RegionEviction::from(EvictionStrategy::Compact { compact_count: 4 });
    assert_eq!(compacting.strategy, RegionStrategy::Compact);
    assert_eq!(compacting.compact_count, Some(4));

    assert_eq!(
        SeedRefresh::from(leviath_core::layout::SeedRefresh::Once),
        SeedRefresh::Once
    );
    assert_eq!(
        SeedRefresh::from(leviath_core::layout::SeedRefresh::EachStage),
        SeedRefresh::EachStage
    );
}

/// The settings that cascade, each state named for its effect rather than for
/// on and off.
#[test]
fn every_cascading_setting_is_translated() {
    assert_eq!(HintSetting::from(None), HintSetting::Inherit);
    assert_eq!(HintSetting::from(Some(true)), HintSetting::Include);
    assert_eq!(HintSetting::from(Some(false)), HintSetting::Omit);

    assert_eq!(NudgePolicy::from(None), NudgePolicy::Inherit);
    assert_eq!(NudgePolicy::from(Some(true)), NudgePolicy::Nudge);
    assert_eq!(NudgePolicy::from(Some(false)), NudgePolicy::NeverNudge);

    // A manifest can ask for tracking and cannot ask for less, so the absence of
    // a request is inheritance rather than a refusal.
    let tracked = BlueprintSecurity::from(&leviath_core::taint::SecurityConfig {
        taint_tracking: true,
    });
    assert_eq!(tracked.taint_tracking, TaintTracking::Track);
    let silent = BlueprintSecurity::from(&leviath_core::taint::SecurityConfig {
        taint_tracking: false,
    });
    assert_eq!(silent.taint_tracking, TaintTracking::Inherit);
}

/// Every sandbox kind and both answers to a sandbox that cannot be built.
#[test]
fn every_sandbox_word_is_translated() {
    use leviath_core::sandbox::{OnUnavailable, SandboxKind as Core};

    assert_eq!(SandboxKind::from(Core::None), SandboxKind::None);
    assert_eq!(SandboxKind::from(Core::Namespace), SandboxKind::Namespace);
    assert_eq!(SandboxKind::from(Core::Container), SandboxKind::Container);
    assert_eq!(
        SandboxUnavailable::from(OnUnavailable::Error),
        SandboxUnavailable::Error
    );
    assert_eq!(
        SandboxUnavailable::from(OnUnavailable::Warn),
        SandboxUnavailable::Warn
    );
}

/// Both answers to a worker that failed, and both to a validator that refused.
#[test]
fn the_failure_policies_are_translated() {
    use leviath_core::blueprint::WorkerFailurePolicy as Core;
    use leviath_core::output::OnValidatorError;

    assert_eq!(
        WorkerFailurePolicy::from(&Core::Continue),
        WorkerFailurePolicy::Continue
    );
    assert_eq!(
        WorkerFailurePolicy::from(&Core::FailAll),
        WorkerFailurePolicy::FailAll
    );
    assert_eq!(
        ValidatorErrorPolicy::from(OnValidatorError::Reject),
        ValidatorErrorPolicy::Reject
    );
    assert_eq!(
        ValidatorErrorPolicy::from(OnValidatorError::Accept),
        ValidatorErrorPolicy::Accept
    );
}

/// Each shape an output cap can take is its own type, with its number.
#[test]
fn every_output_cap_shape_is_translated() {
    use leviath_core::blueprint::OutputCap;

    match MaxOutputTokens::from(OutputCap::Tokens(8_000)) {
        MaxOutputTokens::Count(count) => assert_eq!(count.tokens, 8_000),
        other => panic!("a token count is a count: {other:?}"),
    }
    match MaxOutputTokens::from(OutputCap::WindowPercent(0.4)) {
        MaxOutputTokens::ContextPercent(share) => assert!((share.percent - 40.0).abs() < 1e-9),
        other => panic!("a window share is a share: {other:?}"),
    }
    match MaxOutputTokens::from(OutputCap::RegionPercent {
        percent: 1.0,
        region: "claims".to_string(),
    }) {
        MaxOutputTokens::RegionPercent(share) => {
            assert!((share.percent - 100.0).abs() < 1e-9);
            assert_eq!(share.region, "claims");
        }
        other => panic!("a region share names its region: {other:?}"),
    }
}

/// A permission table comes back sorted, and a word the daemon does not know
/// grants nothing rather than something.
#[test]
fn a_permission_table_is_sorted_and_honest() {
    let table = std::collections::HashMap::from([
        ("shell".to_string(), "deny".to_string()),
        ("read_file".to_string(), "allow".to_string()),
        ("ask_user_text".to_string(), "ask".to_string()),
        ("write_file".to_string(), "sideways".to_string()),
    ]);
    let rules = ToolPermissionRule::from_table(&table);
    let tools: Vec<&str> = rules.iter().map(|rule| rule.tool.as_str()).collect();
    assert_eq!(tools, ["ask_user_text", "read_file", "shell", "write_file"]);
    assert!(rules[3].policy.is_none(), "a word that is not a policy");
    assert!(rules[0].policy.is_some());
}

/// Tool routing carries its overrides and ceilings, each sorted by tool.
#[test]
fn tool_routing_carries_its_tables() {
    let routing = leviath_core::blueprint::ToolResultRouting {
        default_region: "tool_results".to_string(),
        tool_overrides: std::collections::HashMap::from([
            ("shell".to_string(), "logs".to_string()),
            ("read_file".to_string(), "files".to_string()),
        ]),
        persist: true,
        max_result_tokens: Some(4_000),
        tool_max_result_tokens: std::collections::HashMap::from([
            ("shell".to_string(), 500usize),
            ("read_file".to_string(), 2_000usize),
        ]),
    };
    let mapped = ToolRouting::from(&routing);
    assert_eq!(mapped.default_region, "tool_results");
    assert_eq!(mapped.max_result_tokens, Some(4_000));
    let overridden: Vec<&str> = mapped
        .overrides
        .iter()
        .map(|entry| entry.tool.as_str())
        .collect();
    assert_eq!(overridden, ["read_file", "shell"]);
    let ceilings: Vec<&str> = mapped
        .max_result_tokens_per_tool
        .iter()
        .map(|entry| entry.tool.as_str())
        .collect();
    assert_eq!(ceilings, ["read_file", "shell"]);
}

/// A shipped MCP server's transport is read where it is a word the daemon knows,
/// and left out where it is not.
#[test]
fn a_server_templates_transport_is_read_or_left_out() {
    use super::dependency::{McpServerTemplate, McpTransport};

    let template = |transport: Option<&str>| leviath_core::blueprint::McpServerTemplate {
        transport: transport.map(str::to_string),
        command: Some("docs-mcp".to_string()),
        url: None,
        args: Vec::new(),
        headers: std::collections::BTreeMap::new(),
        env: std::collections::BTreeMap::new(),
    };
    assert_eq!(
        McpServerTemplate::from(&template(Some("stdio"))).transport,
        Some(McpTransport::Stdio)
    );
    assert_eq!(
        McpServerTemplate::from(&template(Some("http"))).transport,
        Some(McpTransport::Http)
    );
    // A word neither end knows is left out rather than guessed: the installer
    // infers the transport from the command or the URL anyway.
    assert!(
        McpServerTemplate::from(&template(Some("carrier-pigeon")))
            .transport
            .is_none()
    );
    assert!(McpServerTemplate::from(&template(None)).transport.is_none());
}

/// A mime row that will not read is left out rather than reported empty.
///
/// An empty row would read as a row that sets nothing, which is a different
/// claim from "this is not a row".
#[test]
fn a_mime_row_that_will_not_read_is_left_out() {
    let table: toml::Table =
        toml::from_str("[\"image/png\"]\nfamily = \"image\"\n\n[\"broken/thing\"]\nfamily = 7\n")
            .expect("the table parses");
    let rows = super::mime::BlueprintMimeRow::from_table(&table);
    assert_eq!(rows.len(), 1, "only the row that reads: {rows:?}");
    assert_eq!(rows[0].mime_type, "image/png");
}

/// A checkpoint's directives come back in a fixed order.
///
/// The manifest holds them in a hash map, so two reads of one blueprint would
/// otherwise disagree about the order, and a console diffing them would show
/// changes nobody made.
#[test]
fn a_checkpoints_directives_are_ordered() {
    let point = leviath_core::blueprint::InteractionPoint {
        name: "review".to_string(),
        prompt: "ok?".to_string(),
        required: true,
        unattended: leviath_core::blueprint::UnattendedPolicy::AutoApprove,
        style: leviath_core::blueprint::InteractionStyle::MultipleChoice,
        options: vec!["ship".to_string(), "hold".to_string()],
        directives: [
            ("ship".to_string(), "go to the next stage".to_string()),
            ("hold".to_string(), "ask again later".to_string()),
            ("abort".to_string(), "stop the run".to_string()),
        ]
        .into_iter()
        .collect(),
        abort_options: Vec::new(),
        edit_options: Vec::new(),
        document_region: None,
    };
    let mapped = super::interaction::InteractionPoint::from(&point);
    let options: Vec<&str> = mapped
        .directives
        .iter()
        .map(|entry| entry.option.as_str())
        .collect();
    assert_eq!(options, vec!["abort", "hold", "ship"], "by option, always");
}
