//! Tests for the manifest parser.
//!
//! A sibling file rather than an inline `mod tests`: this module is 3,761
//! lines against 1,533 of parser, and keeping the two in one file made
//! `manifest.rs` read as a 5,000-line monster when the parser itself is a
//! quarter of that.

use super::*;
use crate::layout::SeedToolCall;

// ─── [stages.<name>.hooks] ────────────────────────────────────────────

fn stage_with_hooks(body: &str) -> Result<crate::Stage> {
    let toml_src = format!("[stages.main]\n{body}\n");
    let parsed: toml::Value = toml::from_str(&toml_src).expect("fixture parses");
    let stage_value = parsed
        .get("stages")
        .and_then(|v| v.get("main"))
        .expect("fixture shape");
    parse_stage("main", stage_value)
}

#[test]
fn a_stage_declaring_no_hooks_has_none() {
    let stage = stage_with_hooks("mode = \"autonomous\"").expect("parses");
    assert!(stage.hooks.is_empty());
    assert!(stage.hooks.declared().is_empty());
}

/// Every hook this build implements, in one fixture: each parses, and each
/// reports the function it backs. A hook that parsed but never appeared in
/// `declared()` would be resolved at spawn and then never called.
#[test]
fn every_hook_parses_and_reports_the_function_it_backs() {
    let stage = stage_with_hooks(
        "[stages.main.hooks]\n\
         on_stage_enter = \"a.rhai\"\n\
         on_stage_exit = \"b.rhai\"\n\
         before_inference = \"c.rhai\"\n\
         after_inference = \"d.rhai\"\n\
         on_tool_call = \"e.rhai\"\n\
         on_completion = \"f.rhai\"\n\
         on_error = \"g.rhai\"",
    )
    .expect("parses");
    assert!(!stage.hooks.is_empty());
    assert_eq!(
        stage.hooks.declared(),
        vec![
            ("on_stage_enter", "a.rhai"),
            ("on_stage_exit", "b.rhai"),
            ("before_inference", "c.rhai"),
            ("after_inference", "d.rhai"),
            ("on_tool_call", "e.rhai"),
            ("on_completion", "f.rhai"),
            ("on_error", "g.rhai"),
        ]
    );
}

/// Each hook alone also leaves `is_empty` false - a stage declaring only
/// the newest hook must still get its scripts resolved.
#[test]
fn any_single_hook_makes_the_stage_hooked() {
    for field in [
        "on_stage_enter",
        "on_stage_exit",
        "before_inference",
        "after_inference",
        "on_tool_call",
        "on_completion",
        "on_error",
    ] {
        let stage = stage_with_hooks(&format!("[stages.main.hooks]\n{field} = \"h.rhai\""))
            .expect("parses");
        assert!(!stage.hooks.is_empty(), "{field}");
        assert_eq!(stage.hooks.declared(), vec![(field, "h.rhai")], "{field}");
    }
}

#[test]
fn one_file_may_back_both_hooks() {
    let stage = stage_with_hooks(
        "[stages.main.hooks]\non_stage_enter = \"h.rhai\"\non_stage_exit = \"h.rhai\"",
    )
    .expect("parses");
    let paths: Vec<&str> = stage.hooks.declared().iter().map(|(_, p)| *p).collect();
    assert_eq!(paths, vec!["h.rhai", "h.rhai"]);
}

/// An unrecognised hook is refused rather than ignored. A blueprint writing
/// `on_stage_entry` has asked for behaviour it would silently not get, and
/// a hook that never runs reads exactly like one that ran and did nothing.
#[test]
fn an_unknown_hook_name_is_refused_and_lists_the_real_ones() {
    let err = stage_with_hooks("[stages.main.hooks]\non_stage_entry = \"a.rhai\"")
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown hook 'on_stage_entry'"), "{err}");
    assert!(err.contains("on_stage_enter"), "{err}");
}

#[test]
fn a_hook_that_is_not_a_path_is_refused() {
    let err = stage_with_hooks("[stages.main.hooks]\non_stage_enter = 42")
        .unwrap_err()
        .to_string();
    assert!(err.contains("must be a path"), "{err}");
}

/// Extract the `points` vec from a `StageMode::InteractivePoints`.
/// Panics (with a diagnostic) when the mode is any other variant.
/// The panic branch is exercised by `unwrap_interactive_points_panics_on_wrong_mode`.
fn unwrap_interactive_points(mode: &StageMode) -> &[crate::blueprint::InteractionPoint] {
    match mode {
        StageMode::InteractivePoints { points } => points,
        other => panic!(
            "expected StageMode::InteractivePoints, got {:?}",
            std::mem::discriminant(other)
        ),
    }
}

#[test]
#[should_panic(expected = "expected StageMode::InteractivePoints")]
fn unwrap_interactive_points_panics_on_wrong_mode() {
    let mode = StageMode::Autonomous;
    let _ = unwrap_interactive_points(&mode);
}

// ─── parse_manifest ──────────────────────────────────────────────────────

#[test]
fn parse_minimal_manifest() {
    let toml = r#"
[agent]
name = "test-agent"
"#;
    let bp = parse_manifest(toml).unwrap();
    assert_eq!(bp.name, "test-agent");
    assert_eq!(bp.version, "0.1.0"); // default
    assert_eq!(bp.stages.len(), 1); // default main stage
    assert_eq!(bp.stages[0].name, "main");
}

#[test]
fn parse_full_manifest_with_all_fields() {
    let toml = r#"
[agent]
name = "full-agent"
version = "2.0.0"
description = "A fully configured agent"
max_child_depth = 3
entry_stage = "start"
dynamic_tools = true

[stages.start]
mode = "autonomous"
model = { provider = "openai", model = "gpt-5" }
max_iterations = 25
available_tools = ["read_file", "bash"]
system_prompt = "You are a coding assistant."
requires_children = true
max_revisits = 5

[stages.start.tool_permissions]
bash = "ask"
read_file = "allow"

[stages.finish]
mode = "interactive"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }

[context.regions]
system = { kind = "pinned", max_tokens = 2000 }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#;
    let bp = parse_manifest(toml).unwrap();
    assert_eq!(bp.name, "full-agent");
    assert_eq!(bp.version, "2.0.0");
    assert_eq!(bp.description, "A fully configured agent");
    assert_eq!(bp.max_child_depth, Some(3));
    assert_eq!(bp.entry_stage, Some("start".to_string()));
    assert!(bp.dynamic_tools);
    assert_eq!(bp.stages.len(), 2);

    let start = bp.find_stage("start").unwrap();
    assert_eq!(start.mode, StageMode::Autonomous);
    assert_eq!(start.model.provider(), "openai");
    assert_eq!(start.model.model(), "gpt-5");
    assert_eq!(start.max_iterations, Some(25));
    assert_eq!(start.available_tools, vec!["read_file", "bash"]);
    assert!(start.requires_children);
    assert_eq!(start.max_revisits, Some(5));
    assert_eq!(
        start.tool_permissions.get("bash").map(|s| s.as_str()),
        Some("ask")
    );
    assert_eq!(
        start.tool_permissions.get("read_file").map(|s| s.as_str()),
        Some("allow")
    );

    let finish = bp.find_stage("finish").unwrap();
    assert_eq!(finish.mode, StageMode::Interactive);
}

#[test]
fn parse_manifest_with_graph_transitions() {
    let toml = r#"
[agent]
name = "graph-agent"

[stages.analyze]
mode = "autonomous"
transition_prompt = "Pick the next stage"

[stages.analyze.transitions.implement]
condition = "always"
hint = "Ready to implement"
transform = "direct"

[stages.analyze.transitions.error_handler]
condition = "error"
transform = "clear"

[stages.analyze.transitions.timeout_handler]
condition = "max_iterations"
transform = "compact"

[stages.analyze.transitions.choice_stage]
condition = "llm_choice"
hint = "LLM chooses this"

[stages.implement]
mode = "autonomous"

[stages.error_handler]
mode = "autonomous"

[stages.timeout_handler]
mode = "autonomous"

[stages.choice_stage]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    let analyze = bp.find_stage("analyze").unwrap();
    assert_eq!(
        analyze.transition_prompt,
        Some("Pick the next stage".to_string())
    );
    let transitions = analyze.transitions.as_ref().unwrap();
    assert_eq!(transitions.len(), 4);

    let impl_edge = transitions.get("implement").unwrap();
    assert_eq!(impl_edge.condition, TransitionCondition::Always);
    assert_eq!(impl_edge.hint.as_deref(), Some("Ready to implement"));
    assert_eq!(impl_edge.transform, EdgeTransform::Direct);

    let err_edge = transitions.get("error_handler").unwrap();
    assert_eq!(err_edge.condition, TransitionCondition::Error);
    assert_eq!(err_edge.transform, EdgeTransform::Clear);

    let timeout_edge = transitions.get("timeout_handler").unwrap();
    assert_eq!(timeout_edge.condition, TransitionCondition::MaxIterations);
    assert_eq!(
        timeout_edge.transform,
        EdgeTransform::Compact { prompt: None }
    );

    let choice_edge = transitions.get("choice_stage").unwrap();
    assert_eq!(choice_edge.condition, TransitionCondition::LlmChoice);
}

#[test]
fn parse_manifest_rejects_unknown_transition_condition() {
    let toml = r#"
[agent]
name = "bad-cond"

[stages.analyze]
mode = "autonomous"

[stages.analyze.transitions.next]
condition = "whenever_i_feel_like_it"

[stages.next]
mode = "autonomous"
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("unknown condition"), "got: {err}");
    assert!(err.contains("whenever_i_feel_like_it"), "got: {err}");
    // The rejection message must list every condition that IS valid.
    assert!(err.contains("stuck"), "got: {err}");
    assert!(err.contains("dead_end"), "got: {err}");
}

/// `condition = "dead_end"` parses. It is the escape the `dead-end-possible`
/// lint recommends, so a manifest that follows the advice has to load.
#[test]
fn parse_manifest_accepts_a_dead_end_condition() {
    let toml = r#"
[agent]
name = "escapes"

[stages.work]
mode = "autonomous"

[stages.work.transitions.answer]
condition = "dead_end"

[stages.answer]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).expect("dead_end is a valid condition");
    let edge = bp
        .find_stage("work")
        .and_then(|s| s.transitions.as_ref())
        .and_then(|t| t.get("answer"))
        .expect("the edge survives");
    assert_eq!(
        edge.condition,
        crate::blueprint::TransitionCondition::DeadEnd
    );
}

// ─── stuck detection ────────────────────────────────────────────────────

/// Build a two-stage manifest whose `analyze → next` edge carries `body`.
fn stuck_edge_manifest(body: &str) -> String {
    format!(
        r#"
[agent]
name = "stuck-agent"

[stages.analyze]
mode = "autonomous"

[stages.analyze.transitions.next]
{body}

[stages.next]
mode = "autonomous"
"#
    )
}

#[test]
fn parse_manifest_reads_stuck_transition_thresholds() {
    let toml = stuck_edge_manifest(
        r#"condition = "stuck"
stuck_after_iterations = 20
stuck_after_minutes = 10
stuck_after_same_file_edits = 3
stuck_after_tool_calls = 60"#,
    );
    let bp = parse_manifest(&toml).unwrap();
    let edge = bp.stages[0]
        .transitions
        .as_ref()
        .unwrap()
        .get("next")
        .unwrap();
    assert_eq!(edge.condition, TransitionCondition::Stuck);
    let cfg = edge.stuck.expect("thresholds parsed");
    assert_eq!(cfg.after_iterations, Some(20));
    assert_eq!(cfg.after_minutes, Some(10));
    assert_eq!(cfg.after_same_file_edits, Some(3));
    assert_eq!(cfg.after_tool_calls, Some(60));
}

#[test]
fn parse_manifest_reads_a_partially_armed_stuck_edge() {
    let toml = stuck_edge_manifest(
        r#"condition = "stuck"
stuck_after_same_file_edits = 5"#,
    );
    let bp = parse_manifest(&toml).unwrap();
    let cfg = bp.stages[0].transitions.as_ref().unwrap()["next"]
        .stuck
        .expect("thresholds parsed");
    assert_eq!(cfg.after_same_file_edits, Some(5));
    assert_eq!(cfg.after_iterations, None);
    assert_eq!(cfg.after_minutes, None);
    assert_eq!(cfg.after_tool_calls, None);
}

#[test]
fn parse_manifest_rejects_a_stuck_condition_with_no_threshold() {
    let toml = stuck_edge_manifest(r#"condition = "stuck""#);
    let err = parse_manifest(&toml).unwrap_err().to_string();
    assert!(
        err.contains("condition 'stuck' but no threshold"),
        "got: {err}"
    );
}

/// A zero threshold reads as unset (mirroring `max_iterations = 0` meaning
/// "unlimited"), so it leaves the edge dead rather than firing on turn zero.
/// A negative one is refused outright; see
/// `every_negative_manifest_integer_fails_to_load_naming_the_key`.
#[test]
fn parse_manifest_treats_zero_stuck_thresholds_as_unset() {
    let toml = stuck_edge_manifest(
        r#"condition = "stuck"
stuck_after_iterations = 0
stuck_after_minutes = 0"#,
    );
    let err = parse_manifest(&toml).unwrap_err().to_string();
    assert!(
        err.contains("condition 'stuck' but no threshold"),
        "got: {err}"
    );
}

/// Thresholds under any other condition would silently never be read - the
/// classic "I forgot `condition = \"stuck\"`" footgun.
#[test]
fn parse_manifest_rejects_stuck_thresholds_without_the_stuck_condition() {
    let toml = stuck_edge_manifest("stuck_after_iterations = 20");
    let err = parse_manifest(&toml).unwrap_err().to_string();
    assert!(err.contains("its condition is not 'stuck'"), "got: {err}");
}

#[test]
fn parse_manifest_leaves_ordinary_edges_without_stuck_config() {
    let toml = stuck_edge_manifest(r#"hint = "go on""#);
    let bp = parse_manifest(&toml).unwrap();
    assert!(
        bp.stages[0].transitions.as_ref().unwrap()["next"]
            .stuck
            .is_none()
    );
}

#[test]
fn parse_manifest_rejects_unknown_edge_transform() {
    let toml = r#"
[agent]
name = "bad-xform"

[stages.analyze]
mode = "autonomous"

[stages.analyze.transitions.next]
transform = "teleport"

[stages.next]
mode = "autonomous"
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("unknown transform"), "got: {err}");
    assert!(err.contains("teleport"), "got: {err}");
}

#[test]
fn parse_manifest_custom_region_kind() {
    let toml = r#"
[agent]
name = "custom-region-test"

[context.regions]
brain = { kind = "custom", script = "context_hooks/brain.rhai", persistent = true, max_tokens = 5000 }
scratch = { kind = "custom", script = "context_hooks/scratch.rhai", max_tokens = 2000 }
"#;
    let bp = parse_manifest(toml).unwrap();
    let brain = bp
        .context_layout
        .regions
        .iter()
        .find(|r| r.name == "brain")
        .unwrap();
    assert_eq!(
        brain.kind,
        RegionKind::Custom {
            script: "context_hooks/brain.rhai".to_string(),
            persistent: true,
        }
    );
    // persistent defaults to false when omitted.
    let scratch = bp
        .context_layout
        .regions
        .iter()
        .find(|r| r.name == "scratch")
        .unwrap();
    assert_eq!(
        scratch.kind,
        RegionKind::Custom {
            script: "context_hooks/scratch.rhai".to_string(),
            persistent: false,
        }
    );
}

#[test]
fn parse_manifest_custom_region_requires_script() {
    // Missing script is a load-time hard error, not a silent fallback.
    let missing = r#"
[agent]
name = "bad"

[context.regions]
brain = { kind = "custom", max_tokens = 5000 }
"#;
    let err = parse_manifest(missing).unwrap_err();
    assert!(
        err.to_string().contains("requires"),
        "actionable error: {err}"
    );

    // An empty/whitespace script path is equally dead - same error.
    let empty = r#"
[agent]
name = "bad"

[context.regions]
brain = { kind = "custom", script = "  ", max_tokens = 5000 }
"#;
    let err = parse_manifest(empty).unwrap_err();
    assert!(err.to_string().contains("requires"), "{err}");
}

#[test]
fn parse_manifest_custom_region_with_percent_budget() {
    // Percentage budgets work on custom regions exactly as on built-ins.
    let toml = r#"
[agent]
name = "pct-custom"

[context.regions]
brain = { kind = "custom", script = "b.rhai", budget = "40%", min_tokens = 10000 }
"#;
    let bp = parse_manifest(toml).unwrap();
    let brain = bp
        .context_layout
        .regions
        .iter()
        .find(|r| r.name == "brain")
        .unwrap();
    assert!(brain.budget.is_percent());
    let resolved = bp.context_layout.resolved(200_000);
    assert_eq!(
        resolved
            .regions
            .iter()
            .find(|r| r.name == "brain")
            .unwrap()
            .max_tokens,
        80_000
    );
}

#[test]
fn parse_manifest_per_stage_custom_region() {
    // A per-stage layout can declare a custom region only that stage uses.
    let toml = r#"
[agent]
name = "stage-custom"

[context.regions]
task = { kind = "pinned", max_tokens = 4000 }

[stages.plan]
[stages.plan.model]
provider = "anthropic"
model = "claude-sonnet-4"

[stages.plan.context.regions]
plan_view = { kind = "custom", script = "hooks/plan.rhai", max_tokens = 6000 }

[stages.implement]
[stages.implement.model]
provider = "anthropic"
model = "claude-sonnet-4"

[stages.plan.transitions.implement]
condition = "always"
"#;
    let bp = parse_manifest(toml).unwrap();
    let plan = bp.stages.iter().find(|s| s.name == "plan").unwrap();
    let layout = plan.context_layout.as_ref().unwrap();
    assert!(layout.regions.iter().any(|r| matches!(
        &r.kind,
        RegionKind::Custom { script, persistent: false } if script == "hooks/plan.rhai"
    )));
    // Sibling stage inherits the global layout (no per-stage override).
    let implement = bp.stages.iter().find(|s| s.name == "implement").unwrap();
    assert!(implement.context_layout.is_none());
}

#[test]
fn parse_manifest_with_context_regions_all_kinds() {
    let toml = r#"
[agent]
name = "region-test"

[context.regions]
sys = { kind = "pinned", max_tokens = 1000 }
conv = { kind = "sliding_window", max_items = 15, max_tokens = 5000 }
temp = { kind = "temporary", max_tokens = 3000 }
comp = { kind = "compacting", threshold_tokens = 4000, max_tokens = 6000 }
clr = { kind = "clearable", max_tokens = 2000 }
hist = { kind = "compact_history", source_region = "conv", max_tokens = 4000 }
"#;
    let bp = parse_manifest(toml).unwrap();
    assert_eq!(bp.context_layout.regions.len(), 6);

    let sys = bp
        .context_layout
        .regions
        .iter()
        .find(|r| r.name == "sys")
        .unwrap();
    assert_eq!(sys.kind, RegionKind::Pinned);
    assert_eq!(sys.max_tokens, 1000);

    let conv = bp
        .context_layout
        .regions
        .iter()
        .find(|r| r.name == "conv")
        .unwrap();
    assert_eq!(
        conv.kind,
        RegionKind::SlidingWindow {
            max_items: 15,
            eviction_strategy: EvictionStrategy::PerItem,
        }
    );

    let temp = bp
        .context_layout
        .regions
        .iter()
        .find(|r| r.name == "temp")
        .unwrap();
    assert_eq!(temp.kind, RegionKind::Temporary);

    let comp = bp
        .context_layout
        .regions
        .iter()
        .find(|r| r.name == "comp")
        .unwrap();
    assert_eq!(
        comp.kind,
        RegionKind::Compacting {
            threshold_tokens: 4000
        }
    );

    let clr = bp
        .context_layout
        .regions
        .iter()
        .find(|r| r.name == "clr")
        .unwrap();
    assert_eq!(clr.kind, RegionKind::Clearable);

    let hist = bp
        .context_layout
        .regions
        .iter()
        .find(|r| r.name == "hist")
        .unwrap();
    assert_eq!(
        hist.kind,
        RegionKind::CompactHistory {
            source_region: "conv".to_string()
        }
    );

    // Back-compat: with no `budget`, every region is an Absolute budget
    // matching its max_tokens, and compact_at stays None.
    assert_eq!(sys.budget, crate::BudgetSpec::Absolute(1000));
    assert_eq!(sys.compact_at, None);
    assert_eq!(comp.budget, crate::BudgetSpec::Absolute(6000));
    assert_eq!(comp.compact_at, None);
}

#[test]
fn parse_region_percent_budget_with_guards() {
    let toml = r#"
[agent]
name = "pct"

[context.regions]
task = { kind = "pinned", budget = "2%", max_tokens = 4000, min_tokens = 500 }
free = { kind = "temporary", budget = "25%" }
abs  = { kind = "pinned", max_tokens = 3000 }
"#;
    let bp = parse_manifest(toml).unwrap();
    let task = bp.context_layout.get_region("task").unwrap();
    assert_eq!(
        task.budget,
        crate::BudgetSpec::Percent {
            percent: 0.02,
            min: Some(500),
            max: Some(4000),
        }
    );
    // Provisional max_tokens is the cap until resolution.
    assert_eq!(task.max_tokens, 4000);

    let free = bp.context_layout.get_region("free").unwrap();
    assert_eq!(
        free.budget,
        crate::BudgetSpec::Percent {
            percent: 0.25,
            min: None,
            max: None,
        }
    );
    // Percentage with no cap → provisional 0 until resolved.
    assert_eq!(free.max_tokens, 0);

    let abs = bp.context_layout.get_region("abs").unwrap();
    assert_eq!(abs.budget, crate::BudgetSpec::Absolute(3000));

    assert!(bp.context_layout.has_percent_budgets());
    // Only the absolute region contributes to the summed total.
    assert_eq!(bp.context_layout.total_budget_tokens, 3000);
}

#[test]
fn parse_region_compact_at_variants() {
    let toml = r#"
[agent]
name = "compacting"

[context.regions]
capped   = { kind = "compacting", budget = "35%", compact_at = "80%", threshold_tokens = 32000 }
uncapped = { kind = "compacting", budget = "35%", compact_at = "80%" }
pct_only = { kind = "compacting", budget = "35%" }
absolute = { kind = "compacting", threshold_tokens = 9000 }
default  = { kind = "compacting", max_tokens = 10000 }
"#;
    let bp = parse_manifest(toml).unwrap();

    let capped = bp.context_layout.get_region("capped").unwrap();
    assert_eq!(capped.compact_at, Some(0.80));
    assert_eq!(
        capped.kind,
        RegionKind::Compacting {
            threshold_tokens: 32000
        }
    );

    let uncapped = bp.context_layout.get_region("uncapped").unwrap();
    assert_eq!(uncapped.compact_at, Some(0.80));
    assert_eq!(
        uncapped.kind,
        RegionKind::Compacting {
            threshold_tokens: usize::MAX
        }
    );

    // budget but no compact_at / threshold → 80% default (percentage mode).
    let pct_only = bp.context_layout.get_region("pct_only").unwrap();
    assert_eq!(pct_only.compact_at, Some(0.80));
    assert_eq!(
        pct_only.kind,
        RegionKind::Compacting {
            threshold_tokens: usize::MAX
        }
    );

    // Absolute threshold, no budget → absolute back-compat, compact_at None.
    let absolute = bp.context_layout.get_region("absolute").unwrap();
    assert_eq!(absolute.compact_at, None);
    assert_eq!(
        absolute.kind,
        RegionKind::Compacting {
            threshold_tokens: 9000
        }
    );

    // No budget, no compact_at, no threshold → legacy max_tokens * 8 / 10.
    let default = bp.context_layout.get_region("default").unwrap();
    assert_eq!(default.compact_at, None);
    assert_eq!(
        default.kind,
        RegionKind::Compacting {
            threshold_tokens: 8000
        }
    );
}

#[test]
fn parse_region_rejects_malformed_budget() {
    let toml = r#"
[agent]
name = "bad"

[context.regions]
task = { kind = "pinned", budget = "lots" }
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("must end with '%'"), "{err}");
}

