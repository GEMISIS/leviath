//! The parsed blueprint's own checks and lookups, on blueprints built in
//! code: region references, dependencies, stage inputs, hooks and routing.

use std::collections::{BTreeMap, HashMap};

use leviath_core::RegionKind;
use leviath_core::mime::MimeType;
use serde_json::json;

use super::*;
use crate::spec::layout::{ContextLayout, RegionDefinition, SeedToolCall};

fn region(name: &str, kind: RegionKind) -> RegionDefinition {
    RegionDefinition::new(name.to_string(), kind, 100)
}

fn stage(name: &str) -> Stage {
    Stage::new(
        name.to_string(),
        ModelConfig::new("p".to_string(), "m".to_string()),
    )
}

/// A blueprint over `stages` whose layout holds `notes` (pinned), `todo` (a
/// checklist) and `conversation`.
fn blueprint(stages: Vec<Stage>) -> Blueprint {
    Blueprint::new(
        "checked".to_string(),
        String::new(),
        stages,
        ContextLayout::new(
            vec![
                region("notes", RegionKind::Pinned),
                region("todo", RegionKind::Checklist),
                region(
                    "conversation",
                    RegionKind::SlidingWindow {
                        max_items: 10,
                        eviction_strategy: Default::default(),
                    },
                ),
            ],
            300,
        ),
    )
}

/// What `validate` says about a blueprint of `main` then `end` whose first
/// stage `edit` changes.
fn refusal(edit: impl FnOnce(&mut Stage)) -> String {
    let mut main = stage("main");
    edit(&mut main);
    blueprint(vec![main, stage("end")])
        .validate()
        .expect_err("the blueprint is refused")
        .to_string()
}

/// Give the stage an edge to `end` that `gate` holds.
fn gated(gate: TransitionGate) -> impl FnOnce(&mut Stage) {
    move |s: &mut Stage| {
        s.transitions = Some(HashMap::from([(
            "end".to_string(),
            TransitionEdge {
                target: "end".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::default(),
                gate: Some(gate),
                stuck: None,
            },
        )]));
    }
}

#[test]
fn every_rescan_setting_reads_back_from_its_word() {
    for setting in ToolRescan::ALL {
        assert_eq!(ToolRescan::parse(setting.wire()), Some(setting));
    }
    assert_eq!(ToolRescan::parse("sometimes"), None);
    assert!(!ToolRescan::AtSpawn.rescans());
    assert!(ToolRescan::AfterWrites.rescans());
    assert!(!ToolRescan::AfterWrites.before_dispatch());
    assert!(ToolRescan::BeforeDispatch.before_dispatch());
}

/// A region name that matches nothing, wherever a stage names one, is
/// refused with where it was named.
#[test]
fn a_region_name_that_matches_nothing_is_refused() {
    let hide = |names: &[&str]| {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        move |s: &mut Stage| s.context_hide = names
    };
    assert!(refusal(hide(&["conversation"])).contains("cannot hide"));
    assert!(refusal(hide(&["ghost"])).contains("context.hide names region 'ghost'"));
    let reset = refusal(|s| s.context_reset = vec!["ghost".to_string()]);
    assert!(
        reset.contains("context.reset names region 'ghost'"),
        "{reset}"
    );
    let routed = refusal(|s| {
        s.output_routing = BTreeMap::from([("image/*".to_string(), "ghost".to_string())]);
    });
    assert!(routed.contains("output_routing.\"image/*\""), "{routed}");
    let gate = refusal(gated(TransitionGate {
        region: Some("ghost".to_string()),
        ..TransitionGate::default()
    }));
    assert!(gate.contains("gate.region names region 'ghost'"), "{gate}");
    let open = refusal(gated(TransitionGate {
        require_no_open_items: Some("notes".to_string()),
        ..TransitionGate::default()
    }));
    assert!(open.contains("is not a checklist region"), "{open}");
}

