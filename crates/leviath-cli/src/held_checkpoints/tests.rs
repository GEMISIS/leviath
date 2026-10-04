use super::*;

/// A blueprint with one stage whose interaction point carries `unattended`
/// and whose `required_tools` keep `required`, then an autonomous stage.
fn blueprint(unattended: &str, required: &str) -> String {
    format!(
        r#"
[blueprint]
name = "held-fixture"
version = "0.1.0"
description = "a fixture"

[graph]
edges = [{{ name = "build", from = "plan", to = "build" }}]

[[graph.stages]]
name = "plan"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
tools = ["read_file", "ask_user_text"]
required_tools = [{required}]
max_iterations = 5

[[graph.stages.mode.interactive_points]]
name = "plan_approval"
prompt = "Review the plan"
style = "confirm"
{unattended}

[[graph.stages]]
name = "build"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
tools = ["read_file"]
max_iterations = 5

[graph.layout]
total_budget_tokens = 11000
regions = [
    {{ name = "system", kind = "pinned", budget = 1000 }},
    {{ name = "conversation", kind = {{ kind = "sliding_window", max_items = 50 }}, budget = 10000 }},
]
"#
    )
}

fn parse(unattended: &str, required: &str) -> leviath_runtime::spec::graph::RunGraph {
    leviath_blueprint::BlueprintFile::parse(&blueprint(unattended, required))
        .expect("fixture parses")
        .run_graph()
}

#[test]
fn a_point_holds_only_when_it_declares_that_it_needs_a_person() {
    let held = parse(r#"unattended = "ask""#, "");
    assert_eq!(
        held_points(&held),
        [Held {
            stage: "plan".to_string(),
            name: "plan_approval".to_string(),
        }]
    );
    // The default policy is auto-approve, so nothing holds.
    assert!(held_points(&parse("", "")).is_empty());
    assert!(held_points(&parse(r#"unattended = "auto_approve""#, "")).is_empty());
}

/// Only the tools that actually block on a person count. A stage keeping
/// `read_file` in `required_tools` stops nothing.
#[test]
fn only_a_blocking_tool_counts_as_held() {
    let held = parse("", r#""ask_user_text""#);
    assert_eq!(
        held_tools(&held),
        [Held {
            stage: "plan".to_string(),
            name: "ask_user_text".to_string(),
        }]
    );
    assert!(held_tools(&parse("", r#""read_file""#)).is_empty());
    assert!(held_tools(&parse("", "")).is_empty());
}

/// A blueprint that holds nothing says nothing: a `--yolo` run with no
/// checkpoints must not print a block explaining that it has none.
#[test]
fn a_blueprint_that_holds_nothing_prints_nothing() {
    assert!(preflight_lines(&parse("", ""), Some(3600)).is_empty());
}

#[test]
fn the_preflight_names_every_checkpoint_and_the_deadline() {
    let lines = preflight_lines(
        &parse(r#"unattended = "ask""#, r#""ask_user_text""#),
        Some(3600),
    );
    let block = lines.join("\n");
    assert!(block.contains("2 checkpoints"), "{block}");
    assert!(block.contains("plan: plan_approval"), "{block}");
    assert!(block.contains("plan: ask_user_text"), "{block}");
    assert!(block.contains("after 1h"), "{block}");
    assert!(block.contains("lev interactions"), "{block}");

    // One checkpoint reads as one, not "1 checkpoints".
    let one = preflight_lines(&parse(r#"unattended = "ask""#, ""), Some(3600)).join("\n");
    assert!(one.contains("1 checkpoint:"), "{one}");
}

/// No `interaction_timeout_secs` means the run waits for a person, and so
/// does an explicit `0`; saying "after 0s" would be the opposite of the truth.
#[test]
fn a_disabled_timeout_says_the_run_waits() {
    for unset in [None, Some(0)] {
        let block = preflight_lines(&parse(r#"unattended = "ask""#, ""), unset).join("\n");
        assert!(
            block.contains("until somebody answers"),
            "{unset:?}: {block}"
        );
        assert!(!block.contains("stops with an error"), "{unset:?}: {block}");
    }
}

#[test]
fn a_timeout_reads_as_the_operator_wrote_it() {
    assert_eq!(human_timeout(7200), "2h");
    assert_eq!(human_timeout(300), "5m");
    assert_eq!(human_timeout(45), "45s");
}