#[test]
fn parse_region_rejects_out_of_range_budget() {
    let toml = r#"
[agent]
name = "bad"

[context.regions]
task = { kind = "pinned", budget = "150%" }
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("at most 100%"), "{err}");
}

#[test]
fn parse_region_rejects_malformed_compact_at() {
    let toml = r#"
[agent]
name = "bad"

[context.regions]
work = { kind = "compacting", budget = "35%", compact_at = "eighty" }
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("must end with '%'"), "{err}");
}

#[test]
fn parse_manifest_reads_per_stage_context_regions() {
    let toml = r#"
[agent]
name = "per-stage"
entry_stage = "plan"

[stages.plan]

[stages.plan.context.regions]
plan     = { kind = "pinned", budget = "20%", max_tokens = 40000 }
codebase = { kind = "compacting", budget = "30%", compact_at = "70%" }

[stages.implement]

[context.regions]
task = { kind = "pinned", max_tokens = 4000 }
"#;
    let bp = parse_manifest(toml).unwrap();

    // The plan stage has its own layout with percentage budgets.
    let plan_stage = bp.stages.iter().find(|s| s.name == "plan").unwrap();
    let plan_layout = plan_stage.context_layout.as_ref().unwrap();
    assert!(plan_layout.has_percent_budgets());
    let plan_region = plan_layout.get_region("plan").unwrap();
    assert_eq!(
        plan_region.budget,
        crate::BudgetSpec::Percent {
            percent: 0.20,
            min: None,
            max: Some(40000),
        }
    );
    let codebase = plan_layout.get_region("codebase").unwrap();
    assert_eq!(codebase.compact_at, Some(0.70));

    // The implement stage declared no regions → inherits the global layout.
    let implement_stage = bp.stages.iter().find(|s| s.name == "implement").unwrap();
    assert!(implement_stage.context_layout.is_none());
}

#[test]
fn parse_manifest_rejects_malformed_per_stage_budget() {
    // A bad budget inside a [stages.X.context.regions] table propagates the
    // parse error just like the global layout does.
    let toml = r#"
[agent]
name = "bad-stage"
entry_stage = "plan"

[stages.plan]

[stages.plan.context.regions]
plan = { kind = "pinned", budget = "nope" }
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("must end with '%'"), "{err}");
}

#[test]
fn parse_manifest_with_model_config() {
    let toml = r#"
[agent]
name = "model-test"

[stages.main]
model = { provider = "google", model = "gemini-3.5-pro" }
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert_eq!(stage.model.provider(), "google");
    assert_eq!(stage.model.model(), "gemini-3.5-pro");
}

#[test]
fn system_prompt_nested_under_model_is_ignored_and_warned() {
    // A `system_prompt` written after the `[stages.main.model]` table nests
    // under the model table (TOML rules), so the stage never receives it.
    // parse_manifest emits a warning; the stage config must NOT contain it.
    let toml = r#"
[agent]
name = "misplaced-sp"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"
system_prompt = "these instructions are misplaced under [model]"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert!(
        !stage.config.contains_key("system_prompt"),
        "a system_prompt nested under [model] must not become the stage prompt"
    );
}

#[test]
fn parse_manifest_reads_region_required_and_message() {
    let toml = r#"
[agent]
name = "req-test"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[context.regions]
plan = { kind = "pinned", max_tokens = 4000, required = true, required_message = "write the plan" }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#;
    let bp = parse_manifest(toml).unwrap();
    let plan = bp.context_layout.get_region("plan").unwrap();
    assert!(plan.required, "required flag parsed");
    assert_eq!(plan.required_message.as_deref(), Some("write the plan"));
    let conv = bp.context_layout.get_region("conversation").unwrap();
    assert!(!conv.required, "unmarked region defaults to not required");
    assert!(conv.required_message.is_none());
}

#[test]
fn parse_manifest_reads_region_seed_shapes() {
    let toml = r#"
[agent]
name = "seed-test"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[context.regions]
task = { kind = "pinned", max_tokens = 4000, seed = "task_input" }
criteria = { kind = "pinned", max_tokens = 2000, seed = "input" }
alias = { kind = "pinned", max_tokens = 2000, seed = "files_arg" }
specs = { kind = "pinned", max_tokens = 8000, seed = { glob = "specs/*.md" } }
config = { kind = "pinned", max_tokens = 2000, seed = { files = ["a.yaml", "b.yaml"] } }
lit = { kind = "pinned", max_tokens = 500, seed = { literal = "hello" } }
scripted = { kind = "pinned", max_tokens = 500, seed = { rhai = "init.rhai" } }
facts = { kind = "pinned", max_tokens = 500, seed = { command = "git ls-files" } }
clock = { kind = "pinned", max_tokens = 500, seed = { tool = "current_time" } }
env = { kind = "pinned", max_tokens = 500, seed = { tools = ["current_time", "system_info"] } }
toolchain = { kind = "pinned", max_tokens = 500, seed = { tools = [{ name = "which_command", args = { command = "git" } }, { name = "system_info" }, "locale_info"] } }
plain = { kind = "pinned", max_tokens = 500 }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#;
    let bp = parse_manifest(toml).unwrap();
    let region = |n: &str| bp.context_layout.get_region(n).unwrap().seed.clone();
    assert_eq!(
        region("task"),
        Some(RegionSeed::CallerInput {
            name: "task".to_string()
        })
    );
    assert_eq!(
        region("criteria"),
        Some(RegionSeed::CallerInput {
            name: "criteria".to_string()
        }),
        "\"input\" is keyed by the region's own name"
    );
    assert_eq!(
        region("alias"),
        Some(RegionSeed::CallerInput {
            name: "files_arg".to_string()
        }),
        "a bare string is a caller-input key"
    );
    assert_eq!(
        region("specs"),
        Some(RegionSeed::Glob {
            pattern: "specs/*.md".to_string()
        })
    );
    assert_eq!(
        region("config"),
        Some(RegionSeed::Files {
            paths: vec!["a.yaml".to_string(), "b.yaml".to_string()]
        })
    );
    assert_eq!(
        region("lit"),
        Some(RegionSeed::Literal {
            text: "hello".to_string()
        })
    );
    assert_eq!(
        region("scripted"),
        Some(RegionSeed::Rhai {
            script: "init.rhai".to_string()
        })
    );
    assert_eq!(
        region("facts"),
        Some(RegionSeed::Command {
            command: "git ls-files".to_string()
        })
    );
    // `{ tool = "..." }` is the one-call shorthand, and carries no arguments.
    assert_eq!(
        region("clock"),
        Some(RegionSeed::Tools {
            calls: vec![SeedToolCall::new("current_time")],
            refresh: crate::layout::SeedRefresh::Once,
        })
    );
    // A list of bare names is a list of argument-free calls, in order.
    assert_eq!(
        region("env"),
        Some(RegionSeed::Tools {
            calls: vec![
                SeedToolCall::new("current_time"),
                SeedToolCall::new("system_info"),
            ],
            refresh: crate::layout::SeedRefresh::Once,
        })
    );
    // The two entry spellings mix in one list, because most calls take no
    // arguments and should not have to look like they might.
    assert_eq!(
        region("toolchain"),
        Some(RegionSeed::Tools {
            calls: vec![
                SeedToolCall::with_args("which_command", serde_json::json!({ "command": "git" })),
                // The table form without `args` means the same as the bare
                // string: a call with no arguments, not one with null args.
                SeedToolCall::new("system_info"),
                SeedToolCall::new("locale_info"),
            ],
            refresh: crate::layout::SeedRefresh::Once,
        })
    );
    assert!(
        region("plain").is_none(),
        "a non-task region with no seed stays None"
    );
}

/// `refresh` decides whether a tool seed runs again on every stage entry. It
/// defaults to `once`, which is what every other seed kind does, so the key's
/// absence must not be read as an opt-in.
#[test]
fn a_tool_seed_refreshes_only_when_it_says_so() {
    let base = r#"
[agent]
name = "seed-test"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[context.regions]
task = { kind = "pinned", max_tokens = 4000, seed = "task_input" }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#;
    let refresh_of = |line: &str| {
        let toml = format!("{base}{line}\n");
        match parse_manifest(&toml)
            .unwrap()
            .context_layout
            .get_region("r")
            .unwrap()
            .seed
            .clone()
        {
            Some(RegionSeed::Tools { refresh, .. }) => Some(refresh),
            _ => None,
        }
    };
    use crate::layout::SeedRefresh;
    // Unset is `once`, for both spellings of a tool seed.
    assert_eq!(
        refresh_of(
            r#"r = { kind = "pinned", max_tokens = 500, seed = { tool = "current_time" } }"#
        ),
        Some(SeedRefresh::Once)
    );
    assert_eq!(
        refresh_of(
            r#"r = { kind = "pinned", max_tokens = 500, seed = { tools = ["current_time"] } }"#
        ),
        Some(SeedRefresh::Once)
    );
    // And set, again for both.
    assert_eq!(
        refresh_of(
            r#"r = { kind = "pinned", max_tokens = 500, seed = { tool = "current_time", refresh = "each_stage" } }"#
        ),
        Some(SeedRefresh::EachStage)
    );
    assert_eq!(
        refresh_of(
            r#"r = { kind = "pinned", max_tokens = 500, seed = { tools = ["current_time"], refresh = "each_stage" } }"#
        ),
        Some(SeedRefresh::EachStage)
    );
    // A value that is not a word at all falls back rather than failing the
    // whole manifest, as the rest of this parser does with a key it cannot read.
    assert_eq!(
        refresh_of(
            r#"r = { kind = "pinned", max_tokens = 500, seed = { tool = "current_time", refresh = 7 } }"#
        ),
        Some(SeedRefresh::Once)
    );
    assert_eq!(
        refresh_of(
            r#"r = { kind = "pinned", max_tokens = 500, seed = { tool = "current_time", refresh = "every stage" } }"#
        ),
        Some(SeedRefresh::Once)
    );
}