/// Tool results routed somewhere the stage cannot read are refused, for the
/// default region and for a single tool's override alike.
#[test]
fn tool_results_routed_out_of_sight_are_refused() {
    let routing = |default_region: &str, overrides: &[(&str, &str)]| ToolResultRouting {
        default_region: default_region.to_string(),
        tool_overrides: overrides
            .iter()
            .map(|(t, r)| (t.to_string(), r.to_string()))
            .collect(),
        ..ToolResultRouting::default()
    };
    let hidden = refusal(|s| {
        s.context_hide = vec!["notes".to_string()];
        s.tool_result_routing = Some(routing("notes", &[]));
    });
    assert!(hidden.contains("tool_routing.default_region"), "{hidden}");
    let overridden = refusal(|s| {
        s.context_hide = vec!["notes".to_string()];
        s.tool_result_routing = Some(routing("conversation", &[("read_file", "notes")]));
    });
    assert!(
        overridden.contains("tool_routing.overrides.read_file"),
        "{overridden}"
    );
}

/// A blueprint whose every reference holds validates, including a region
/// only a stage's own layout declares and a checklist gate on it.
#[test]
fn references_that_hold_validate() {
    let mut main = stage("main");
    main.context_layout = Some(ContextLayout::new(
        vec![
            region("steps", RegionKind::Checklist),
            region("notes", RegionKind::Pinned),
        ],
        200,
    ));
    main.tool_result_routing = Some(ToolResultRouting {
        default_region: "notes".to_string(),
        tool_overrides: HashMap::from([("read_file".to_string(), "steps".to_string())]),
        ..ToolResultRouting::default()
    });
    main.output_routing = BTreeMap::from([("image/*".to_string(), "todo".to_string())]);
    main.context_reset = vec!["notes".to_string()];
    gated(TransitionGate {
        region: Some("notes".to_string()),
        require_no_open_items: Some("steps".to_string()),
        require_region_entries: Some(RegionCount {
            region: "todo".to_string(),
            at_least: 1,
        }),
        ..TransitionGate::default()
    })(&mut main);
    let mut end = stage("end");
    end.context_hide = vec!["steps".to_string()];
    blueprint(vec![main, end]).validate().unwrap();
}

/// Each `[[dependencies]]` entry from JSON, as a parsed manifest holds it.
fn dependencies(entries: serde_json::Value) -> Vec<Dependency> {
    serde_json::from_value(entries).expect("the test dependencies read")
}

fn dependency_refusal(entries: serde_json::Value) -> String {
    let mut bp = blueprint(vec![stage("main")]);
    bp.dependencies = dependencies(entries);
    bp.validate()
        .expect_err("the dependency is refused")
        .to_string()
}

#[test]
fn a_dependency_is_required_unless_it_says_otherwise() {
    let deps = dependencies(json!([
        { "name": "git", "kind": "binary", "command": "git" },
        { "name": "key", "kind": "env", "var": "KEY", "required": false },
    ]));
    assert!(deps[0].required);
    assert!(!deps[1].required);
}

#[test]
fn every_malformed_dependency_is_refused() {
    let cases = [
        (
            json!([{ "name": " ", "kind": "env", "var": "K" }]),
            "non-empty name",
        ),
        (
            json!([
                { "name": "k", "kind": "env", "var": "K" },
                { "name": "k", "kind": "env", "var": "J" },
            ]),
            "share this name",
        ),
        (
            json!([{ "name": "s", "kind": "mcp_server", "server": "" }]),
            "'server'",
        ),
        (json!([{ "name": "e", "kind": "env", "var": " " }]), "'var'"),
        (
            json!([{ "name": "b", "kind": "binary", "command": "" }]),
            "'command'",
        ),
        (
            json!([{ "name": "c", "kind": "script", "check": "" }]),
            "'check'",
        ),
        (
            json!([{ "name": "b", "kind": "binary", "command": "git",
                     "install": { "server": { "command": "x" } } }]),
            "only valid for a 'mcp_server'",
        ),
        (
            json!([{ "name": "s", "kind": "mcp_server", "server": "web",
                     "install": { "server": { "transport": "smoke" } } }]),
            "transport must be",
        ),
        (
            json!([{ "name": "b", "kind": "binary", "command": "git",
                     "install": { "commands": { "beos": "x" } } }]),
            "key 'beos'",
        ),
    ];
    for (entries, says) in cases {
        let err = dependency_refusal(entries);
        assert!(err.contains(says), "{says}: {err}");
    }
    let mut bp = blueprint(vec![stage("main")]);
    bp.dependencies = dependencies(json!([
        { "name": "web", "kind": "mcp_server", "server": "web",
          "install": { "server": { "transport": "http", "url": "https://x" } } },
        { "name": "git", "kind": "binary", "command": "git",
          "install": { "commands": { "macos": "brew install git" } } },
        { "name": "check", "kind": "script", "check": "check.rhai" },
    ]));
    bp.validate().unwrap();
}