/// A `tools` list that names nothing usable is not a tool seed. Leaving it
/// unparsed is what makes `lev validate` report `region-seed-not-understood`,
/// which is a better answer than a seed that silently runs nothing.
#[test]
fn a_tool_seed_with_no_readable_call_is_not_a_seed() {
    let base = r#"
[agent]
name = "seed-test"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[context.regions]
task = { kind = "pinned", max_tokens = 4000, seed = "task_input" }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#;
    let seed_of = |line: &str| {
        let toml = format!("{base}{line}\n");
        parse_manifest(&toml)
            .unwrap()
            .context_layout
            .get_region("r")
            .unwrap()
            .seed
            .clone()
    };
    // An empty list.
    assert_eq!(
        seed_of(r#"r = { kind = "pinned", max_tokens = 500, seed = { tools = [] } }"#),
        None
    );
    // Entries of a shape that names no tool.
    assert_eq!(
        seed_of(r#"r = { kind = "pinned", max_tokens = 500, seed = { tools = [1, 2] } }"#),
        None
    );
    // A table with no `name`.
    assert_eq!(
        seed_of(
            r#"r = { kind = "pinned", max_tokens = 500, seed = { tools = [{ args = { a = 1 } }] } }"#
        ),
        None
    );
    // But one readable entry among unreadable ones still seeds, carrying only
    // the entries that named something.
    assert_eq!(
        seed_of(
            r#"r = { kind = "pinned", max_tokens = 500, seed = { tools = [1, "current_time"] } }"#
        ),
        Some(RegionSeed::Tools {
            calls: vec![SeedToolCall::new("current_time")],
            refresh: crate::layout::SeedRefresh::Once,
        })
    );
}

#[test]
fn parse_manifest_region_seed_caller_table_and_non_seed_shapes() {
    let toml = r#"
[agent]
name = "seed-edges"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[context.regions]
via_caller = { kind = "pinned", max_tokens = 500, seed = { caller = "extra" } }
unknown_tbl = { kind = "pinned", max_tokens = 500, seed = { nope = "x" } }
weird_type = { kind = "pinned", max_tokens = 500, seed = 42 }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#;
    let bp = parse_manifest(toml).unwrap();
    let region = |n: &str| bp.context_layout.get_region(n).unwrap().seed.clone();
    assert_eq!(
        region("via_caller"),
        Some(RegionSeed::CallerInput {
            name: "extra".to_string()
        })
    );
    // A table with none of the recognized keys → no seed.
    assert!(region("unknown_tbl").is_none());
    // A non-string, non-table seed value → no seed.
    assert!(region("weird_type").is_none());
}

#[test]
fn parse_manifest_seeds_unnamed_task_region_by_default() {
    // A region literally named `task` with no explicit `seed` still gets an
    // implicit CallerInput seed, preserving pre-feature task seeding.
    let toml = r#"
[agent]
name = "implicit-task"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[context.regions]
task = { kind = "pinned", max_tokens = 4000 }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#;
    let bp = parse_manifest(toml).unwrap();
    assert_eq!(
        bp.context_layout.get_region("task").unwrap().seed,
        Some(RegionSeed::CallerInput {
            name: "task".to_string()
        })
    );
}

#[test]
fn parse_manifest_reads_repetition_detection() {
    let toml = r#"
[agent]
name = "rep-test"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[repetition_detection]
max_repeat_calls = 4
max_readonly_streak = 12
enabled = true
"#;
    let bp = parse_manifest(toml).unwrap();
    let rd = bp
        .repetition_detection
        .expect("repetition_detection parsed");
    assert_eq!(rd.max_repeat_calls, Some(4));
    assert_eq!(rd.max_readonly_streak, Some(12));
    assert_eq!(rd.enabled, Some(true));
}

#[test]
fn parse_manifest_repetition_detection_absent_and_partial() {
    // Absent → None.
    let base = r#"
[agent]
name = "rep2"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"
"#;
    assert!(parse_manifest(base).unwrap().repetition_detection.is_none());
    // A partial block leaves the unset fields as None.
    let partial = format!("{base}\n[repetition_detection]\nenabled = false\n");
    let rd = parse_manifest(&partial)
        .unwrap()
        .repetition_detection
        .unwrap();
    assert_eq!(rd.enabled, Some(false));
    assert_eq!(rd.max_repeat_calls, None);
    assert_eq!(rd.max_readonly_streak, None);
}

#[test]
fn parse_manifest_reads_transforms_with_all_mapping_kinds() {
    use crate::blueprint::ContentTransform;
    let toml = r#"
[agent]
name = "xform-test"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[[transforms]]
from_blueprint = "planner"
to_blueprint = "coder"

[[transforms.mappings]]
from_region = "plan"
to_region = "plan"
transform = "direct"

[[transforms.mappings]]
from_region = "notes"
to_region = "summary"
transform = "summarize"

[[transforms.mappings]]
from_region = "spec"
to_region = "fields"
transform = "extract"
fields = ["title", "owner"]

[[transforms.mappings]]
from_region = "misc"
to_region = "misc"
"#;
    // Render each parsed transform to a stable tag (ContentTransform has no
    // PartialEq); the four mappings exercise every arm.
    fn tag(t: &Option<ContentTransform>) -> String {
        match t {
            None => "none".to_string(),
            Some(ContentTransform::Direct) => "direct".to_string(),
            Some(ContentTransform::Summarize) => "summarize".to_string(),
            Some(ContentTransform::Extract { fields }) => {
                format!("extract:{}", fields.join(","))
            }
        }
    }
    let bp = parse_manifest(toml).unwrap();
    assert_eq!(bp.transforms.len(), 1);
    let xf = &bp.transforms[0];
    assert_eq!(xf.from_blueprint, "planner");
    assert_eq!(xf.to_blueprint, "coder");
    assert_eq!(xf.mappings.len(), 4);
    assert_eq!(tag(&xf.mappings[0].transform), "direct");
    assert_eq!(tag(&xf.mappings[1].transform), "summarize");
    assert_eq!(tag(&xf.mappings[2].transform), "extract:title,owner");
    // A mapping with no `transform` key → None (plain copy at apply time).
    assert_eq!(tag(&xf.mappings[3].transform), "none");
    assert_eq!(xf.mappings[3].from_region, "misc");
}

#[test]
fn parse_manifest_transform_unknown_kind_is_none() {
    let toml = r#"
[agent]
name = "xform-unknown"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[[transforms]]
from_blueprint = "a"
to_blueprint = "b"

[[transforms.mappings]]
from_region = "r"
to_region = "r"
transform = "bogus"
"#;
    let bp = parse_manifest(toml).unwrap();
    // An unrecognized transform is treated as a plain copy (None), never a panic.
    assert!(bp.transforms[0].mappings[0].transform.is_none());
}

#[test]
fn parse_manifest_transforms_absent_is_empty() {
    let toml = r#"
[agent]
name = "no-xform"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"
"#;
    assert!(parse_manifest(toml).unwrap().transforms.is_empty());
}

#[test]
fn parse_manifest_model_with_models_list() {
    let toml = r#"
[agent]
name = "models-list-test"

[stages.main.model]
allow_user_default = false

[[stages.main.model.models]]
provider = "anthropic"
model = "claude-sonnet-4-6"

[[stages.main.model.models]]
provider = "openai"
model = "gpt-4o"

[[stages.main.model.models]]
provider = "ollama"
model = "llama3"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert_eq!(stage.model.models.len(), 3);
    assert_eq!(stage.model.models[0].provider, "anthropic");
    assert_eq!(stage.model.models[0].model, "claude-sonnet-4-6");
    assert_eq!(stage.model.models[1].provider, "openai");
    assert_eq!(stage.model.models[1].model, "gpt-4o");
    assert_eq!(stage.model.models[2].provider, "ollama");
    assert_eq!(stage.model.models[2].model, "llama3");
    assert!(!stage.model.allow_user_default);
}

#[test]
fn parse_manifest_model_backward_compat_fallbacks() {
    // Old format with fallbacks should be converted to models list
    let toml = r#"
[agent]
name = "fallback-test"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-4-6"

[[stages.main.model.fallbacks]]
provider = "openai"
model = "gpt-4o"

[[stages.main.model.fallbacks]]
provider = "ollama"
model = "llama3"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert_eq!(stage.model.models.len(), 3);
    assert_eq!(stage.model.models[0].provider, "anthropic");
    assert_eq!(stage.model.models[0].model, "claude-sonnet-4-6");
    assert_eq!(stage.model.models[1].provider, "openai");
    assert_eq!(stage.model.models[1].model, "gpt-4o");
    assert_eq!(stage.model.models[2].provider, "ollama");
    assert_eq!(stage.model.models[2].model, "llama3");
}

#[test]
fn parse_manifest_model_with_parameters() {
    let toml = r#"
[agent]
name = "params-test"

[stages.main]

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-4-6"

[stages.main.model.parameters]
temperature = 0.3
max_output_tokens = 8192
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert_eq!(
        stage
            .model
            .parameters
            .get("temperature")
            .and_then(|v| v.as_f64()),
        Some(0.3)
    );
    assert_eq!(
        stage
            .model
            .parameters
            .get("max_output_tokens")
            .and_then(|v| v.as_u64()),
        Some(8192)
    );
}

#[test]
fn parse_manifest_default_model() {
    let toml = r#"
[agent]
name = "default-model"

[stages.main]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert_eq!(stage.model.provider(), "anthropic");
    assert_eq!(stage.model.model(), "claude-sonnet-4-6");
}

#[test]
fn parse_manifest_model_table_without_models_uses_default() {
    // A model table that exists but declares no `models`, no top-level
    // `provider`, and no `fallbacks` must fall through to the built-in
    // default single entry.
    let toml = r#"
[agent]
name = "empty-model-table"

[stages.main.model]
allow_user_default = false
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert_eq!(stage.model.models.len(), 1);
    assert_eq!(stage.model.models[0].provider, "anthropic");
    assert_eq!(stage.model.models[0].model, "claude-sonnet-4-6");
    assert!(!stage.model.allow_user_default);
}

#[test]
fn parse_manifest_models_array_takes_bare_names_and_leaves_the_route_open() {
    // A bare string names a model and leaves the route open. A table may pin a
    // provider when the route matters, and one naming no model names nothing.
    let toml = r#"
[agent]
name = "models-defaults"

[stages.main.model]
models = ["gpt-5.5", { provider = "openai" }, { model = "custom-model" }, 7, { provider = "ollama", model = "q:9b" }]
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert_eq!(
        stage.model.models.len(),
        3,
        "an entry naming no model is dropped, whether it is a table without one \
         or not a model entry at all"
    );

    // A bare name: the model is what the author asked for, the route is not
    // theirs to know.
    assert_eq!(stage.model.models[0].model, "gpt-5.5");
    assert_eq!(stage.model.models[0].provider, "");

    // An absent provider is empty, NOT "anthropic". Defaulting it made omitting
    // the field a silent specific choice rather than an open one.
    assert_eq!(stage.model.models[1].model, "custom-model");
    assert_eq!(stage.model.models[1].provider, "");

    // A named provider still pins the route, which a local model needs.
    assert_eq!(stage.model.models[2].provider, "ollama");
    assert_eq!(stage.model.models[2].model, "q:9b");
}

#[test]
fn parse_manifest_top_level_provider_without_model() {
    // Old single-model format with a top-level provider but no model →
    // model defaults to claude-sonnet-4-6.
    let toml = r#"
[agent]
name = "provider-only"

[stages.main.model]
provider = "openai"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert_eq!(stage.model.models.len(), 1);
    assert_eq!(stage.model.models[0].provider, "openai");
    assert_eq!(stage.model.models[0].model, "claude-sonnet-4-6");
}

#[test]
fn parse_manifest_fallbacks_without_top_level_provider() {
    // `fallbacks` with no top-level provider: non-table entries are
    // skipped and per-field defaults apply to the table entries.
    let toml = r#"
[agent]
name = "fallbacks-only"

[stages.main.model]
fallbacks = ["skip-me", { provider = "openai" }, { model = "custom-model" }]
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert_eq!(stage.model.models.len(), 2);
    // provider given, model defaulted
    assert_eq!(stage.model.models[0].provider, "openai");
    assert_eq!(stage.model.models[0].model, "claude-sonnet-4-6");
    // model given, provider defaulted
    assert_eq!(stage.model.models[1].provider, "anthropic");
    assert_eq!(stage.model.models[1].model, "custom-model");
}

#[test]
fn parse_manifest_with_interaction_points() {
    let toml = r#"
[agent]
name = "interactive-test"

[stages.main]
mode = "interactive_points"

[[stages.main.interaction_points]]
name = "review"
prompt = "Review the output"
required = true
style = "multiple_choice"
options = ["approve", "reject", "revise"]
document_region = "plan"

[[stages.main.interaction_points]]
name = "feedback"
prompt = "Any feedback?"
required = false
style = "free_text"

[[stages.main.interaction_points]]
name = "confirm"
prompt = "Proceed?"
style = "confirm"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    let points = unwrap_interactive_points(&stage.mode);
    assert_eq!(points.len(), 3);

    assert_eq!(points[0].name, "review");
    assert_eq!(points[0].prompt, "Review the output");
    assert!(points[0].required);
    assert_eq!(
        points[0].style,
        crate::blueprint::InteractionStyle::MultipleChoice
    );
    assert_eq!(points[0].options, vec!["approve", "reject", "revise"]);
    assert_eq!(points[0].document_region.as_deref(), Some("plan"));
    // A point that omits it parses to None.
    assert_eq!(points[1].document_region, None);

    assert_eq!(points[1].name, "feedback");
    assert!(!points[1].required);
    assert_eq!(
        points[1].style,
        crate::blueprint::InteractionStyle::FreeText
    );

    assert_eq!(points[2].name, "confirm");
    assert_eq!(points[2].style, crate::blueprint::InteractionStyle::Confirm);
}

#[test]
fn parse_manifest_interaction_point_directives_and_abort() {
    let toml = r#"
[agent]
name = "directive-test"

[stages.plan]
mode = "interactive_points"

[[stages.plan.interaction_points]]
name     = "plan_approval"
prompt   = "Approve?"
required = true
style    = "multiple_choice"
options  = ["Approve", "Revise", "Edit", "Abort"]
abort_options = ["Abort"]
edit_options = ["Edit"]
directives = { "Revise" = "Call ask_user_text to find out what to change." }
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("plan").unwrap();
    let points = unwrap_interactive_points(&stage.mode);
    assert_eq!(points.len(), 1);
    assert_eq!(
        points[0].directives.get("Revise").map(|s| s.as_str()),
        Some("Call ask_user_text to find out what to change.")
    );
    assert!(!points[0].directives.contains_key("Approve"));
    assert_eq!(points[0].abort_options, vec!["Abort".to_string()]);
    assert_eq!(points[0].edit_options, vec!["Edit".to_string()]);
}

/// The unattended opt-out: absent means the point waves itself through under
/// `--yolo`, `"ask"` means it holds for a person.
#[test]
fn parse_manifest_interaction_point_unattended_policy() {
    let toml = |line: &str| {
        format!(
            r#"
[agent]
name = "unattended-test"

[stages.plan]
mode = "interactive_points"

[[stages.plan.interaction_points]]
name     = "plan_approval"
prompt   = "Approve?"
required = true
{line}
"#
        )
    };
    let policy_of = |line: &str| {
        let bp = parse_manifest(&toml(line)).expect("valid manifest");
        let stage = bp.find_stage("plan").expect("the plan stage");
        unwrap_interactive_points(&stage.mode)[0].unattended
    };

    assert_eq!(
        policy_of(""),
        crate::blueprint::UnattendedPolicy::AutoApprove
    );
    assert_eq!(
        policy_of("unattended = \"auto_approve\""),
        crate::blueprint::UnattendedPolicy::AutoApprove
    );
    assert_eq!(
        policy_of("unattended = \"ask\""),
        crate::blueprint::UnattendedPolicy::Ask
    );
}

/// A misspelling here would silently un-gate a checkpoint an author meant to
/// hold, so it is an error rather than a fallback to the default.
#[test]
fn parse_manifest_rejects_an_unknown_unattended_policy() {
    let toml = r#"
[agent]
name = "unattended-typo"

[stages.plan]
mode = "interactive_points"

[[stages.plan.interaction_points]]
name     = "plan_approval"
prompt   = "Approve?"
required = true
unattended = "always"
"#;
    let err = parse_manifest(toml).expect_err("unknown policy");
    let text = err.to_string();
    assert!(text.contains("plan_approval"), "names the point: {text}");
    assert!(text.contains("always"), "quotes what was written: {text}");
    assert!(text.contains("\"ask\""), "says what is allowed: {text}");
}

/// `required_tools` names the human tools a stage keeps when nobody is
/// watching.
#[test]
fn parse_manifest_reads_required_tools() {
    let toml = r#"
[agent]
name = "required-tools-test"

[stages.plan]
mode = "autonomous"
available_tools = ["read_file", "ask_user_text"]
required_tools = ["ask_user_text"]

[stages.build]
mode = "autonomous"
available_tools = ["read_file"]
"#;
    let bp = parse_manifest(toml).expect("valid manifest");
    assert_eq!(
        bp.find_stage("plan").expect("plan").required_tools,
        vec!["ask_user_text".to_string()]
    );
    // A stage that says nothing keeps nothing - the default is the cut.
    assert!(
        bp.find_stage("build")
            .expect("build")
            .required_tools
            .is_empty()
    );
}

#[test]
fn parse_manifest_interaction_point_followups_alias_maps_to_directives() {
    // Backward compat: the old `followups` key is accepted as an alias.
    let toml = r#"
[agent]
name = "followup-alias-test"

[stages.plan]
mode = "interactive_points"

[[stages.plan.interaction_points]]
name     = "plan_approval"
prompt   = "Approve?"
required = true
style    = "multiple_choice"
options  = ["Approve", "Revise"]
followups = { "Revise" = "What would you like to change?" }
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("plan").unwrap();
    let points = unwrap_interactive_points(&stage.mode);
    assert_eq!(
        points[0].directives.get("Revise").map(|s| s.as_str()),
        Some("What would you like to change?")
    );
}

#[test]
fn parse_manifest_agent_and_stage_security() {
    let toml = r#"
[agent]
name = "sec-test"

[security]
taint_tracking = true

[stages.plan]
mode = "autonomous"

[stages.plan.security]
taint_tracking = false

[stages.build]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    // Agent-level [security] parsed.
    assert!(bp.security.as_ref().unwrap().taint_tracking);
    // Stage-level [stages.plan.security] opts this stage out.
    let plan = bp.find_stage("plan").unwrap();
    assert_eq!(
        plan.security.as_ref().map(|s| s.taint_tracking),
        Some(false)
    );
    // A stage with no [security] inherits (None).
    let build = bp.find_stage("build").unwrap();
    assert!(build.security.is_none());
}

#[test]
fn parse_manifest_read_paths_declarations() {
    let toml = r#"
[agent]
name = "rp-test"

[read_paths]
allow = ["~/.leviath/runs", "glob:~/design-docs/**", "regex:/data/archives/.*"]

[stages.plan]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    let rp = bp.read_paths.as_ref().unwrap();
    assert_eq!(
        rp.allow,
        vec![
            "~/.leviath/runs".to_string(),
            "glob:~/design-docs/**".to_string(),
            "regex:/data/archives/.*".to_string(),
        ]
    );
}

/// A `[read_paths]` section without an `allow` key parses as an empty
/// declaration list - present but granting nothing to ask for.
#[test]
fn parse_manifest_read_paths_without_allow_is_empty() {
    let toml = r#"
[agent]
name = "rp-empty"

[read_paths]

[stages.plan]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    assert!(bp.read_paths.as_ref().unwrap().allow.is_empty());
}

#[test]
fn parse_manifest_safe_commands_declarations() {
    let toml = r#"
[agent]
name = "sc-test"

[safe_commands]
tools = ["web_fetch"]
shell = ["cargo test", "rg"]

[stages.plan]
mode = "autonomous"
"#;
    let sc = parse_manifest(toml).unwrap().safe_commands.unwrap();
    assert_eq!(sc.tools, vec!["web_fetch".to_string()]);
    assert_eq!(sc.shell, vec!["cargo test".to_string(), "rg".to_string()]);
}

/// A `[safe_commands]` section with neither key parses as a present but
/// empty declaration, exactly as `[read_paths]` does.
#[test]
fn parse_manifest_safe_commands_without_entries_is_empty() {
    let toml = r#"
[agent]
name = "sc-empty"

[safe_commands]

[stages.plan]
mode = "autonomous"
"#;
    let sc = parse_manifest(toml).unwrap().safe_commands.unwrap();
    assert!(sc.tools.is_empty());
    assert!(sc.shell.is_empty());
}

/// No section at all means no declarations.
#[test]
fn parse_manifest_without_safe_commands_leaves_none() {
    let toml = r#"
[agent]
name = "sc-none"

[stages.plan]
mode = "autonomous"
"#;
    assert!(parse_manifest(toml).unwrap().safe_commands.is_none());
}

/// A non-string entry fails the whole parse. Skipping it would turn a list
/// that silently lost a member into a grant the author believes they made,
/// which is the one direction this must never fail in.
#[test]
fn parse_manifest_safe_commands_rejects_a_non_string_entry() {
    for field in ["tools", "shell"] {
        let toml = format!(
            r#"
[agent]
name = "sc-bad"

[safe_commands]
{field} = ["ok", 42]

[stages.plan]
mode = "autonomous"
"#
        );
        let err = parse_manifest(&toml).unwrap_err().to_string();
        assert!(err.contains("must be strings"), "{field}: {err}");
    }
}

/// No `[read_paths]` section means no declarations at all - the field
/// stays `None` and the workdir sandbox is unchanged.
#[test]
fn parse_manifest_without_read_paths_leaves_none() {
    let toml = r#"
[agent]
name = "rp-none"

[stages.plan]
mode = "autonomous"
"#;
    assert!(parse_manifest(toml).unwrap().read_paths.is_none());
}

/// A malformed entry fails the whole parse - a skipped entry would
/// degrade the agent silently at its first out-of-workdir read.
#[test]
fn parse_manifest_refuses_invalid_read_path_entries() {
    for (entry, expect) in [
        (r#""glob:[""#, "invalid glob"),
        (r#""regex:relative/.*""#, "must start with"),
        (r#"42"#, "must be strings"),
    ] {
        let toml = format!(
            r#"
[agent]
name = "rp-bad"

[read_paths]
allow = [{entry}]

[stages.plan]
mode = "autonomous"
"#
        );
        let err = parse_manifest(&toml).unwrap_err().to_string();
        assert!(err.contains(expect), "{entry}: {err}");
    }
}

#[test]
fn parse_manifest_agent_and_stage_batch_tool_hint() {
    let toml = r#"
[agent]
name = "batch-test"
batch_tool_hint = true

[stages.plan]
mode = "autonomous"
batch_tool_hint = false

[stages.build]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    // Agent-level `[agent] batch_tool_hint` parsed.
    assert_eq!(bp.batch_tool_hint, Some(true));
    // Stage-level override opts this stage out.
    assert_eq!(bp.find_stage("plan").unwrap().batch_tool_hint, Some(false));
    // A stage with no override inherits (None).
    assert_eq!(bp.find_stage("build").unwrap().batch_tool_hint, None);
    // End-to-end cascade: plan resolves off, build inherits the agent's on.
    assert!(!crate::taint::resolve_batch_tool_hint(
        true,
        bp.batch_tool_hint,
        bp.find_stage("plan").unwrap().batch_tool_hint,
    ));
    assert!(crate::taint::resolve_batch_tool_hint(
        true,
        bp.batch_tool_hint,
        bp.find_stage("build").unwrap().batch_tool_hint,
    ));
}

#[test]
fn parse_manifest_no_batch_tool_hint_is_none() {
    // No `batch_tool_hint` anywhere ⇒ both levels None ⇒ inherit global.
    let toml = r#"
[agent]
name = "no-batch"

[stages.main]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    assert_eq!(bp.batch_tool_hint, None);
    assert_eq!(bp.find_stage("main").unwrap().batch_tool_hint, None);
}

#[test]
fn parse_manifest_agent_and_stage_shell_hint() {
    let toml = r#"
[agent]
name = "shell-test"
shell_hint = false

[stages.plan]
mode = "autonomous"
shell_hint = true

[stages.build]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    // Agent-level `[agent] shell_hint` parsed, and independent of the
    // batch hint sharing the cascade shape.
    assert_eq!(bp.shell_hint, Some(false));
    assert_eq!(bp.batch_tool_hint, None);
    // Stage-level override opts this one stage back in.
    assert_eq!(bp.find_stage("plan").unwrap().shell_hint, Some(true));
    // A stage with no override inherits (None).
    assert_eq!(bp.find_stage("build").unwrap().shell_hint, None);
    // End-to-end cascade: plan resolves on despite the agent and global
    // both being off, build follows the agent's off despite the global on.
    assert!(crate::taint::resolve_shell_hint(
        false,
        bp.shell_hint,
        bp.find_stage("plan").unwrap().shell_hint,
    ));
    assert!(!crate::taint::resolve_shell_hint(
        true,
        bp.shell_hint,
        bp.find_stage("build").unwrap().shell_hint,
    ));
}

#[test]
fn parse_manifest_no_shell_hint_is_none() {
    // No `shell_hint` anywhere ⇒ both levels None ⇒ inherit global.
    let toml = r#"
[agent]
name = "no-shell-hint"

[stages.main]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    assert_eq!(bp.shell_hint, None);
    assert_eq!(bp.find_stage("main").unwrap().shell_hint, None);
    assert!(crate::taint::resolve_shell_hint(true, None, None));
    assert!(!crate::taint::resolve_shell_hint(false, None, None));
}

#[test]
fn parse_manifest_agent_and_stage_nudge_blocks() {
    let toml = r#"
[agent]
name = "nudge-test"

[agent.nudge]
max = 2

[stages.plan]
mode = "autonomous"

[stages.plan.nudge]
enabled = false

[stages.implement]
mode = "autonomous"

[stages.implement.nudge]
max = 5
text = "  Edit the files named in {regions}.  "

[stages.review]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    // Agent-level `[agent.nudge]` parsed; unset fields stay None.
    let agent_nudge = bp.nudge.as_ref().unwrap();
    assert_eq!(agent_nudge.enabled, None);
    assert_eq!(agent_nudge.max, Some(2));
    assert_eq!(agent_nudge.text, None);
    // Stage-level blocks: each carries only what it names.
    let plan = bp.find_stage("plan").unwrap().nudge.as_ref().unwrap();
    assert_eq!(plan.enabled, Some(false));
    assert_eq!(plan.max, None);
    let implement = bp.find_stage("implement").unwrap().nudge.as_ref().unwrap();
    assert_eq!(implement.max, Some(5));
    // Text is trimmed, placeholders kept verbatim for the runtime.
    assert_eq!(
        implement.text.as_deref(),
        Some("Edit the files named in {regions}.")
    );
    // A stage with no block inherits (None).
    assert!(bp.find_stage("review").unwrap().nudge.is_none());
}

#[test]
fn parse_manifest_nudge_ignores_bad_types() {
    // An empty block is inert; wrong-typed values fall back to inheriting
    // rather than misconfiguring the stage. (A negative `max` is refused;
    // see `every_negative_manifest_integer_fails_to_load_naming_the_key`.)
    let toml = r#"
[agent]
name = "nudge-bad"

[agent.nudge]

[stages.main]
mode = "autonomous"

[stages.main.nudge]
enabled = "yes"
max = "many"
text = 7
"#;
    let bp = parse_manifest(toml).unwrap();
    assert_eq!(
        bp.nudge.as_ref().unwrap(),
        &crate::blueprint::NudgeConfig::default()
    );
    assert_eq!(
        bp.find_stage("main").unwrap().nudge.as_ref().unwrap(),
        &crate::blueprint::NudgeConfig::default()
    );
}

#[test]
fn parse_manifest_no_nudge_blocks_is_none() {
    // No nudge block anywhere ⇒ both levels None ⇒ inherit global.
    let toml = r#"
[agent]
name = "no-nudge"

[stages.main]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    assert!(bp.nudge.is_none());
    assert!(bp.find_stage("main").unwrap().nudge.is_none());
}

#[test]
fn parse_manifest_security_block_without_taint_tracking_defaults_true() {
    // A present `[security]` block that omits `taint_tracking` keeps the
    // default (true) - block presence implies intent to configure security.
    let toml = r#"
[agent]
name = "sec-default"

[security]

[stages.main]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    assert!(bp.security.as_ref().unwrap().taint_tracking);
}

#[test]
fn parse_manifest_no_security_is_none() {
    let toml = r#"
[agent]
name = "no-sec"

[stages.main]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    assert!(bp.security.is_none());
    assert!(bp.find_stage("main").unwrap().security.is_none());
}

#[test]
fn parse_manifest_interaction_point_no_directives_defaults_empty() {
    let toml = r#"
[agent]
name = "no-directive-test"

[stages.main]
mode = "interactive_points"

[[stages.main.interaction_points]]
name     = "confirm"
prompt   = "Proceed?"
style    = "confirm"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    let points = unwrap_interactive_points(&stage.mode);
    assert!(points[0].directives.is_empty());
    assert!(points[0].abort_options.is_empty());
    assert!(points[0].edit_options.is_empty());
}

#[test]
fn parse_manifest_stage_allow_complete() {
    let toml = r#"
[agent]
name = "allow-complete-test"

[stages.review]
mode = "autonomous"
allow_complete = true
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("review").unwrap();
    assert!(stage.allow_complete);
}

/// `allow_blocking_tools` is read off the stage table and defaults to false
/// for a stage that says nothing about it.
#[test]
fn parse_manifest_allow_blocking_tools() {
    let toml = r#"
[agent]
name = "blocking-test"

[stages.implement]
mode = "autonomous"
available_tools = ["ask_user_confirm"]
allow_blocking_tools = true

[stages.review]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    assert!(bp.find_stage("implement").unwrap().allow_blocking_tools);
    assert!(!bp.find_stage("review").unwrap().allow_blocking_tools);
}

/// `available_global_tools` is read off the stage table: `true` opts the
/// stage in, an explicit `false` and an absent key both leave it off.
#[test]
fn parse_manifest_available_global_tools() {
    let toml = r#"
[agent]
name = "global-tools-test"

[stages.implement]
mode = "autonomous"
available_tools = ["read_file"]
available_global_tools = true

[stages.review]
mode = "autonomous"
available_global_tools = false

[stages.summary]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    assert!(bp.find_stage("implement").unwrap().available_global_tools);
    assert!(!bp.find_stage("review").unwrap().available_global_tools);
    assert!(!bp.find_stage("summary").unwrap().available_global_tools);
    // The parser only records the flag; the grant list itself is untouched
    // until the daemon resolves the global inventory at spawn.
    assert_eq!(
        bp.find_stage("implement").unwrap().available_tools,
        vec!["read_file".to_string()]
    );
}

/// A non-boolean `available_global_tools` is not a grant: exactly like
/// `allow_blocking_tools`, a value of the wrong type reads as unset.
#[test]
fn parse_manifest_available_global_tools_ignores_a_non_bool() {
    let toml = r#"
[agent]
name = "global-tools-test"

[stages.main]
mode = "autonomous"
available_global_tools = "yes"
"#;
    let bp = parse_manifest(toml).unwrap();
    assert!(!bp.find_stage("main").unwrap().available_global_tools);
}

#[test]
fn parse_manifest_fan_out_stage() {
    let toml = r#"
[agent]
name = "fanout-test"

[stages.parallel]
mode = "fan_out"
worker_stage = "worker"
merge_stage = "merge"
max_workers = 7
on_worker_failure = "fail_all"
split_prompt = "split the work"

[stages.worker]
mode = "autonomous"
allow_as_worker = true

[stages.merge]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    // Compare the whole mode (no never-taken fallback arm to leave uncovered).
    let expected = crate::blueprint::StageMode::FanOut {
        config: crate::blueprint::FanOutConfig {
            worker_agent: None,
            worker_stage: Some("worker".to_string()),
            worker_query: None,
            merge_stage: Some("merge".to_string()),
            max_workers: 7,
            on_worker_failure: crate::blueprint::WorkerFailurePolicy::FailAll,
            split_prompt: "split the work".to_string(),
            results_region: None,
            max_items: None,
            max_attempts: None,
        },
    };
    assert_eq!(bp.find_stage("parallel").unwrap().mode, expected);
    assert!(bp.find_stage("worker").unwrap().allow_as_worker);
    // Defaults: unspecified fan_out fields.
    assert!(!bp.find_stage("merge").unwrap().allow_as_worker);
}

#[test]
fn parse_manifest_fan_out_defaults() {
    let toml = r#"
[agent]
name = "fanout-defaults"

[stages.parallel]
mode = "fan_out"
worker_agent = "external-worker"
split_prompt = "go"
"#;
    let bp = parse_manifest(toml).unwrap();
    let expected = crate::blueprint::StageMode::FanOut {
        config: crate::blueprint::FanOutConfig {
            worker_agent: Some("external-worker".to_string()),
            worker_stage: None,
            worker_query: None,
            merge_stage: None,
            max_workers: crate::blueprint::DEFAULT_MAX_WORKERS,
            on_worker_failure: crate::blueprint::WorkerFailurePolicy::Continue,
            split_prompt: "go".to_string(),
            results_region: None,
            max_items: None,
            max_attempts: None,
        },
    };
    assert_eq!(bp.find_stage("parallel").unwrap().mode, expected);
}

/// `results_region` names where the workers' reports land, and `max_items`
/// caps how many slices the split may produce. The cap matters because each
/// worker's share of the region is the region's budget divided by how many
/// there are: past some count every share is too small to carry rows, and a
/// stated ceiling says so instead of letting the shares shrink to nothing.
#[test]
fn parse_manifest_fan_out_results_region_and_max_items() {
    let toml = r#"
[agent]
name = "fanout-region"

[stages.parallel]
mode = "fan_out"
worker_agent = "w"
split_prompt = "go"
results_region = "worker_rows"
max_items = 12
"#;
    // The whole mode, so there is no never-taken match arm left behind.
    let bp = parse_manifest(toml).unwrap();
    let expected = crate::blueprint::StageMode::FanOut {
        config: crate::blueprint::FanOutConfig {
            worker_agent: Some("w".to_string()),
            worker_stage: None,
            worker_query: None,
            merge_stage: None,
            max_workers: crate::blueprint::DEFAULT_MAX_WORKERS,
            on_worker_failure: crate::blueprint::WorkerFailurePolicy::Continue,
            split_prompt: "go".to_string(),
            results_region: Some("worker_rows".to_string()),
            max_items: Some(12),
            max_attempts: None,
        },
    };
    assert_eq!(bp.find_stage("parallel").unwrap().mode, expected);
}

/// `0` is the manifest's word for "unlimited" on both caps: `max_items = 0`
/// reads as no ceiling on how many items the split may produce (the same as
/// leaving the key out), and `max_workers = 0` keeps the zero so the runtime
/// starts every item at once. Neither is a fan-out that can never run.
#[test]
fn parse_manifest_fan_out_zero_caps_mean_unlimited() {
    let toml = r#"
[agent]
name = "fanout-cap"

[stages.parallel]
mode = "fan_out"
worker_agent = "w"
split_prompt = "go"
max_items = 0
max_workers = 0
"#;
    let bp = parse_manifest(toml).unwrap();
    let expected = crate::blueprint::StageMode::FanOut {
        config: crate::blueprint::FanOutConfig {
            worker_agent: Some("w".to_string()),
            worker_stage: None,
            worker_query: None,
            merge_stage: None,
            max_workers: 0,
            on_worker_failure: crate::blueprint::WorkerFailurePolicy::Continue,
            split_prompt: "go".to_string(),
            results_region: None,
            max_items: None,
            max_attempts: None,
        },
    };
    let stage = bp.find_stage("parallel").unwrap();
    assert_eq!(stage.mode, expected);
    let crate::blueprint::StageMode::FanOut { config } = &stage.mode else {
        unreachable!("asserted equal to a fan-out above");
    };
    assert_eq!(config.worker_cap(), None);
}

/// A negative or non-numeric cap is a mistake the author should hear about at
/// parse time. `max_workers = -1` would wrap to the largest `usize` and run
/// unbounded, and `max_items = "twelve"` would read as no cap at all; both
/// show up only as a fan-out wider than the manifest appears to allow.
#[test]
fn parse_manifest_fan_out_rejects_a_negative_or_non_numeric_cap() {
    for (key, value, wants) in [
        ("max_items", "-3", "must not be negative"),
        ("max_items", "\"twelve\"", "must be a whole number"),
        ("max_workers", "-1", "must not be negative"),
        ("max_workers", "\"lots\"", "must be a whole number"),
    ] {
        let toml = format!(
            r#"
[agent]
name = "fanout-cap"

[stages.parallel]
mode = "fan_out"
worker_agent = "w"
split_prompt = "go"
{key} = {value}
"#
        );
        let err = parse_manifest(&toml).unwrap_err().to_string();
        assert!(
            err.contains(&format!("stage 'parallel': {key} {wants}")),
            "{key} = {value}: {err}"
        );
        assert!(err.contains("0 means unlimited"), "{key} = {value}: {err}");
    }
}

#[test]
fn parse_manifest_stage_allow_complete_defaults_false() {
    let toml = r#"
[agent]
name = "allow-complete-default-test"

[stages.review]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("review").unwrap();
    assert!(!stage.allow_complete);
}

#[test]
fn parse_manifest_transition_gate_reads_every_key() {
    let toml = r#"
[agent]
name = "gate-test"

[stages.implement]
mode = "autonomous"
available_tools = ["write_file"]

[stages.implement.transitions.review]
hint = "done"
gate = { require_modifications = true, message = "  write something!  ", region = "implementation", tools = ["patch_file", 7], max_attempts = 2 }

[stages.review]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    let gate = bp
        .find_stage("implement")
        .unwrap()
        .transitions
        .as_ref()
        .unwrap()["review"]
        .gate
        .as_ref()
        .unwrap();
    assert!(gate.require_modifications);
    assert_eq!(gate.message.as_deref(), Some("write something!"));
    assert_eq!(gate.region.as_deref(), Some("implementation"));
    // Non-string entries in `tools` are skipped, not an error.
    assert_eq!(gate.tools, vec!["patch_file".to_string()]);
    assert_eq!(gate.max_attempts, Some(2));
}

#[test]
fn parse_manifest_transition_gate_defaults_and_ignores_wrong_types() {
    let toml = r#"
[agent]
name = "gate-default-test"

[stages.a]
mode = "autonomous"

[stages.a.transitions.b]
hint = "no gate here"

[stages.a.transitions.c]
gate = { require_modifications = "yes", message = 3, region = [], tools = "write_file", max_attempts = "four" }

[stages.b]
mode = "autonomous"

[stages.c]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    let transitions = bp.find_stage("a").unwrap().transitions.as_ref().unwrap();
    // An edge with no `gate` table has no gate at all.
    assert!(transitions["b"].gate.is_none());
    // A gate whose every key is the wrong type falls back to the defaults,
    // i.e. a gate that blocks nothing rather than one that silently never
    // holds. (A negative attempt budget is refused instead; see
    // `every_negative_manifest_integer_fails_to_load_naming_the_key`.)
    let gate = transitions["c"].gate.as_ref().unwrap();
    assert_eq!(gate, &crate::blueprint::TransitionGate::default());
    // Zero, on the other hand, is a deliberate "record it but never hold".
    let toml = toml.replace("max_attempts = \"four\"", "max_attempts = 0");
    let bp = parse_manifest(&toml).unwrap();
    assert_eq!(
        bp.find_stage("a").unwrap().transitions.as_ref().unwrap()["c"]
            .gate
            .as_ref()
            .unwrap()
            .max_attempts,
        Some(0)
    );
}

#[test]
fn parse_manifest_stage_accepts_messages_false() {
    let toml = r#"
[agent]
name = "accepts-messages-test"

[stages.report]
mode = "autonomous"
accepts_messages = false
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("report").unwrap();
    assert!(!stage.accepts_messages);
}

#[test]
fn parse_manifest_stage_accepts_messages_defaults_true() {
    let toml = r#"
[agent]
name = "accepts-messages-default-test"

[stages.report]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("report").unwrap();
    assert!(stage.accepts_messages);
}

#[test]
fn parse_manifest_with_compaction_config() {
    let toml = r#"
[agent]
name = "compact-test"

[compaction]
provider = "openai"
model = "gpt-4o-mini"
system_prompt = "Summarize concisely"
max_summary_tokens = 500
temperature = 0.2
"#;
    let bp = parse_manifest(toml).unwrap();
    let cc = bp.compaction_config.as_ref().unwrap();
    assert_eq!(cc.provider, "openai");
    assert_eq!(cc.model, "gpt-4o-mini");
    assert_eq!(cc.system_prompt.as_deref(), Some("Summarize concisely"));
    assert_eq!(cc.max_summary_tokens, 500);
    assert!((cc.temperature - 0.2).abs() < 0.01);
}

#[test]
fn parse_manifest_with_custom_edge_transform() {
    let toml = r#"
[agent]
name = "custom-edge"

[stages.a]
mode = "autonomous"

[stages.a.transitions.b]
transform = "custom"

[stages.a.transitions.b.transform_config]
carry = ["system"]
compact = ["conversation"]
clear = ["scratch"]
compact_prompt = "Summarize for next stage"

[stages.b]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage_a = bp.find_stage("a").unwrap();
    let transitions = stage_a.transitions.as_ref().unwrap();
    let edge = transitions.get("b").unwrap();
    assert_eq!(
        edge.transform,
        EdgeTransform::Custom {
            carry: vec!["system".to_string()],
            compact: vec!["conversation".to_string()],
            clear: vec!["scratch".to_string()],
            compact_prompt: Some("Summarize for next stage".to_string()),
        }
    );
}

#[test]
fn parse_manifest_with_agent_tool_permissions() {
    let toml = r#"
[agent]
name = "perm-test"

[tool_permissions]
bash = "ask"
write_file = "deny"
read_file = "allow"
"#;
    let bp = parse_manifest(toml).unwrap();
    assert_eq!(
        bp.metadata.get("tool_perm:bash").and_then(|v| v.as_str()),
        Some("ask")
    );
    assert_eq!(
        bp.metadata
            .get("tool_perm:write_file")
            .and_then(|v| v.as_str()),
        Some("deny")
    );
    assert_eq!(
        bp.metadata
            .get("tool_perm:read_file")
            .and_then(|v| v.as_str()),
        Some("allow")
    );
}

#[test]
fn parse_manifest_error_missing_agent_section() {
    let toml = r#"
[stages.main]
mode = "autonomous"
"#;
    let result = parse_manifest(toml);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Missing [agent] section")
    );
}

#[test]
fn parse_manifest_error_invalid_toml() {
    let result = parse_manifest("not valid toml {{{}}}");
    assert!(result.is_err());
}

#[test]
fn parse_manifest_default_regions_when_none_specified() {
    let toml = r#"
[agent]
name = "no-regions"
"#;
    let bp = parse_manifest(toml).unwrap();
    assert_eq!(bp.context_layout.regions.len(), 2); // system + conversation
    assert_eq!(bp.context_layout.regions[0].name, "system");
    assert_eq!(bp.context_layout.regions[0].kind, RegionKind::Pinned);
    assert_eq!(bp.context_layout.regions[1].name, "conversation");
    assert_eq!(
        bp.context_layout.regions[1].kind,
        RegionKind::SlidingWindow {
            max_items: 10,
            eviction_strategy: EvictionStrategy::default(),
        }
    );
}

#[test]
fn parse_manifest_unknown_region_kind_is_a_hard_error() {
    // A typo'd kind must not fold into Temporary: for a custom region
    // that would mean the script never runs, with no signal. The error
    // names the region, the bad value, and the valid kinds.
    let toml = r#"
[agent]
name = "unknown-kind"

[context.regions]
test = { kind = "cusotm", max_tokens = 1000 }
"#;
    let err = parse_manifest(toml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("region 'test'"), "{msg}");
    assert!(msg.contains("cusotm"), "{msg}");
    assert!(msg.contains("valid kinds"), "{msg}");
}

#[test]
fn parse_manifest_stage_modes() {
    let toml = r#"
[agent]
name = "modes-test"

[stages.auto]
mode = "autonomous"

[stages.inter]
mode = "interactive"

[stages.default_mode]
"#;
    let bp = parse_manifest(toml).unwrap();
    let auto = bp.find_stage("auto").unwrap();
    assert_eq!(auto.mode, StageMode::Autonomous);

    let inter = bp.find_stage("inter").unwrap();
    assert_eq!(inter.mode, StageMode::Interactive);

    // Default mode (no mode specified) - Autonomous
    let default = bp.find_stage("default_mode").unwrap();
    assert_eq!(default.mode, StageMode::Autonomous);
}

/// A misspelled mode must not fold into an autonomous stage: a stage written
/// to produce an output would run normally and never ask for one. Region
/// kinds reject an unknown value for the same reason.
#[test]
fn parse_manifest_rejects_an_unknown_stage_mode() {
    let toml = r#"
[agent]
name = "typo-test"

[stages.summary]
mode = "outupt"
"#;
    let err = parse_manifest(toml).expect_err("a misspelled mode is not autonomous");
    let msg = err.to_string();
    assert!(msg.contains("summary"), "names the stage: {msg}");
    assert!(msg.contains("outupt"), "quotes the bad value: {msg}");
    assert!(msg.contains("valid modes"), "lists the alternatives: {msg}");
}

/// `mode = "output"` is sugar for three settings. They are written onto the
/// Stage at parse time rather than special-cased at dispatch, so the tool
/// filter, `lev validate`, and the lint all read one honest list.
#[test]
fn output_mode_grants_the_submit_tool_and_requires_an_output() {
    let toml = r#"
[agent]
name = "output-test"

[stages.summary]
mode = "output"
"#;
    let bp = parse_manifest(toml).expect("parses");
    let stage = bp.find_stage("summary").expect("the stage exists");
    assert_eq!(stage.mode, StageMode::Output);
    assert!(stage.require_output);
    assert!(
        stage
            .available_tools
            .contains(&crate::blueprint::SUBMIT_OUTPUT_TOOL.to_string())
    );
    // An output stage is normally the last thing a run does.
    assert!(stage.allow_complete);
    // And the grant survives validation, which would otherwise reject a
    // stage required to produce an output it cannot submit.
    bp.validate().expect("the auto-grant satisfies validation");
}

/// `max_attempts` is how many times a fan-out stage is asked again before it is
/// let through without workers. Absent means the framework default; `0` means
/// let it through on the first refusal.
#[test]
fn fan_out_max_attempts_is_read_from_the_stage() {
    let with = |line: &str| {
        let toml = format!(
            r#"
[agent]
name = "attempts-test"

[stages.split]
mode = "fan_out"
worker_stage = "work"
split_prompt = "split it"
{line}

[stages.work]
mode = "autonomous"
allow_as_worker = true
"#
        );
        let bp = parse_manifest(&toml).expect("parses");
        match &bp.find_stage("split").expect("the stage exists").mode {
            StageMode::FanOut { config } => config.max_attempts,
            other => panic!("expected a fan-out stage, got {other:?}"),
        }
    };
    assert_eq!(with(""), None, "absent leaves the default to the runtime");
    assert_eq!(with("max_attempts = 7"), Some(7));
    assert_eq!(with("max_attempts = 0"), Some(0), "zero is meaningful here");
}

/// A typo in the budget is refused rather than read as "no budget", and the
/// message says what zero would have meant for this key rather than for the
/// two caps that share the reader.
#[test]
fn a_bad_max_attempts_names_what_zero_means_here() {
    let toml = r#"
[agent]
name = "attempts-test"

[stages.split]
mode = "fan_out"
worker_stage = "work"
split_prompt = "split it"
max_attempts = "three"

[stages.work]
mode = "autonomous"
allow_as_worker = true
"#;
    let err = parse_manifest(toml).expect_err("refused").to_string();
    assert!(err.contains("max_attempts"), "{err}");
    assert!(err.contains("do not ask again"), "{err}");
}

/// A `fan_out` stage is granted `fan_out` regardless of what it listed, and
/// listing it by hand does not produce two of them.
///
/// The grant ignores `available_tools` because a fan-out stage's is usually
/// `[]` - the split runs no tools of its own - and that empty list is a
/// statement about the work, not about how the answer comes back.
#[test]
fn fan_out_mode_grants_the_fan_out_tool_exactly_once() {
    let bare = r#"
[agent]
name = "fanout-test"

[stages.split]
mode = "fan_out"
worker_stage = "work"
available_tools = []
split_prompt = "split it"

[stages.work]
mode = "autonomous"
allow_as_worker = true
"#;
    let stage = parse_manifest(bare)
        .expect("parses")
        .find_stage("split")
        .cloned()
        .expect("the stage exists");
    assert_eq!(
        stage.available_tools,
        vec![crate::blueprint::FAN_OUT_TOOL.to_string()],
        "granted even though the author wrote an empty list"
    );

    let spelled_out = bare.replace(
        "available_tools = []",
        "available_tools = [\"fan_out\", \"read_file\"]",
    );
    let stage = parse_manifest(&spelled_out)
        .expect("parses")
        .find_stage("split")
        .cloned()
        .expect("the stage exists");
    assert_eq!(
        stage.available_tools,
        vec![
            crate::blueprint::FAN_OUT_TOOL.to_string(),
            "read_file".to_string()
        ],
        "the author's own list is kept and the grant is not duplicated"
    );
}

/// The auto-grant appends rather than replaces, and does not duplicate a
/// tool the author already listed.
#[test]
fn output_mode_keeps_the_authors_own_tools_and_does_not_duplicate() {
    let toml = r#"
[agent]
name = "output-test"

[stages.summary]
mode = "output"
available_tools = ["read_file", "submit_output"]
allow_complete = false
"#;
    let bp = parse_manifest(toml).expect("parses");
    let stage = bp.find_stage("summary").expect("the stage exists");
    assert_eq!(
        stage.available_tools,
        vec!["read_file".to_string(), "submit_output".to_string()]
    );
    // An author who routes onward is believed.
    assert!(!stage.allow_complete);
}

/// An ordinary stage can be made to hand something back, which is how a
/// fan-out worker guarantees its merge stage something to merge.
#[test]
fn require_output_works_on_a_stage_that_is_not_an_output_stage() {
    let toml = r#"
[agent]
name = "worker-test"

[stages.fix_worker]
mode = "autonomous"
available_tools = ["edit_file", "submit_output"]
require_output = true
"#;
    let bp = parse_manifest(toml).expect("parses");
    let stage = bp.find_stage("fix_worker").expect("the stage exists");
    assert!(stage.require_output);
    assert_eq!(stage.mode, StageMode::Autonomous);
    bp.validate().expect("the stage can submit");
}

/// An output shape is read at both levels, and `format` is taken as an
/// opaque string: a value this parser has never heard of is as valid as
/// `"markdown"`, which is what lets a2ui work with no code support.
#[test]
fn output_specs_parse_at_agent_and_stage_level_with_an_opaque_format() {
    let toml = r#"
[agent]
name = "shape-test"

[agent.output]
format = "markdown"
instructions = "Two sentences, no preamble."

[stages.summary]
mode = "output"

[stages.summary.output]
format = "a2ui"
example = "{\"root\": {\"component\": \"Card\"}}"

[stages.summary.output.schema]
type = "object"
"#;
    let bp = parse_manifest(toml).expect("parses");
    let agent_spec = bp.output.as_ref().expect("the agent declares a shape");
    assert_eq!(agent_spec.format.as_deref(), Some("markdown"));
    assert_eq!(
        agent_spec.instructions.as_deref(),
        Some("Two sentences, no preamble.")
    );

    let stage_spec = bp
        .find_stage("summary")
        .expect("the stage exists")
        .output
        .as_ref()
        .expect("the stage narrows the shape");
    // An unrecognized format is carried through untouched.
    assert_eq!(stage_spec.format.as_deref(), Some("a2ui"));
    assert_eq!(
        stage_spec.example.as_deref(),
        Some("{\"root\": {\"component\": \"Card\"}}")
    );
    // A schema written as an inline TOML table becomes JSON.
    assert_eq!(
        stage_spec.schema,
        Some(serde_json::json!({"type": "object"}))
    );

    // And the two levels combine the way the cascade says.
    let resolved = crate::output::resolve_output_spec(bp.output.as_ref(), Some(stage_spec), None)
        .expect("some level asked for an output");
    assert_eq!(resolved.format.as_deref(), Some("a2ui"));
    assert_eq!(
        resolved.instructions.as_deref(),
        Some("Two sentences, no preamble.")
    );
}

/// The validator-error policy is read at both levels, and only the two spelled
/// values load.
#[test]
fn on_validator_error_parses_at_agent_and_stage_level() {
    let toml = r#"
[agent]
name = "policy-test"

[agent.output]
format = "a2ui"
validator = "v.rhai"
on_validator_error = "accept"

[stages.summary]
mode = "output"

[stages.summary.output]
on_validator_error = "reject"
"#;
    let bp = parse_manifest(toml).expect("parses");
    assert_eq!(
        bp.output.as_ref().and_then(|s| s.on_validator_error),
        Some(crate::output::OnValidatorError::Accept)
    );
    assert_eq!(
        bp.find_stage("summary")
            .expect("the stage exists")
            .output
            .as_ref()
            .and_then(|s| s.on_validator_error),
        Some(crate::output::OnValidatorError::Reject)
    );
}

/// A policy this parser does not recognise is a load error, not a silent
/// fallback: `"sometimes"` would otherwise load as the default and change what
/// happens to the run's answer.
#[test]
fn an_unknown_validator_error_policy_is_refused_at_load() {
    let toml = r#"
[agent]
name = "policy-test"

[agent.output]
on_validator_error = "sometimes"
"#;
    let err = parse_manifest(toml).expect_err("refused").to_string();
    assert!(err.contains("[agent.output]"), "{err}");
    assert!(err.contains("\"reject\" or \"accept\""), "{err}");
    assert!(err.contains("sometimes"), "{err}");
}

/// The wrong type is refused with the same error, naming the stage whose table
/// carried it. Reading only `as_str` would silently ignore a non-string value.
#[test]
fn a_non_string_validator_error_policy_is_refused_at_load() {
    let toml = r#"
[agent]
name = "policy-test"

[stages.summary]
mode = "output"

[stages.summary.output]
on_validator_error = 3
"#;
    let err = parse_manifest(toml).expect_err("refused").to_string();
    assert!(err.contains("stage 'summary': output"), "{err}");
    assert!(err.contains("got: 3"), "{err}");
}

#[test]
fn parse_manifest_with_stage_system_prompt() {
    let toml = r#"
[agent]
name = "prompt-test"

[stages.main]
system_prompt = "  You are helpful.  "
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    let sp = stage.config.get("system_prompt").unwrap().as_str().unwrap();
    assert_eq!(sp, "You are helpful.");
}

#[test]
fn parse_manifest_summarize_transform_alias() {
    let toml = r#"
[agent]
name = "alias-test"

[stages.a]
mode = "autonomous"

[stages.a.transitions.b]
transform = "summarize"

[stages.b]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage_a = bp.find_stage("a").unwrap();
    let transitions = stage_a.transitions.as_ref().unwrap();
    let edge = transitions.get("b").unwrap();
    assert_eq!(edge.transform, EdgeTransform::Compact { prompt: None });
}

// ─── Regression: the shipped coding agent must branch on plan_approval ──
//
// The "plan" stage's plan_approval interaction point lets the user pick
// Approve / Revise / Add detail / Abort. If "plan" only has a single
// outgoing transition edge, resolve_transition() auto-follows it without
// ever consulting the LLM - so anything other than "Approve" is silently
// ignored and the run proceeds to "implement" anyway. Guard against that
// regressing by requiring at least two outgoing edges (forcing the
// LLM-consultation path in resolve_transition / prompt_llm_transition).
#[test]
fn coder_plan_stage_branches_on_choice() {
    let manifest_content = include_str!("../../../leviath-cli/agents/coder/agent.leviath");
    let bp = parse_manifest(manifest_content).unwrap();
    let plan = bp.find_stage("plan").unwrap();

    let transitions = plan
        .transitions
        .as_ref()
        .expect("plan stage must declare transitions");
    // plan stage must have >=2 outgoing edges so the user's plan_approval
    // choice (Revise/Add detail/Abort) actually changes behavior instead
    // of being silently ignored by a single-edge auto-transition.
    assert!(transitions.len() >= 2);
    assert!(transitions.contains_key("implement"));

    // A self-loop (or other non-"implement" edge) must exist so revising/
    // aborting doesn't fall through to implementation.
    assert!(transitions.keys().any(|t| t != "implement"));

    // The self-loop must be revisit-capped to avoid an infinite planning loop.
    // plan stage must have a self-loop ('plan' transition) so the user can revise.
    assert!(transitions.contains_key("plan"));
    // self-looping 'plan' stage must cap max_revisits.
    assert!(plan.max_revisits.is_some());
}

/// An implement stage that can leave without having written anything is
/// how a run ends up with no output at all. Every edge out of
/// `implement` that the AGENT can choose must carry a `require_modifications`
/// gate, and the write tools must be routed into the region that gate points
/// at (which is also what the reviewer is told to read).
///
/// Runtime-fired edges (`error`, `stuck`) are exempt, and deliberately so: a
/// gate answers "did you do any work before leaving?", which only makes sense
/// for a voluntary exit. Gating an escape hatch would send a failed or
/// looping agent back into the stage it is failing in - a stuck agent is not
/// helped by being told to write more.
#[test]
fn shipped_coding_agents_gate_every_non_error_implement_edge() {
    for manifest_content in [include_str!(
        "../../../leviath-cli/agents/coder/agent.leviath"
    )] {
        let bp = parse_manifest(manifest_content).unwrap();
        bp.validate().unwrap();
        let implement = bp.find_stage("implement").unwrap();
        assert!(
            implement.available_tools.iter().any(|t| t == "write_file")
                && implement.available_tools.iter().any(|t| t == "edit_file"),
            "{}: implement must advertise both write tools",
            bp.name
        );
        let transitions = implement.transitions.as_ref().unwrap();
        for (target, edge) in transitions {
            // Recovery must stay reachable from a failed stage, a stuck escape
            // from a looping one, and a dead-end escape from a stage whose
            // every other way out is revisit-exhausted. None of the three is a
            // route the model picks, so a gate on one could only block the
            // escape it exists to provide.
            if matches!(
                edge.condition,
                crate::blueprint::TransitionCondition::Error
                    | crate::blueprint::TransitionCondition::Stuck
                    | crate::blueprint::TransitionCondition::DeadEnd
            ) {
                continue;
            }
            assert!(
                edge.gate.is_some(),
                "{}: implement → {target} has no gate",
                bp.name
            );
            let gate = edge.gate.as_ref().unwrap();
            assert!(gate.require_modifications, "{}: → {target}", bp.name);
            assert_eq!(
                gate.region.as_deref(),
                Some("implementation"),
                "{}: → {target} must accept the persisted region as evidence",
                bp.name
            );
        }
        // ...and the writes actually land in that region.
        let overrides = &implement
            .tool_result_routing
            .as_ref()
            .expect("implement must route tool results")
            .tool_overrides;
        for tool in ["write_file", "edit_file"] {
            assert_eq!(
                overrides.get(tool).map(String::as_str),
                Some("implementation"),
                "{}: {tool} results must persist in `implementation`",
                bp.name
            );
        }
    }
}

#[test]
fn coder_plan_routes_errors_and_cannot_end_the_run() {
    let manifest_content = include_str!("../../../leviath-cli/agents/coder/agent.leviath");
    let bp = parse_manifest(manifest_content).unwrap();
    let plan = bp.find_stage("plan").unwrap();
    let transitions = plan.transitions.as_ref().unwrap();

    // plan stage should route errors to error_recovery, like implement/review do.
    assert!(
        transitions
            .get("error_recovery")
            .map(|e| e.condition == crate::blueprint::TransitionCondition::Error)
            .unwrap_or(false)
    );

    // Planning must NOT be able to end the run. Abort is the engine's path
    // (abort_options on the plan_approval point), so `allow_complete` was
    // only ever a way for the model to stop early - and it did: one that had
    // created a file during `discover` read it back here, decided "already
    // created - no further action is needed", and completed without ever
    // reaching `implement`, taking the user's plan correction with it.
    assert!(!plan.allow_complete);
    // Every way out leads somewhere that does the work.
    for target in ["implement", "prototype"] {
        assert!(
            transitions.contains_key(target),
            "plan must be able to reach {target}"
        );
    }
}

#[test]
fn coder_plan_approval_option_routing() {
    let manifest_content = include_str!("../../../leviath-cli/agents/coder/agent.leviath");
    let bp = parse_manifest(manifest_content).unwrap();
    let plan = bp.find_stage("plan").unwrap();
    let points = unwrap_interactive_points(&plan.mode);
    let approval = points
        .iter()
        .find(|p| p.name == "plan_approval")
        .expect("plan_approval interaction point must exist");

    let opt = |prefix: &str| {
        approval
            .options
            .iter()
            .find(|o| o.starts_with(prefix))
            .expect("interaction-point option with the requested prefix must exist")
            .clone()
    };
    let approve = opt("Approve");
    let revise = opt("Revise");
    let detail = opt("Add detail");
    let abort = opt("Abort");

    // "Revise" carries a directive (agent-driven, calls ask_user_text).
    assert!(approval.directives.contains_key(&revise));
    // "Add detail" is a deterministic edit option (engine opens an editor).
    assert!(approval.edit_options.contains(&detail));
    assert!(!approval.directives.contains_key(&detail));
    // "Abort" is a deterministic abort option.
    assert!(approval.abort_options.contains(&abort));
    // "Approve" is a plain completing option - none of the above.
    assert!(!approval.directives.contains_key(&approve));
    assert!(!approval.abort_options.contains(&approve));
    assert!(!approval.edit_options.contains(&approve));
}

#[test]
fn coder_review_stage_can_finish_and_routes_errors() {
    let manifest_content = include_str!("../../../leviath-cli/agents/coder/agent.leviath");
    let bp = parse_manifest(manifest_content).unwrap();
    let review = bp.find_stage("review").unwrap();

    let transitions = review
        .transitions
        .as_ref()
        .expect("review stage must declare transitions");
    // An approving review must not be forced back into 'implement'. It used
    // to say so with `allow_complete`, which ended the run outright; now it
    // routes to the output stage, so the run still explains what it did.
    // The property is the same one either way: review has somewhere to go
    // that is not more implementation.
    assert!(
        transitions.contains_key("summary"),
        "an approving review needs an exit that is not 'implement'"
    );
    assert!(
        !review.allow_complete,
        "ending the run here would skip the output stage"
    );
    // review stage should route errors to error_recovery, like implement does.
    assert!(
        transitions
            .get("error_recovery")
            .map(|e| e.condition == crate::blueprint::TransitionCondition::Error)
            .unwrap_or(false)
    );
}

#[test]
fn coder_blueprint_passes_full_validation() {
    let manifest_content = include_str!("../../../leviath-cli/agents/coder/agent.leviath");
    let bp = parse_manifest(manifest_content).unwrap();
    bp.validate()
        .expect("the shipped coder blueprint must pass Blueprint::validate()");
}

#[test]
fn coder_plan_and_implement_can_ask_the_user_dynamically() {
    // Beyond the static plan_approval checkpoint, plan/implement should
    // be able to decide for themselves, mid-reasoning, that they need
    // human input - via the ask_user_* tools, not just the forced
    // interaction_points.
    let manifest_content = include_str!("../../../leviath-cli/agents/coder/agent.leviath");
    let bp = parse_manifest(manifest_content).unwrap();

    let plan = bp.find_stage("plan").unwrap();
    assert!(plan.available_tools.contains(&"ask_user_text".to_string()));
    assert!(
        plan.available_tools
            .contains(&"ask_user_choice".to_string())
    );

    let implement = bp.find_stage("implement").unwrap();
    assert!(
        implement
            .available_tools
            .contains(&"ask_user_text".to_string())
    );
    assert!(
        implement
            .available_tools
            .contains(&"ask_user_confirm".to_string())
    );
}

// ─── Production-code branch coverage: optional field None-paths ──────────

/// `interactive_points` mode with NO `interaction_points` array - the stage
/// still gets the mode, just with an empty points list.
#[test]
fn parse_manifest_interactive_points_mode_with_no_points_array() {
    let toml = r#"
[agent]
name = "no-points"

[stages.main]
mode = "interactive_points"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    let points = unwrap_interactive_points(&stage.mode);
    assert!(points.is_empty());
}

/// Per-stage tool_permissions with a non-string policy value - the inner
/// `if let Some(policy_str) = policy_val.as_str()` should be skipped.
#[test]
fn parse_manifest_stage_tool_permissions_non_string_value_is_rejected() {
    let toml = r#"
[agent]
name = "non-string-perm"

[stages.main]
mode = "autonomous"

[stages.main.tool_permissions]
bash = 123
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("bash"), "names the tool: {err}");
}

/// Compaction config without `provider` - covers the None-branch at line ~460.
#[test]
fn parse_manifest_compaction_without_provider_uses_default() {
    let toml = r#"
[agent]
name = "compact-no-provider"

[compaction]
model = "gpt-4o-mini"
"#;
    let bp = parse_manifest(toml).unwrap();
    let cc = bp.compaction_config.as_ref().unwrap();
    // provider absent - stays at CompactionConfig default
    assert_eq!(cc.model, "gpt-4o-mini");
}

/// Compaction config without `model` - covers the None-branch at line ~463.
#[test]
fn parse_manifest_compaction_without_model_uses_default() {
    let toml = r#"
[agent]
name = "compact-no-model"

[compaction]
provider = "anthropic"
"#;
    let bp = parse_manifest(toml).unwrap();
    let cc = bp.compaction_config.as_ref().unwrap();
    assert_eq!(cc.provider, "anthropic");
    // model absent - stays at CompactionConfig default
}

// ─── Security config parsing ──────────────────────────────────────────

#[test]
fn parse_manifest_with_security_config() {
    let toml = r#"
[agent]
name = "security-test"

[security]
taint_tracking = true
"#;
    let bp = parse_manifest(toml).unwrap();
    let sc = bp.security.as_ref().unwrap();
    assert!(sc.taint_tracking);
}

#[test]
fn parse_manifest_security_disabled() {
    let toml = r#"
[agent]
name = "no-taint"

[security]
taint_tracking = false
"#;
    let bp = parse_manifest(toml).unwrap();
    let sc = bp.security.as_ref().unwrap();
    assert!(!sc.taint_tracking);
}

#[test]
fn parse_manifest_no_security_section() {
    let toml = r#"
[agent]
name = "no-security"
"#;
    let bp = parse_manifest(toml).unwrap();
    assert!(bp.security.is_none());
}

/// Agent-level tool_permissions with a non-string value - the inner
/// `if let Some(policy_str) = policy_val.as_str()` should be skipped.
#[test]
fn parse_manifest_agent_tool_permissions_non_string_value_is_rejected() {
    // Skipped, until it wasn't worth it: a permission that does not survive
    // parsing is a permission nobody enforces, and the tool falls back to a
    // default that may be looser than what was written.
    let toml = r#"
[agent]
name = "agent-non-string-perm"

[tool_permissions]
bash = 42
read_file = "allow"
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("bash"), "names the tool: {err}");
}

// ─── Models list & allow_user_default tests ─────────────────────────────

#[test]
fn parse_manifest_models_list_priority_order() {
    let toml = r#"
[agent]
name = "priority-test"

[stages.main.model]
allow_user_default = true

[[stages.main.model.models]]
provider = "anthropic"
model = "claude-sonnet-4-6"

[[stages.main.model.models]]
provider = "openai"
model = "gpt-4o"

[[stages.main.model.models]]
provider = "ollama"
model = "llama3"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert_eq!(stage.model.models.len(), 3);
    // Order preserved
    assert_eq!(stage.model.models[0].provider, "anthropic");
    assert_eq!(stage.model.models[0].model, "claude-sonnet-4-6");
    assert_eq!(stage.model.models[1].provider, "openai");
    assert_eq!(stage.model.models[1].model, "gpt-4o");
    assert_eq!(stage.model.models[2].provider, "ollama");
    assert_eq!(stage.model.models[2].model, "llama3");
    assert!(stage.model.allow_user_default);
}

#[test]
fn parse_manifest_allow_user_default_false() {
    let toml = r#"
[agent]
name = "no-fallback-test"

[stages.main.model]
allow_user_default = false

[[stages.main.model.models]]
provider = "anthropic"
model = "claude-sonnet-4-6"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert!(!stage.model.allow_user_default);
    assert_eq!(stage.model.models.len(), 1);
}

#[test]
fn parse_manifest_allow_user_default_defaults_true() {
    let toml = r#"
[agent]
name = "default-aud-test"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-4-6"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert!(stage.model.allow_user_default);
}

#[test]
fn parse_manifest_backward_compat_single_model_inline() {
    // Old inline format: model = { provider = "...", model = "..." }
    let toml = r#"
[agent]
name = "compat-test"

[stages.main]
model = { provider = "google", model = "gemini-3.5-pro" }
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert_eq!(stage.model.models.len(), 1);
    assert_eq!(stage.model.models[0].provider, "google");
    assert_eq!(stage.model.models[0].model, "gemini-3.5-pro");
    assert!(stage.model.allow_user_default);
}

#[test]
fn parse_manifest_models_list_with_parameters() {
    let toml = r#"
[agent]
name = "params-models-test"

[stages.main.model]
allow_user_default = true

[stages.main.model.parameters]
temperature = 0.3
max_output_tokens = 16384

[[stages.main.model.models]]
provider = "anthropic"
model = "claude-sonnet-4-6"

[[stages.main.model.models]]
provider = "openai"
model = "gpt-4o"
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert_eq!(stage.model.models.len(), 2);
    assert_eq!(
        stage
            .model
            .parameters
            .get("temperature")
            .and_then(|v| v.as_f64()),
        Some(0.3)
    );
    assert_eq!(
        stage
            .model
            .parameters
            .get("max_output_tokens")
            .and_then(|v| v.as_u64()),
        Some(16384)
    );
}

#[test]
fn parse_manifest_max_output_tokens_override_via_parameters() {
    let toml = r#"
[agent]
name = "token-override-test"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-4-6"

[stages.main.model.parameters]
max_output_tokens = 2048
"#;
    let bp = parse_manifest(toml).unwrap();
    let stage = bp.find_stage("main").unwrap();
    assert_eq!(
        stage
            .model
            .parameters
            .get("max_output_tokens")
            .and_then(|v| v.as_u64()),
        Some(2048)
    );
}

#[test]
fn parse_manifest_per_stage_request_timeout() {
    let toml = r#"
[agent]
name = "timeout-test"

[stages.analyze.model]
provider = "anthropic"
model = "claude-sonnet-5"
request_timeout_secs = 900

[stages.test_fix.model]
provider = "anthropic"
model = "claude-sonnet-5"
"#;
    let bp = parse_manifest(toml).unwrap();
    // Set on the stage that declares it.
    assert_eq!(
        bp.find_stage("analyze").unwrap().model.request_timeout_secs,
        Some(900)
    );
    // Absent → None (default job timeout applies).
    assert_eq!(
        bp.find_stage("test_fix")
            .unwrap()
            .model
            .request_timeout_secs,
        None
    );
}

#[test]
fn parse_manifest_negative_request_timeout_is_refused() {
    let toml = r#"
[agent]
name = "neg-timeout-test"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"
request_timeout_secs = -5
"#;
    // A negative timeout fails the load rather than being dropped without a
    // word, which would run the stage on the default while the file said
    // otherwise.
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(
        err.contains("stage 'main': model: request_timeout_secs must not be negative (got -5)"),
        "{err}"
    );
}

#[test]
fn parse_manifest_with_hashmap_region() {
    let toml = r#"
[agent]
name = "test"

[context.regions]
files = { kind = "hashmap", max_tokens = 40000 }
files_limited = { kind = "hashmap", max_tokens = 20000, max_entries = 50 }
"#;
    let bp = parse_manifest(toml).unwrap();
    let files_region = bp.context_layout.get_region("files").unwrap();
    assert_eq!(files_region.kind, RegionKind::HashMap { max_entries: None });
    assert_eq!(files_region.max_tokens, 40000);

    let limited = bp.context_layout.get_region("files_limited").unwrap();
    assert_eq!(
        limited.kind,
        RegionKind::HashMap {
            max_entries: Some(50)
        }
    );
}

#[test]
fn parse_manifest_with_file_tracking() {
    let toml = r#"
[agent]
name = "test"

[context.regions]
files = { kind = "hashmap", max_tokens = 40000 }

[context.file_tracking]
region = "files"
track_reads = true
track_writes = true
max_file_tokens = 5000
"#;
    let bp = parse_manifest(toml).unwrap();
    let ft = bp.file_tracking.unwrap();
    assert_eq!(ft.region, "files");
    assert!(ft.track_reads);
    assert!(ft.track_writes);
    assert_eq!(ft.max_file_tokens, Some(5000));
}

#[test]
fn parse_manifest_file_tracking_defaults() {
    let toml = r#"
[agent]
name = "test"

[context.file_tracking]
region = "myfiles"
"#;
    let bp = parse_manifest(toml).unwrap();
    let ft = bp.file_tracking.unwrap();
    assert_eq!(ft.region, "myfiles");
    assert!(ft.track_reads);
    assert!(ft.track_writes);
    assert!(ft.max_file_tokens.is_none());
}

// ─── tool_routing ────────────────────────────────────────────────────────

#[test]
fn parse_stage_tool_routing_all_fields() {
    let toml = r#"
[agent]
name = "routing-test"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]
default_region = "my_results"
persist = true
max_result_tokens = 4096

[stages.main.tool_routing.overrides]
bash = "bash_output"
read_file = "file_contents"
"#;
    let bp = parse_manifest(toml).unwrap();
    let main = bp.find_stage("main").unwrap();
    let routing = main
        .tool_result_routing
        .as_ref()
        .expect("tool_result_routing should be Some");
    assert_eq!(routing.default_region, "my_results");
    assert!(routing.persist);
    assert_eq!(routing.max_result_tokens, Some(4096));
    assert_eq!(routing.tool_overrides.len(), 2);
    assert_eq!(routing.tool_overrides.get("bash").unwrap(), "bash_output");
    assert_eq!(
        routing.tool_overrides.get("read_file").unwrap(),
        "file_contents"
    );
}

#[test]
fn parse_stage_tool_routing_only_default_region() {
    let toml = r#"
[agent]
name = "routing-partial"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]
default_region = "custom_region"
"#;
    let bp = parse_manifest(toml).unwrap();
    let main = bp.find_stage("main").unwrap();
    let routing = main
        .tool_result_routing
        .as_ref()
        .expect("tool_result_routing should be Some");
    assert_eq!(routing.default_region, "custom_region");
    // defaults from ToolResultRouting::default()
    assert!(routing.persist);
    assert!(routing.max_result_tokens.is_none());
    assert!(routing.tool_overrides.is_empty());
}

#[test]
fn parse_stage_without_tool_routing() {
    let toml = r#"
[agent]
name = "no-routing"

[stages.main]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).unwrap();
    let main = bp.find_stage("main").unwrap();
    assert!(main.tool_result_routing.is_none());
}

#[test]
fn parse_stage_tool_routing_with_overrides_only() {
    let toml = r#"
[agent]
name = "overrides-only"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]

[stages.main.tool_routing.overrides]
search = "search_results"
write_file = "written_files"
compile = "build_output"
"#;
    let bp = parse_manifest(toml).unwrap();
    let main = bp.find_stage("main").unwrap();
    let routing = main
        .tool_result_routing
        .as_ref()
        .expect("tool_result_routing should be Some");
    // default_region keeps its default since we didn't set it
    assert_eq!(routing.default_region, "tool_results");
    assert_eq!(routing.tool_overrides.len(), 3);
    assert_eq!(
        routing.tool_overrides.get("search").unwrap(),
        "search_results"
    );
    assert_eq!(
        routing.tool_overrides.get("write_file").unwrap(),
        "written_files"
    );
    assert_eq!(
        routing.tool_overrides.get("compile").unwrap(),
        "build_output"
    );
}

#[test]
fn parse_stage_tool_routing_persist_false() {
    let toml = r#"
[agent]
name = "persist-false"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]
persist = false
"#;
    let bp = parse_manifest(toml).unwrap();
    let main = bp.find_stage("main").unwrap();
    let routing = main
        .tool_result_routing
        .as_ref()
        .expect("tool_result_routing should be Some");
    assert!(!routing.persist);
    // other fields keep defaults
    assert_eq!(routing.default_region, "tool_results");
    assert!(routing.max_result_tokens.is_none());
    assert!(routing.tool_overrides.is_empty());
}