/// A stage's own `[input] accepts` is what it takes; otherwise it takes what
/// the regions it sees accept, any type for a region that names none, and
/// never lists text.
#[test]
fn a_stage_takes_what_its_input_or_its_regions_accept() {
    let mut images = region("images", RegionKind::Pinned);
    images.accepts = vec!["image/*".to_string(), "text/markdown".to_string()];
    let mut docs = region("docs", RegionKind::Pinned);
    docs.accepts = vec!["image/*".to_string()];
    let mut main = stage("main");
    main.context_layout = Some(ContextLayout::new(
        vec![
            images,
            docs,
            region("open", RegionKind::Pinned),
            region("secret", RegionKind::Pinned),
        ],
        400,
    ));
    main.context_hide = vec!["secret".to_string()];
    let bp = blueprint(vec![main.clone()]);
    assert_eq!(bp.stage_inputs(&main), ["image/*", "*/*"]);
    main.input_accepts = vec!["audio/*".to_string()];
    assert_eq!(bp.stage_inputs(&main), ["audio/*"]);
}

#[test]
fn a_stage_lists_its_hooks_in_firing_order() {
    let mut hooks = StageHooks::default();
    assert!(hooks.declared().is_empty());
    hooks.on_stage_enter = Some("a.rhai".to_string());
    hooks.on_stage_exit = Some("b.rhai".to_string());
    hooks.before_inference = Some("c.rhai".to_string());
    hooks.after_inference = Some("d.rhai".to_string());
    hooks.on_tool_call = Some("e.rhai".to_string());
    hooks.on_completion = Some("f.rhai".to_string());
    hooks.on_error = Some("g.rhai".to_string());
    let names: Vec<&str> = hooks.declared().into_iter().map(|(hook, _)| hook).collect();
    assert_eq!(
        names,
        [
            "on_stage_enter",
            "on_stage_exit",
            "before_inference",
            "after_inference",
            "on_tool_call",
            "on_completion",
            "on_error",
        ]
    );
}

/// The most specific matching routing pattern wins, and a stage that limits
/// a tool's inputs says so for that tool alone.
#[test]
fn a_stage_routes_parts_and_limits_tools_by_mime_type() {
    let mut main = stage("main");
    main.output_routing = BTreeMap::from([
        ("*/*".to_string(), "anything".to_string()),
        ("image/*".to_string(), "images".to_string()),
        ("image/png".to_string(), "pngs".to_string()),
    ]);
    let route = |mime: &str| main.route_for_mime(&MimeType::parse(mime).unwrap());
    assert_eq!(route("image/png"), Some("pngs"));
    assert_eq!(route("image/jpeg"), Some("images"));
    assert_eq!(route("application/pdf"), Some("anything"));
    main.tool_accepts = BTreeMap::from([("read_file".to_string(), vec!["text/*".to_string()])]);
    assert_eq!(
        main.tool_limit("read_file"),
        Some(&["text/*".to_string()][..])
    );
    assert_eq!(main.tool_limit("bash"), None);
}

#[test]
fn a_seed_tool_call_carries_its_arguments() {
    let call = SeedToolCall::with_args("read_file", json!({"path": "a"}));
    assert_eq!(call.name, "read_file");
    assert_eq!(call.args["path"], "a");
}