#[test]
fn parse_stage_tool_routing_max_result_tokens() {
    let toml = r#"
[agent]
name = "max-tokens"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]
max_result_tokens = 8192
"#;
    let bp = parse_manifest(toml).unwrap();
    let main = bp.find_stage("main").unwrap();
    let routing = main
        .tool_result_routing
        .as_ref()
        .expect("tool_result_routing should be Some");
    assert_eq!(routing.max_result_tokens, Some(8192));
    // other fields keep defaults
    assert_eq!(routing.default_region, "tool_results");
    assert!(routing.persist);
    assert!(routing.tool_overrides.is_empty());
}

#[test]
fn parse_manifest_sliding_window_bulk_and_compact_strategies() {
    // Exercises the `bulk` and `compact` eviction-strategy arms of the
    // sliding_window region parser (with and without their optional counts).
    let toml = r#"
[agent]
name = "eviction-strategies"

[context.regions]
bulk_default = { kind = "sliding_window", max_items = 20, max_tokens = 3000, strategy = "bulk" }
bulk_overflow = { kind = "sliding_window", max_items = 20, max_tokens = 3000, strategy = "bulk", overflow = 5 }
compact_default = { kind = "sliding_window", max_items = 20, max_tokens = 3000, strategy = "compact" }
compact_count = { kind = "sliding_window", max_items = 20, max_tokens = 3000, strategy = "compact", compact_count = 7 }
"#;
    let bp = parse_manifest(toml).unwrap();
    let region = |name: &str| {
        bp.context_layout
            .regions
            .iter()
            .find(|r| r.name == name)
            .unwrap()
            .kind
            .clone()
    };

    assert_eq!(
        region("bulk_default"),
        RegionKind::SlidingWindow {
            max_items: 20,
            eviction_strategy: EvictionStrategy::Bulk { overflow: 10 },
        }
    );
    assert_eq!(
        region("bulk_overflow"),
        RegionKind::SlidingWindow {
            max_items: 20,
            eviction_strategy: EvictionStrategy::Bulk { overflow: 5 },
        }
    );
    assert_eq!(
        region("compact_default"),
        RegionKind::SlidingWindow {
            max_items: 20,
            eviction_strategy: EvictionStrategy::Compact { compact_count: 10 },
        }
    );
    assert_eq!(
        region("compact_count"),
        RegionKind::SlidingWindow {
            max_items: 20,
            eviction_strategy: EvictionStrategy::Compact { compact_count: 7 },
        }
    );
}

#[test]
fn parse_manifest_tool_routing_override_non_string_value_is_rejected() {
    // Skipping a non-string value would leave `write_file` with neither the
    // region named here nor any cap, and the manifest would still parse.
    // Silently discarding the line an author wrote is the failure, not the
    // recovery.
    let toml = r#"
[agent]
name = "routing-nonstring"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]
default_region = "temp"

[stages.main.tool_routing.overrides]
read_file = "files"
write_file = 123
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(
        err.contains("write_file"),
        "names the offending tool: {err}"
    );
}

#[test]
fn parse_manifest_security_agent_and_stage() {
    // Both an agent-level [security] and a stage-level override parse,
    // exercising both call sites of parse_security_config.
    let toml = r#"
[agent]
name = "sec-branches"

[security]
taint_tracking = false

[stages.a]
mode = "autonomous"

[stages.a.security]
taint_tracking = true
"#;
    let bp = parse_manifest(toml).unwrap();
    assert!(!bp.security.as_ref().unwrap().taint_tracking);
    let stage_a = bp.find_stage("a").unwrap();
    assert!(stage_a.security.as_ref().unwrap().taint_tracking);
}

#[test]
fn parse_manifest_sandbox_agent_and_stage() {
    // Agent-level [sandbox] plus a tighter per-stage override, exercising
    // every field and both call sites of parse_sandbox_config.
    let toml = r#"
[agent]
name = "sandboxed"

[sandbox]
kind = "container"
image = "ubuntu:24.04"

[stages.implement]
mode = "autonomous"

[stages.implement.sandbox]
kind = "container"
image = "node:22-slim"
engine = "podman"
network = false
mount = ["/data", "/cache"]
persist = true
on_unavailable = "warn"
"#;
    let bp = parse_manifest(toml).unwrap();
    let agent = bp.sandbox.as_ref().unwrap();
    assert_eq!(agent.kind, crate::SandboxKind::Container);
    assert_eq!(agent.image.as_deref(), Some("ubuntu:24.04"));
    assert!(agent.network); // default when omitted
    assert_eq!(agent.engine, None); // auto-detect when omitted

    let stage = bp.find_stage("implement").unwrap();
    let sb = stage.sandbox.as_ref().unwrap();
    assert_eq!(sb.image.as_deref(), Some("node:22-slim"));
    assert_eq!(sb.engine.as_deref(), Some("podman"));
    assert!(!sb.network);
    assert_eq!(sb.mounts, vec!["/data".to_string(), "/cache".to_string()]);
    assert!(sb.persist);
    assert_eq!(sb.on_unavailable, crate::OnUnavailable::Warn);
}

#[test]
fn parse_manifest_sandbox_kind_variants_and_no_kind() {
    // A `[sandbox]` block with no `kind` defaults to host (None); non-string
    // mount entries are filtered out.
    let bp = parse_manifest(
        "[agent]\nname = \"a\"\n\n\
         [sandbox]\nmount = [\"/ok\", 42]\n\n\
         [stages.s]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
    )
    .unwrap();
    let sb = bp.sandbox.unwrap();
    assert_eq!(sb.kind, crate::SandboxKind::None);
    assert_eq!(sb.mounts, vec!["/ok".to_string()]);

    // `namespace` and `none` kind strings both parse.
    let bp = parse_manifest(
        "[agent]\nname = \"a\"\n\n\
         [sandbox]\nkind = \"namespace\"\n\n\
         [stages.s]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
    )
    .unwrap();
    assert_eq!(bp.sandbox.unwrap().kind, crate::SandboxKind::Namespace);

    let bp = parse_manifest(
        "[agent]\nname = \"a\"\n\n\
         [sandbox]\nkind = \"none\"\n\n\
         [stages.s]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
    )
    .unwrap();
    assert_eq!(bp.sandbox.unwrap().kind, crate::SandboxKind::None);
}

#[test]
fn parse_manifest_sandbox_explicit_error_policy_and_stage_error() {
    // Explicit `on_unavailable = "error"` exercises that match arm.
    let bp = parse_manifest(
        "[agent]\nname = \"a\"\n\n\
         [sandbox]\nkind = \"container\"\nimage = \"x\"\non_unavailable = \"error\"\n\n\
         [stages.s]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
    )
    .unwrap();
    assert_eq!(
        bp.sandbox.unwrap().on_unavailable,
        crate::OnUnavailable::Error
    );

    // An invalid *per-stage* sandbox propagates its error through the stage
    // parse (the `?` on the stage-level `parse_sandbox_config`).
    let err = parse_manifest(
        "[agent]\nname = \"a\"\n\n\
         [stages.s]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\n\
         [stages.s.sandbox]\nkind = \"vm\"\n",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("unknown kind 'vm'"), "got: {err}");
}

#[test]
fn parse_manifest_sandbox_unknown_kind_errors() {
    let toml = r#"
[agent]
name = "bad"

[sandbox]
kind = "vm"
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("unknown kind 'vm'"), "got: {err}");
}
/// Per-tool result ceilings parse, spelled like the region overrides beside
/// them. Without this the table is accepted and ignored.
#[test]
fn parse_manifest_reads_per_tool_result_ceilings() {
    let toml = r#"
[agent]
name = "capped"

[stages.work]
mode = "autonomous"

[stages.work.tool_routing]
default_region = "results"
max_result_tokens = 4000

[stages.work.tool_routing.max_result_tokens_per_tool]
read_file = 20000
"#;
    let bp = parse_manifest(toml).expect("parses");
    let routing = bp
        .find_stage("work")
        .and_then(|s| s.tool_result_routing.as_ref())
        .expect("routing survives");
    assert_eq!(routing.max_result_tokens, Some(4000));
    assert_eq!(
        routing.tool_max_result_tokens.get("read_file").copied(),
        Some(20000)
    );
}

/// A non-integer ceiling fails the manifest.
///
/// Skipping the entry and keeping the rest of the table would match how the
/// region overrides beside it treat a non-string, and both readings are wrong
/// for the same reason: the author wrote a ceiling, no ceiling would be
/// applied, and nothing would say so.
#[test]
fn parse_manifest_rejects_a_non_integer_per_tool_ceiling() {
    let toml = r#"
[agent]
name = "capped"

[stages.work]
mode = "autonomous"

[stages.work.tool_routing]
default_region = "results"

[stages.work.tool_routing.max_result_tokens_per_tool]
read_file = "lots"
grep = 500
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("read_file"), "names the offending tool: {err}");
    assert!(err.contains("must be a number"), "got: {err}");
}

// ─── Values that quietly became a default ────────────────────────────────────
//
// Each of these would parse clean and resolve to something the author did not
// write. Same class as the unknown keys below: a line that does nothing and
// says nothing. The policy one is the sharp one - it resolves *looser* than
// what was asked for.

/// A misspelled `deny` must not resolve to `ask`, the more permissive of the
/// two: approvable by a session grant or `--yolo`. The author wrote a refusal
/// and would get a prompt.
#[test]
fn parse_manifest_rejects_a_misspelled_stage_tool_policy() {
    let toml = r#"
[agent]
name = "typo"

[stages.work]
mode = "autonomous"

[stages.work.tool_permissions]
shell = "denied"
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("\"denied\""), "quotes what was written: {err}");
    assert!(err.contains("valid: allow, ask, deny"), "got: {err}");
}

#[test]
fn parse_manifest_rejects_a_misspelled_agent_tool_policy() {
    let toml = r#"
[agent]
name = "typo"

[tool_permissions]
shell = "denny"
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("valid: allow, ask, deny"), "got: {err}");
}

/// Case is not the mistake - `Deny` has always resolved, and still must.
#[test]
fn parse_manifest_accepts_a_tool_policy_in_any_case() {
    let toml = r#"
[agent]
name = "cased"

[stages.work]
mode = "autonomous"

[stages.work.tool_permissions]
shell = "Deny"
write_file = "ALLOW"
"#;
    let bp = parse_manifest(toml).expect("case is not a typo");
    let stage = bp.find_stage("work").expect("stage");
    assert_eq!(
        stage.tool_permissions.get("shell").map(String::as_str),
        Some("Deny")
    );
}

/// A misspelled `fail_all` let a fan-out swallow every worker failure.
#[test]
fn parse_manifest_rejects_an_unknown_worker_failure_policy() {
    let toml = r#"
[agent]
name = "fan"

[stages.split]
mode = "fan_out"
worker_stage = "work"
on_worker_failure = "failall"

[stages.work]
mode = "autonomous"
allow_as_worker = true
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("valid: continue, fail_all"), "got: {err}");
}

#[test]
fn parse_manifest_accepts_both_worker_failure_policies() {
    for policy in ["continue", "fail_all"] {
        let toml = format!(
            r#"
[agent]
name = "fan"

[stages.split]
mode = "fan_out"
worker_stage = "work"
on_worker_failure = "{policy}"

[stages.work]
mode = "autonomous"
allow_as_worker = true
"#
        );
        parse_manifest(&toml).unwrap_or_else(|e| panic!("{policy} is valid: {e}"));
    }
}

/// Its neighbour `unattended` has always rejected an unknown value; `style`
/// turned a mistyped `confirm` into a free-text question with the options
/// listed and nothing enforcing them.
#[test]
fn parse_manifest_rejects_an_unknown_interaction_style() {
    let toml = r#"
[agent]
name = "ask"

[stages.work]
mode = "interactive_points"

[[stages.work.interaction_points]]
name = "approve"
prompt = "ok?"
style = "confirmation"
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(
        err.contains("valid: free_text, multiple_choice, confirm"),
        "got: {err}"
    );
}

/// `strategy = "per-item"` is the hyphen mistake this invites, and it left the
/// region evicting one entry at a time with no sign the line was read.
#[test]
fn parse_manifest_rejects_an_unknown_eviction_strategy() {
    let toml = r#"
[agent]
name = "evict"

[context.regions]
notes = { kind = "sliding_window", max_items = 20, strategy = "per-item" }
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("valid: per_item, bulk, compact"), "got: {err}");
    assert!(err.contains("notes"), "names the region: {err}");
}

#[test]
fn parse_manifest_accepts_every_eviction_strategy() {
    for strategy in ["per_item", "bulk", "compact"] {
        let toml = format!(
            r#"
[agent]
name = "evict"

[context.regions]
notes = {{ kind = "sliding_window", max_items = 20, strategy = "{strategy}" }}
"#
        );
        parse_manifest(&toml).unwrap_or_else(|e| panic!("{strategy} is valid: {e}"));
    }
}

// ─── Keys that do not exist ──────────────────────────────────────────────────
//
// Every one of these would parse clean, which is what makes a typo in the
// feature indistinguishable from a working config.

/// The smallest manifest with a `bulk` region and one stage, for the key tests.
fn keys_fixture(stage_extra: &str) -> String {
    format!(
        r#"
[agent]
name = "keys"
entry_stage = "work"

[context.regions]
bulk = {{ kind = "pinned", max_tokens = 1000 }}

[stages.work]
mode = "autonomous"
system_prompt = "go"
{stage_extra}
"#
    )
}

#[test]
fn parse_manifest_rejects_an_unknown_stage_key() {
    let err = parse_manifest(&keys_fixture("totally_made_up_key = 42"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("unknown key 'totally_made_up_key'"),
        "got: {err}"
    );
    // The valid list is the message, same as region kinds and conditions.
    assert!(err.contains("system_prompt"), "lists what is valid: {err}");
}

/// A stage narrows its window by listing what it wants, so there is no
/// `hidden` key - and guessing one leaves the stage carrying the region it
/// meant to drop, at full cost, silently.
#[test]
fn parse_manifest_rejects_an_unknown_context_key() {
    let err = parse_manifest(&keys_fixture("[stages.work.context]\nhidden = [\"bulk\"]"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown key 'hidden'"), "got: {err}");
    assert!(err.contains("valid: regions"), "got: {err}");
}

#[test]
fn parse_manifest_rejects_an_unknown_tool_routing_key() {
    let err = parse_manifest(&keys_fixture(
        "[stages.work.tool_routing]\nmax_tokens = 100",
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("unknown key 'max_tokens'"), "got: {err}");
}

#[test]
fn parse_manifest_rejects_an_unknown_gate_key() {
    let err = parse_manifest(&keys_fixture(
        "[stages.work.transitions.work]\ncondition = \"always\"\ngate = { require_writes = \"bulk\" }",
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("unknown key 'require_writes'"), "got: {err}");
}

#[test]
fn parse_manifest_rejects_an_unknown_transition_key() {
    let err = parse_manifest(&keys_fixture(
        "[stages.work.transitions.work]\nconditon = \"always\"",
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("unknown key 'conditon'"), "got: {err}");
}

/// A stage `description` is now read. Every bundled agent writes one and the
/// parser never looked at the key, so the field was permanently `None`.
#[test]
fn parse_manifest_reads_a_stage_description() {
    let bp = parse_manifest(&keys_fixture("description = \"map the codebase\"")).expect("parses");
    assert_eq!(
        bp.find_stage("work").and_then(|s| s.description.as_deref()),
        Some("map the codebase")
    );
}

// ─── Region names that match nothing ─────────────────────────────────────────

#[test]
fn validate_rejects_a_routing_override_naming_no_region() {
    let bp = parse_manifest(&keys_fixture(
        "[stages.work.tool_routing.overrides]\nread_file = \"ghost\"",
    ))
    .expect("parses");
    let err = bp.validate().unwrap_err().to_string();
    assert!(err.contains("'ghost'"), "names the region: {err}");
}

#[test]
fn validate_rejects_a_gate_naming_no_region() {
    let bp = parse_manifest(&keys_fixture(
        "[stages.work.transitions.done]\ncondition = \"always\"\ngate = { require_region_updated = \"nope\" }\n\n[stages.done]\nmode = \"autonomous\"\nsystem_prompt = \"end\"",
    ))
    .expect("parses");
    let err = bp.validate().unwrap_err().to_string();
    assert!(err.contains("'nope'"), "names the region: {err}");
}

/// A checklist gate counts open items, which only a checklist region has.
/// Pointed at a text region it reads zero every time, so it passes on the
/// first attempt - a gate that looks armed and holds nothing.
#[test]
fn validate_rejects_a_checklist_gate_on_a_non_checklist_region() {
    let bp = parse_manifest(&keys_fixture(
        "[stages.work.transitions.done]\ncondition = \"always\"\ngate = { require_no_open_items = \"bulk\" }\n\n[stages.done]\nmode = \"autonomous\"\nsystem_prompt = \"end\"",
    ))
    .expect("parses");
    let err = bp.validate().unwrap_err().to_string();
    assert!(err.contains("not a checklist region"), "got: {err}");
}

/// A stage may name a region another stage declares: omitting a region hides
/// it rather than destroying it, so this is legitimate and must not be
/// mistaken for a typo.
#[test]
fn validate_allows_a_gate_on_a_region_declared_by_another_stage() {
    // A gate is evaluated by the runtime against the region's contents, not by
    // the model reading it, so a region this stage does not render is still a
    // sound thing to gate on. Routing is the opposite case and is checked
    // per-stage; see the routing tests.
    let toml = r#"
[agent]
name = "keys"
entry_stage = "work"

[stages.work]
mode = "autonomous"
system_prompt = "go"

[stages.work.transitions.other]
condition = "always"
gate = { require_region_updated = "notes" }

[stages.other]
mode = "autonomous"
system_prompt = "go"

[stages.other.context.regions]
notes = { kind = "pinned", max_tokens = 1000 }
"#;
    let bp = parse_manifest(toml).expect("parses");
    bp.validate()
        .expect("a cross-stage region reference is fine for a gate");
}

#[test]
fn validate_rejects_a_default_region_naming_no_region() {
    let bp = parse_manifest(&keys_fixture(
        "[stages.work.tool_routing]\ndefault_region = \"ghost\"",
    ))
    .expect("parses");
    let err = bp.validate().unwrap_err().to_string();
    assert!(err.contains("'ghost'"), "names the region: {err}");
}

/// A checklist gate pointed at a real checklist region validates, which is
/// what makes the rejection beside it mean something.
#[test]
fn validate_allows_a_checklist_gate_on_a_checklist_region() {
    let toml = r#"
[agent]
name = "keys"
entry_stage = "work"

[context.regions]
todos = { kind = "checklist", max_tokens = 2000 }

[stages.work]
mode = "autonomous"
system_prompt = "go"

[stages.work.transitions.done]
condition = "always"
gate = { require_no_open_items = "todos" }

[stages.done]
mode = "autonomous"
system_prompt = "end"
"#;
    let bp = parse_manifest(toml).expect("parses");
    bp.validate()
        .expect("a checklist gate on a checklist region is fine");
}

/// The three the runtime adds when nobody declares them stay addressable.
#[test]
fn validate_allows_the_auto_added_regions() {
    let bp = parse_manifest(&keys_fixture(
        "[stages.work.tool_routing]\ndefault_region = \"tool_results\"",
    ))
    .expect("parses");
    bp.validate().expect("tool_results always exists");
}

/// The `{ region, max_result_tokens }` shape routes *and* caps.
///
/// Without it the entry falls through the string-only match arm entirely: it
/// parses clean, does nothing, and the tool loses its region as well as its
/// cap.
#[test]
fn parse_manifest_reads_a_tool_override_table() {
    let toml = r#"
[agent]
name = "capped"

[stages.work]
mode = "autonomous"

[stages.work.tool_routing]
default_region = "conversation"

[stages.work.tool_routing.overrides]
read_file = { region = "bulk", max_result_tokens = 500 }
grep = "conversation"
"#;
    let bp = parse_manifest(toml).expect("parses");
    let routing = bp
        .find_stage("work")
        .and_then(|s| s.tool_result_routing.as_ref())
        .expect("routing survives");
    assert_eq!(
        routing.tool_overrides.get("read_file").map(String::as_str),
        Some("bulk"),
        "the table form must still route"
    );
    assert_eq!(
        routing.tool_max_result_tokens.get("read_file").copied(),
        Some(500),
        "and must carry the cap"
    );
    // The bare-string form beside it is untouched.
    assert_eq!(
        routing.tool_overrides.get("grep").map(String::as_str),
        Some("conversation")
    );
}

/// Either half of the table on its own is meaningful.
#[test]
fn parse_manifest_reads_a_half_filled_tool_override_table() {
    let toml = r#"
[agent]
name = "capped"

[stages.work]
mode = "autonomous"

[stages.work.tool_routing]
default_region = "conversation"

[stages.work.tool_routing.overrides]
read_file = { max_result_tokens = 500 }
grep = { region = "bulk" }
"#;
    let bp = parse_manifest(toml).expect("parses");
    let routing = bp
        .find_stage("work")
        .and_then(|s| s.tool_result_routing.as_ref())
        .expect("routing survives");
    // Capped, but left in the stage's default region.
    assert_eq!(
        routing.tool_max_result_tokens.get("read_file").copied(),
        Some(500)
    );
    assert!(!routing.tool_overrides.contains_key("read_file"));
    // Routed, but uncapped.
    assert_eq!(
        routing.tool_overrides.get("grep").map(String::as_str),
        Some("bulk")
    );
    assert!(!routing.tool_max_result_tokens.contains_key("grep"));
}

/// A misspelled key inside the table is refused, naming what is valid.
#[test]
fn parse_manifest_rejects_an_unknown_tool_override_key() {
    let toml = r#"
[agent]
name = "capped"

[stages.work]
mode = "autonomous"

[stages.work.tool_routing.overrides]
read_file = { region = "bulk", max_tokens = 500 }
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("unknown key 'max_tokens'"), "got: {err}");
    assert!(
        err.contains("valid: region, max_result_tokens"),
        "names what is valid: {err}"
    );
}

/// A value that is neither a region name nor a table is refused rather than
/// skipped.
#[test]
fn parse_manifest_rejects_a_tool_override_of_the_wrong_type() {
    let toml = r#"
[agent]
name = "capped"

[stages.work]
mode = "autonomous"

[stages.work.tool_routing.overrides]
read_file = 500
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(
        err.contains("tool_routing.overrides.read_file"),
        "got: {err}"
    );
    assert!(err.contains("integer"), "names the type it got: {err}");
}

/// A negative ceiling is refused rather than wrapping.
///
/// `as usize` on a negative turns 500 tokens into 18 exabytes, which reads at a
/// glance like "no limit" - the opposite of what was written.
#[test]
fn parse_manifest_rejects_a_negative_tool_override_ceiling() {
    let toml = r#"
[agent]
name = "capped"

[stages.work]
mode = "autonomous"

[stages.work.tool_routing.overrides]
read_file = { max_result_tokens = -1 }
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("must not be negative"), "got: {err}");
}

/// A `region` that is not a name.
#[test]
fn parse_manifest_rejects_a_non_string_region_in_a_tool_override() {
    let toml = r#"
[agent]
name = "capped"

[stages.work]
mode = "autonomous"

[stages.work.tool_routing.overrides]
read_file = { region = 5 }
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("must be a region name"), "got: {err}");
}

/// A cap inside the table that is not a number.
#[test]
fn parse_manifest_rejects_a_non_integer_cap_in_a_tool_override() {
    let toml = r#"
[agent]
name = "capped"

[stages.work]
mode = "autonomous"

[stages.work.tool_routing.overrides]
read_file = { max_result_tokens = "lots" }
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("must be a number"), "got: {err}");
}

/// The same negative guard on the sibling table.
#[test]
fn parse_manifest_rejects_a_negative_per_tool_ceiling() {
    let toml = r#"
[agent]
name = "capped"

[stages.work]
mode = "autonomous"

[stages.work.tool_routing.max_result_tokens_per_tool]
read_file = -1
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("must not be negative"), "got: {err}");
}

/// A stage that is not a table at all has no keys to check, and still parses
/// into a default stage as it always did.
#[test]
fn parse_manifest_tolerates_a_stage_that_is_not_a_table() {
    let toml = r#"
[agent]
name = "odd"

[stages]
work = 5
"#;
    let bp = parse_manifest(toml).expect("parses");
    assert!(bp.find_stage("work").is_some());
}

/// `[stages.X.context]` with no `regions` leaves the stage on the global
/// layout rather than an empty one.
#[test]
fn parse_manifest_tolerates_a_context_table_without_regions() {
    let toml = r#"
[agent]
name = "ctx"

[stages.work]
mode = "autonomous"

[stages.work.context]
"#;
    let bp = parse_manifest(toml).expect("parses");
    assert!(
        bp.find_stage("work")
            .expect("stage")
            .context_layout
            .is_none()
    );
}

/// A string edge (`b = "true"`) carries no keys to check.
#[test]
fn parse_manifest_tolerates_a_string_transition_edge() {
    let toml = r#"
[agent]
name = "edge"

[stages.work]
mode = "autonomous"

[stages.work.transitions]
done = "true"

[stages.done]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).expect("parses");
    assert!(
        bp.find_stage("work")
            .and_then(|s| s.transitions.as_ref())
            .is_some_and(|t| t.contains_key("done"))
    );
}

/// An empty table says nothing, so it is a mistake rather than a default.
#[test]
fn parse_manifest_rejects_an_empty_tool_override_table() {
    let toml = r#"
[agent]
name = "capped"

[stages.work]
mode = "autonomous"

[stages.work.tool_routing.overrides]
read_file = {}
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(err.contains("is empty"), "got: {err}");
}

/// `require_region_updated` parses onto the edge gate. Without this the key is
/// accepted and ignored, which is how a gate silently blocks nothing.
#[test]
fn parse_manifest_reads_a_region_update_gate() {
    let toml = r#"
[agent]
name = "revising"

[stages.plan]
mode = "autonomous"

[stages.plan.transitions.compute]
gate = { require_region_updated = "plan", message = "Change it first." }

[stages.compute]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).expect("parses");
    let gate = bp
        .find_stage("plan")
        .and_then(|s| s.transitions.as_ref())
        .and_then(|t| t.get("compute"))
        .and_then(|e| e.gate.as_ref())
        .expect("the gate survives");
    assert_eq!(gate.require_region_updated.as_deref(), Some("plan"));
    assert_eq!(gate.message.as_deref(), Some("Change it first."));
    // And it does not silently turn on the other gate.
    assert!(!gate.require_modifications);
}

/// `require_regions` parses onto the edge gate as a list.
///
/// The key exists because `region` reads as though it says this and does not:
/// it is one of several *alternative* ways to satisfy `require_modifications`,
/// so a stage that wrote any file at all satisfies it with the named region
/// still empty.
#[test]
fn parse_manifest_reads_a_required_regions_gate() {
    let toml = r#"
[agent]
name = "planning"

[context.regions]
plan = { kind = "pinned", max_tokens = 2000 }
risks = { kind = "pinned", max_tokens = 2000 }

[stages.plan]
mode = "autonomous"

[stages.plan.transitions.compute]
gate = { require_regions = ["plan", "risks"] }

[stages.compute]
mode = "autonomous"
"#;
    let bp = parse_manifest(toml).expect("parses");
    let gate = bp
        .find_stage("plan")
        .and_then(|s| s.transitions.as_ref())
        .and_then(|t| t.get("compute"))
        .and_then(|e| e.gate.as_ref())
        .expect("the gate survives");
    assert_eq!(gate.require_regions, vec!["plan", "risks"]);
    // And it does not silently turn on the condition it exists to replace.
    assert!(!gate.require_modifications);
}

/// A checklist region and its gate both parse. Without the parse the key is
/// accepted and ignored, which is a gate that silently blocks nothing.
#[test]
fn parse_manifest_reads_a_checklist_and_its_gate() {
    let toml = r#"
[agent]
name = "tracking"

[stages.implement]
mode = "autonomous"

[stages.implement.transitions.review]
gate = { require_no_open_items = "todos" }

[stages.review]
mode = "autonomous"

[context.regions]
todos = { kind = "checklist", max_tokens = 2000 }
"#;
    let bp = parse_manifest(toml).expect("parses");
    assert_eq!(
        bp.context_layout
            .get_region("todos")
            .map(|r| r.kind.clone()),
        Some(crate::RegionKind::Checklist)
    );
    let gate = bp
        .find_stage("implement")
        .and_then(|s| s.transitions.as_ref())
        .and_then(|t| t.get("review"))
        .and_then(|e| e.gate.as_ref())
        .expect("the gate survives");
    assert_eq!(gate.require_no_open_items.as_deref(), Some("todos"));
}

#[test]
fn parse_manifest_sandbox_unknown_on_unavailable_errors() {
    let toml = r#"
[agent]
name = "bad"

[sandbox]
kind = "container"
on_unavailable = "explode"
"#;
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(
        err.contains("unknown on_unavailable 'explode'"),
        "got: {err}"
    );
}

// ─── Routing into a region the stage cannot see ──────────────────────────────

/// The reported shape: a stage scopes its context and routes a tool into a
/// region it left out. The result is written where the stage cannot read it,
/// and the pointer left in `conversation` tells the model to go read it.
#[test]
fn validate_rejects_routing_into_a_region_the_stage_omits() {
    let toml = r#"
[agent]
name = "scoped"
entry_stage = "verify"

[context.regions]
data_preview = { kind = "pinned", max_tokens = 8000 }
plan = { kind = "pinned", max_tokens = 2000 }

[stages.verify]
mode = "autonomous"
system_prompt = "check the rules against the manual"

[stages.verify.context.regions]
plan = { kind = "pinned", max_tokens = 2000 }

[stages.verify.tool_routing.overrides]
read_file = "data_preview"
"#;
    let bp = parse_manifest(toml).expect("parses");
    let err = bp.validate().unwrap_err().to_string();
    assert!(err.contains("data_preview"), "names the region: {err}");
    assert!(
        err.contains("could not read them back"),
        "says what is wrong: {err}"
    );
    assert!(
        err.contains("[stages.verify.context.regions]"),
        "says how to fix it: {err}"
    );
}

/// `scratch` in the report: declared globally, named as a routing target, and
/// in no stage's layout at all - unreachable by construction for the whole life
/// of the blueprint.
#[test]
fn validate_rejects_a_default_region_no_stage_renders() {
    let toml = r#"
[agent]
name = "scoped"
entry_stage = "plan"

[context.regions]
scratch = { kind = "temporary", max_tokens = 4000 }
notes = { kind = "pinned", max_tokens = 2000 }

[stages.plan]
mode = "autonomous"
system_prompt = "go"

[stages.plan.context.regions]
notes = { kind = "pinned", max_tokens = 2000 }

[stages.plan.tool_routing]
default_region = "scratch"
"#;
    let bp = parse_manifest(toml).expect("parses");
    let err = bp.validate().unwrap_err().to_string();
    assert!(err.contains("scratch"), "{err}");
}

/// A stage that declares no layout of its own sees the blueprint's, so routing
/// into any global region is fine. Without this the check would reject the
/// ordinary un-scoped blueprint, which is most of them.
#[test]
fn validate_allows_routing_into_a_global_region_from_an_unscoped_stage() {
    let toml = r#"
[agent]
name = "plain"
entry_stage = "work"

[context.regions]
codebase = { kind = "compacting", max_tokens = 8000 }

[stages.work]
mode = "autonomous"
system_prompt = "go"

[stages.work.tool_routing.overrides]
read_file = "codebase"
"#;
    let bp = parse_manifest(toml).expect("parses");
    bp.validate()
        .expect("an unscoped stage sees the global layout");
}

/// The regions the runtime carries visible whatever a stage declares are always
/// legitimate routing targets, or a scoped stage could not route anywhere.
#[test]
fn validate_allows_routing_into_the_always_visible_regions() {
    for target in ["conversation", "tool_results", "final_output"] {
        let toml = format!(
            r#"
[agent]
name = "scoped"
entry_stage = "work"

[context.regions]
notes = {{ kind = "pinned", max_tokens = 2000 }}

[stages.work]
mode = "autonomous"
system_prompt = "go"

[stages.work.context.regions]
notes = {{ kind = "pinned", max_tokens = 2000 }}

[stages.work.tool_routing]
default_region = "{target}"
"#
        );
        let bp = parse_manifest(&toml).expect("parses");
        bp.validate()
            .unwrap_or_else(|e| panic!("{target} is always visible: {e}"));
    }
}

/// A stage that scopes its context and routes into a region it *did* declare is
/// the case this must not touch.
#[test]
fn validate_allows_routing_into_a_region_the_stage_declares() {
    let toml = r#"
[agent]
name = "scoped"
entry_stage = "work"

[context.regions]
codebase = { kind = "compacting", max_tokens = 8000 }
notes = { kind = "pinned", max_tokens = 2000 }

[stages.work]
mode = "autonomous"
system_prompt = "go"

[stages.work.context.regions]
codebase = { kind = "compacting", max_tokens = 8000 }

[stages.work.tool_routing.overrides]
read_file = "codebase"
"#;
    let bp = parse_manifest(toml).expect("parses");
    bp.validate()
        .expect("routing into a declared region is fine");
}

// ─── Regions an edge transform must not paraphrase ───────────────────────────

/// `summarizable = false` parses, and the default is on.
///
/// A region is summarizable unless its author says the content does not survive
/// a paraphrase, so the flag has to be readable and its absence has to mean
/// "yes" - a default of false would quietly stop `compact` doing the thing it
/// is for.
#[test]
fn parse_manifest_reads_the_summarizable_flag() {
    let toml = r#"
[agent]
name = "figures"

[context.regions]
results = { kind = "sliding_window", max_items = 20, summarizable = false }
notes = { kind = "sliding_window", max_items = 20 }
"#;
    let bp = parse_manifest(toml).expect("parses");
    let region = |name: &str| {
        bp.context_layout
            .regions
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name} declared"))
    };
    assert!(!region("results").summarizable, "the flag is read");
    assert!(region("notes").summarizable, "and defaults to on");
}

/// A region's `description` parses, and its absence stays absent.
///
/// Documentation on its own: it reaches `lev dash` and the blueprint API, and
/// only reaches the model when the region also sets `describe_in_prompt`.
#[test]
fn parse_manifest_reads_a_region_description() {
    let toml = r#"
[agent]
name = "curator"

[context.regions]
sources_index = { kind = "pinned", max_tokens = 100, description = "One bibliography line per source actually used." }
task = { kind = "pinned", max_tokens = 100 }
blank = { kind = "pinned", max_tokens = 100, description = "   " }
"#;
    let bp = parse_manifest(toml).expect("parses");
    let region = |name: &str| {
        bp.context_layout
            .regions
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name} declared"))
    };
    assert_eq!(
        region("sources_index").description.as_deref(),
        Some("One bibliography line per source actually used.")
    );
    assert_eq!(region("task").description, None);
    assert_eq!(
        region("blank").description,
        None,
        "whitespace is not a description, and would render as a blank line"
    );
}

/// `describe_in_prompt` decides whether the description is spent on the model.
///
/// Off by default, so describing every region for the people who maintain a
/// blueprint cannot quietly add a sentence per region to every turn. A
/// non-boolean value falls back to the default rather than failing the load:
/// the cost of guessing wrong is a sentence, and refusing to load an agent over
/// it would be the larger harm.
#[test]
fn parse_manifest_reads_the_describe_in_prompt_flag() {
    let toml = r#"
[agent]
name = "curator"

[context.regions]
shown = { kind = "pinned", max_tokens = 100, description = "Format: one line per source.", describe_in_prompt = true }
quiet = { kind = "pinned", max_tokens = 100, description = "For whoever edits this." }
odd = { kind = "pinned", max_tokens = 100, describe_in_prompt = "yes" }
"#;
    let bp = parse_manifest(toml).expect("parses");
    let region = |name: &str| {
        bp.context_layout
            .regions
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name} declared"))
    };
    assert!(region("shown").describe_in_prompt, "the flag is read");
    assert!(
        !region("quiet").describe_in_prompt,
        "a description alone does not reach the model"
    );
    assert!(
        !region("odd").describe_in_prompt,
        "a non-boolean falls back to the default rather than failing the load"
    );
}

/// `admission` parses, defaults to evicting, and refuses a value it does not
/// know rather than falling back.
///
/// Falling back would be the dangerous reading: an author who typed
/// `admission = "rejcet"` wants their curated region protected, and silently
/// giving them the evicting default would drop the entries they were trying to
/// keep, with nothing said.
#[test]
fn parse_manifest_reads_the_admission_setting() {
    let toml = r#"
[agent]
name = "curator"

[context.regions]
sources = { kind = "temporary", max_tokens = 100, admission = "reject" }
scratch = { kind = "temporary", max_tokens = 100, admission = "evict" }
notes = { kind = "temporary", max_tokens = 100 }
"#;
    let bp = parse_manifest(toml).expect("parses");
    let region = |name: &str| {
        bp.context_layout
            .regions
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name} declared"))
    };
    assert_eq!(
        region("sources").admission,
        crate::region::Admission::Reject,
        "the setting is read"
    );
    assert_eq!(
        region("scratch").admission,
        crate::region::Admission::Evict,
        "and its other value"
    );
    assert_eq!(
        region("notes").admission,
        crate::region::Admission::Evict,
        "and absence means the behaviour every region had before"
    );

    let bad = r#"
[agent]
name = "curator"

[context.regions]
sources = { kind = "temporary", max_tokens = 100, admission = "rejcet" }
"#;
    let err = parse_manifest(bad).expect_err("an unknown value is refused");
    let message = err.to_string();
    assert!(message.contains("sources"), "{message}");
    assert!(
        message.contains("rejcet"),
        "names what was written: {message}"
    );
    assert!(message.contains("reject"), "and what was meant: {message}");
}

/// `available_connectors` parses, and its absence is an empty list rather than
/// anything implied - a stage that names no server grants no server.
#[test]
fn parse_manifest_reads_available_connectors() {
    let toml = r#"
[agent]
name = "triage"

[stages.look]
available_tools = ["read_file"]
available_connectors = ["github", "database"]

[stages.plain]
available_tools = ["read_file"]
"#;
    let bp = parse_manifest(toml).expect("parses");
    let stage = |name: &str| {
        bp.stages
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("{name} declared"))
    };
    assert_eq!(
        stage("look").available_connectors,
        vec!["github".to_string(), "database".to_string()],
        "read in the order written, since that is the order their tools are granted in"
    );
    assert_eq!(
        stage("look").available_tools,
        vec!["read_file".to_string()],
        "and the exact-match list is left exactly as it was"
    );
    assert!(
        stage("plain").available_connectors.is_empty(),
        "a stage that names no connector grants none"
    );
}

/// A region definition written before this field existed deserializes as
/// summarizable, so an archived layout does not come back protected by
/// accident - or, worse, unprotected when it was not.
#[test]
fn a_region_definition_without_the_field_deserializes_as_summarizable() {
    let json = serde_json::json!({
        "name": "notes",
        "kind": "Temporary",
        "max_tokens": 1000,
    });
    let def: crate::layout::RegionDefinition =
        serde_json::from_value(json).expect("an older definition still loads");
    assert!(def.summarizable);
}

/// `volatility` parses, and an unclassified region gets the pessimistic value.
///
/// Pessimistic because a provider caches by prefix: a block that moves
/// invalidates everything behind it, so a region nobody has classified must be
/// assumed to move and sorted late. An optimistic default is how inferring
/// stability from the region's kind went wrong.
#[test]
fn parse_manifest_reads_region_volatility() {
    let toml = r#"
[agent]
name = "curator"

[context.regions]
task = { kind = "pinned", max_tokens = 100, volatility = "stable" }
notes = { kind = "pinned", max_tokens = 100, volatility = "grows" }
scratch = { kind = "pinned", max_tokens = 100, volatility = "rewritten" }
unsaid = { kind = "pinned", max_tokens = 100 }
"#;
    let bp = parse_manifest(toml).expect("parses");
    let region = |name: &str| {
        bp.context_layout
            .regions
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name} declared"))
    };
    assert_eq!(region("task").volatility, crate::region::Volatility::Stable);
    assert_eq!(region("notes").volatility, crate::region::Volatility::Grows);
    assert_eq!(
        region("scratch").volatility,
        crate::region::Volatility::Rewritten
    );
    assert_eq!(
        region("unsaid").volatility,
        crate::region::Volatility::Rewritten
    );
}

/// A misspelling is refused rather than quietly taking the default.
///
/// The default is the *worst* placement, so falling back to it would cost the
/// author exactly the caching they were asking for, silently.
#[test]
fn parse_manifest_refuses_an_unknown_volatility() {
    let toml = r#"
[agent]
name = "curator"

[context.regions]
task = { kind = "pinned", max_tokens = 100, volatility = "immutable" }
"#;
    let err = parse_manifest(toml).expect_err("an unknown value is refused");
    let message = err.to_string();
    assert!(message.contains("immutable"), "{message}");
    assert!(message.contains("stable"), "{message}");
}

/// A `max_output_tokens` the loader cannot read fails the load, naming the
/// stage: passed through as written it would reach the provider as nonsense
/// or as no cap at all, and a typo in a limit only shows up as a bill.
#[test]
fn a_bad_output_cap_fails_the_load_and_a_relative_one_is_kept() {
    let bad = r#"
[agent]
name = "t"

[stages.write]
mode = "autonomous"
model = { models = ["claude-sonnet-5"], parameters = { max_output_tokens = "lots" } }
"#;
    let err = parse_manifest(bad).expect_err("rejected");
    assert!(err.to_string().contains("stage 'write'"), "{err}");
    assert!(err.to_string().contains("max_output_tokens"), "{err}");

    let good = r#"
[agent]
name = "t"

[stages.write]
mode = "autonomous"
model = { models = ["claude-sonnet-5"], parameters = { max_output_tokens = "100% of report" } }
"#;
    let manifest = parse_manifest(good).expect("loads");
    assert_eq!(
        manifest.stages[0].model.output_cap(),
        Ok(Some(crate::blueprint::OutputCap::RegionPercent {
            percent: 1.0,
            region: "report".to_string()
        }))
    );
}

/// `[stages.<name>.context] hide = [...]` reads as a list of region names,
/// is checked against the blueprint's layouts, and cannot name one of the
/// regions every stage carries.
#[test]
fn a_stage_can_hide_a_region_it_does_not_read() {
    let good = r#"
[agent]
name = "t"

[context.regions]
sources = { kind = "temporary", budget = "30%" }
claims = { kind = "pinned", budget = "10%" }

[stages.gather]
mode = "autonomous"
model = { models = ["claude-sonnet-5"] }

[stages.polish]
mode = "autonomous"
model = { models = ["claude-sonnet-5"] }

[stages.polish.context]
hide = ["sources"]
"#;
    let manifest = parse_manifest(good).expect("loads");
    assert_eq!(manifest.stages[1].context_hide, vec!["sources".to_string()]);
    assert!(manifest.stages[0].context_hide.is_empty());
    manifest
        .validate()
        .expect("a hidden region the layout declares is fine");

    let bad_shape = good.replace(r#"hide = ["sources"]"#, r#"hide = "sources""#);
    let err = parse_manifest(&bad_shape).expect_err("not a list");
    assert!(
        err.to_string().contains("context.hide must be a list"),
        "{err}"
    );

    let unknown = good.replace(r#"hide = ["sources"]"#, r#"hide = ["sauces"]"#);
    let err = parse_manifest(&unknown)
        .expect("shape is fine")
        .validate()
        .expect_err("no such region");
    assert!(err.to_string().contains("'sauces'"), "{err}");

    let always = good.replace(r#"hide = ["sources"]"#, r#"hide = ["conversation"]"#);
    let err = parse_manifest(&always)
        .expect("shape is fine")
        .validate()
        .expect_err("cannot hide the conversation");
    assert!(err.to_string().contains("cannot hide"), "{err}");

    // A tool result routed to a region the stage hid is a result the stage
    // cannot read, and is refused the way routing to an undeclared region is.
    let routed = good.replace(
        r#"hide = ["sources"]"#,
        "hide = [\"sources\"]\n\n[stages.polish.tool_routing]\ndefault_region = \"sources\"",
    );
    let err = parse_manifest(&routed)
        .expect("shape is fine")
        .validate()
        .expect_err("routed into a hidden region");
    assert!(err.to_string().contains("sources"), "{err}");
}

// ─── Arithmetic on hostile numbers ─────────────────────────────────────

/// `max_tokens = -1` became `usize::MAX` through `as usize`, and the default
/// compaction threshold (`max_tokens * 8 / 10`) then overflowed. With
/// `overflow-checks` on in release that aborted the daemon on load, taking
/// every running agent with it. Loading must not panic; whether it should be
/// refused is a separate question (it will be).
#[test]
fn a_negative_max_tokens_on_a_compacting_region_does_not_abort() {
    let toml = r#"
[agent]
name = "neg"

[context.regions]
notes = { kind = "compacting", max_tokens = -1 }
"#;
    let _ = parse_manifest(toml);
}

/// Two such regions overflowed the summed total instead.
#[test]
fn two_negative_budgets_do_not_abort_the_total() {
    let toml = r#"
[agent]
name = "neg"

[context.regions]
a = { kind = "pinned", max_tokens = -1 }
b = { kind = "pinned", max_tokens = -1 }
"#;
    let _ = parse_manifest(toml);
}

// ─── sandbox mount spellings ─────────────────────────────────────────────

fn sandbox_manifest(table: &str) -> String {
    format!(
        "[agent]\nname = \"a\"\n\n[sandbox]\n{table}\n\n\
         [stages.s]\nmodel = {{ provider = \"anthropic\", model = \"m\" }}\n"
    )
}

#[test]
fn sandbox_mounts_is_read_the_same_as_mount() {
    let bp = parse_manifest(&sandbox_manifest("mounts = [\"/data\", \"/cache\"]")).unwrap();
    assert_eq!(
        bp.sandbox.unwrap().mounts,
        vec!["/data".to_string(), "/cache".to_string()]
    );
}

#[test]
fn sandbox_accepts_both_spellings_when_they_agree() {
    let bp = parse_manifest(&sandbox_manifest(
        "mount = [\"/data\"]\nmounts = [\"/data\"]",
    ))
    .unwrap();
    assert_eq!(bp.sandbox.unwrap().mounts, vec!["/data".to_string()]);
}

#[test]
fn sandbox_refuses_both_spellings_when_they_differ() {
    let err = parse_manifest(&sandbox_manifest(
        "mount = [\"/data\"]\nmounts = [\"/other\"]",
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("both `mount` and `mounts`"), "{err}");
}

// ─── the published schema and the parser name the same keys ─────────────

/// The schema is what people validate against and the parser is what runs;
/// a key in one and not the other is either a silently ignored setting or a
/// validation failure for a working blueprint. Both directions, per table.
#[test]
fn the_published_schema_and_the_parser_agree_on_every_key() {
    let schema: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../docs/schema/blueprint.schema.json"
    ))
    .expect("the schema is valid JSON");
    let keys_at = |path: &[&str]| -> Vec<String> {
        let mut node = &schema;
        for step in path {
            node = &node[step];
        }
        let mut keys: Vec<String> = node["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("no properties at {path:?}"))
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    };
    let sorted = |list: &[&str]| -> Vec<String> {
        let mut v: Vec<String> = list.iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    };
    for (table, schema_path, parser) in [
        ("stage", &["$defs", "stage"][..], super::stage::STAGE_KEYS),
        (
            "stage.context",
            &["$defs", "stage", "properties", "context"][..],
            super::stage::CONTEXT_KEYS,
        ),
        (
            "stage.tool_routing",
            &["$defs", "stage", "properties", "tool_routing"][..],
            super::stage::TOOL_ROUTING_KEYS,
        ),
        (
            "transition",
            &["$defs", "transition"][..],
            super::stage::EDGE_KEYS,
        ),
        (
            "transition.gate",
            &["$defs", "transition", "properties", "gate"][..],
            super::stage::GATE_KEYS,
        ),
        (
            "sandbox",
            &["$defs", "sandbox"][..],
            super::sections::SANDBOX_KEYS,
        ),
        // The tables below are read key by key with nothing refusing a
        // stranger, so their lists are kept by hand beside each parser and
        // this is the only thing that checks them.
        (
            "region",
            &["$defs", "region"][..],
            super::regions::REGION_KEYS,
        ),
        (
            "output",
            &["$defs", "outputSpec"][..],
            super::sections::OUTPUT_KEYS,
        ),
        (
            "interaction point",
            &["$defs", "interactionPoint"][..],
            super::stage::INTERACTION_POINT_KEYS,
        ),
        (
            "stage.model",
            &["$defs", "modelConfig"][..],
            super::model::MODEL_KEYS,
        ),
        (
            "stage.hooks",
            &["$defs", "stageHooks"][..],
            super::stage::HOOK_KEYS,
        ),
        ("agent", &["$defs", "agent"][..], super::AGENT_KEYS),
    ] {
        assert_eq!(
            keys_at(schema_path),
            sorted(parser),
            "keys of the {table} table"
        );
    }
}

// ─── negative integers and unknown sandbox keys are load errors ─────────

/// Every integer a manifest carries, with a negative value, and the error
/// each must produce. Before this list existed, `max_items = -1` went through
/// `as usize` and became the largest possible cap, `max_child_depth = -1`
/// became unlimited nesting, and a handful of keys quietly dropped the value
/// instead; none of them said anything.
fn negative_integer_cases() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "[agent]\nname = \"a\"\nmax_child_depth = -1\n",
            "[agent]: max_child_depth must not be negative (got -1)",
        ),
        (
            "[agent]\nname = \"a\"\n[compaction]\nmax_summary_tokens = -2\n",
            "[compaction]: max_summary_tokens must not be negative (got -2)",
        ),
        (
            "[agent]\nname = \"a\"\n[context.file_tracking]\nmax_file_tokens = -3\n",
            "[context.file_tracking]: max_file_tokens must not be negative (got -3)",
        ),
        (
            "[agent]\nname = \"a\"\n[repetition_detection]\nmax_repeat_calls = -4\n",
            "[repetition_detection]: max_repeat_calls must not be negative (got -4)",
        ),
        (
            "[agent]\nname = \"a\"\n[repetition_detection]\nmax_readonly_streak = -5\n",
            "[repetition_detection]: max_readonly_streak must not be negative (got -5)",
        ),
        (
            "[agent]\nname = \"a\"\n[context.regions]\nr = { kind = \"pinned\", max_tokens = -6 }\n",
            "region 'r': max_tokens must not be negative (got -6)",
        ),
        (
            "[agent]\nname = \"a\"\n[context.regions]\nr = { kind = \"pinned\", budget = \"10%\", min_tokens = -7 }\n",
            "region 'r': min_tokens must not be negative (got -7)",
        ),
        (
            "[agent]\nname = \"a\"\n[context.regions]\nr = { kind = \"compacting\", threshold_tokens = -8 }\n",
            "region 'r': threshold_tokens must not be negative (got -8)",
        ),
        (
            "[agent]\nname = \"a\"\n[context.regions]\nr = { kind = \"sliding_window\", max_items = -9 }\n",
            "region 'r': max_items must not be negative (got -9)",
        ),
        (
            "[agent]\nname = \"a\"\n[context.regions]\nr = { kind = \"sliding_window\", strategy = \"bulk\", overflow = -10 }\n",
            "region 'r': overflow must not be negative (got -10)",
        ),
        (
            "[agent]\nname = \"a\"\n[context.regions]\nr = { kind = \"sliding_window\", strategy = \"compact\", compact_count = -11 }\n",
            "region 'r': compact_count must not be negative (got -11)",
        ),
        (
            "[agent]\nname = \"a\"\n[context.regions]\nr = { kind = \"hashmap\", max_entries = -12 }\n",
            "region 'r': max_entries must not be negative (got -12)",
        ),
        (
            "[agent]\nname = \"a\"\n[stages.s]\nmax_iterations = -13\n",
            "stage 's': max_iterations must not be negative (got -13)",
        ),
        (
            "[agent]\nname = \"a\"\n[stages.s]\nmax_revisits = -14\n",
            "stage 's': max_revisits must not be negative (got -14)",
        ),
        (
            "[agent]\nname = \"a\"\n[stages.s.tool_routing]\nmax_result_tokens = -15\n",
            "stage 's': tool_routing: max_result_tokens must not be negative (got -15)",
        ),
        (
            "[agent]\nname = \"a\"\n[stages.s.model]\nprovider = \"p\"\nmodel = \"m\"\nrequest_timeout_secs = -16\n",
            "stage 's': model: request_timeout_secs must not be negative (got -16)",
        ),
        (
            "[agent]\nname = \"a\"\n[stages.s.transitions.t]\ngate = { max_attempts = -17 }\n[stages.t]\n",
            "stage 's': transition to 't': gate: max_attempts must not be negative (got -17)",
        ),
        (
            "[agent]\nname = \"a\"\n[stages.s.transitions.t]\ncondition = \"stuck\"\nstuck_after_minutes = -18\n[stages.t]\n",
            "stage 's': transition to 't': stuck_after_minutes must not be negative (got -18)",
        ),
        (
            "[agent]\nname = \"a\"\n[stages.s.transitions.t]\ncondition = \"stuck\"\nstuck_after_iterations = -21\n[stages.t]\n",
            "stage 's': transition to 't': stuck_after_iterations must not be negative (got -21)",
        ),
        (
            "[agent]\nname = \"a\"\n[stages.s.transitions.t]\ncondition = \"stuck\"\nstuck_after_same_file_edits = -22\n[stages.t]\n",
            "stage 's': transition to 't': stuck_after_same_file_edits must not be negative (got -22)",
        ),
        (
            "[agent]\nname = \"a\"\n[stages.s.transitions.t]\ncondition = \"stuck\"\nstuck_after_tool_calls = -23\n[stages.t]\n",
            "stage 's': transition to 't': stuck_after_tool_calls must not be negative (got -23)",
        ),
        (
            "[agent]\nname = \"a\"\n[agent.nudge]\nmax = -19\n",
            "[agent.nudge]: max must not be negative (got -19)",
        ),
        (
            "[agent]\nname = \"a\"\n[stages.s.nudge]\nmax = -20\n",
            "stage 's': nudge: max must not be negative (got -20)",
        ),
    ]
}

#[test]
fn every_negative_manifest_integer_fails_to_load_naming_the_key() {
    for (toml, expected) in negative_integer_cases() {
        let err = match parse_manifest(toml) {
            Ok(_) => panic!("loaded despite a negative value:\n{toml}"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains(expected), "want {expected:?} in {err:?}");
    }
}

/// The same keys at zero still load: zero is a value each key gives a
/// meaning to (unlimited, never hold, unset), not a typo.
#[test]
fn every_manifest_integer_still_loads_at_zero() {
    for (toml, _) in negative_integer_cases() {
        let zeroed = with_negative_literal_zeroed(toml);
        let bp = parse_manifest(&zeroed);
        // A `stuck` edge at zero has no threshold, which is its own error.
        if zeroed.contains("condition = \"stuck\"") {
            let err = bp.unwrap_err().to_string();
            assert!(err.contains("no threshold"), "{err}");
            continue;
        }
        assert!(bp.is_ok(), "{zeroed}\n{:?}", bp.err());
    }
}

/// Replace the one negative literal in a fixture with `0`.
fn with_negative_literal_zeroed(toml: &str) -> String {
    let (head, tail) = toml
        .split_once("= -")
        .expect("fixture has a negative literal");
    let tail = tail.trim_start_matches(|c: char| c.is_ascii_digit());
    format!("{head}= 0{tail}")
}

#[test]
fn sandbox_refuses_an_unknown_key_naming_it() {
    let err = parse_manifest(&sandbox_manifest("netwrok = false"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("sandbox has unknown key 'netwrok'"), "{err}");
    assert!(
        err.contains("network"),
        "the error lists the valid keys: {err}"
    );
}

#[test]
fn a_stage_sandbox_refuses_an_unknown_key_naming_it() {
    let toml = "[agent]\nname = \"a\"\n[stages.s.sandbox]\nnetwrok = false\n";
    let err = parse_manifest(toml).unwrap_err().to_string();
    assert!(
        err.contains("stage 's': sandbox has unknown key 'netwrok'"),
        "{err}"
    );
}
