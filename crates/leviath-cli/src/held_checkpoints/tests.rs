use super::*;

/// A manifest with one interactive stage whose point carries `unattended`, and
/// one autonomous stage whose `required_tools` keep a blocking tool.
fn manifest(unattended: &str, required: &str) -> String {
    format!(
        r#"
[agent]
name = "held-fixture"
version = "0.1.0"
description = "a fixture"

[stages.plan]
mode = "interactive_points"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
max_iterations = 5
available_tools = ["read_file", "ask_user_text"]
required_tools = [{required}]

[[stages.plan.interaction_points]]
name = "plan_approval"
prompt = "Review the plan"
style = "confirm"
{unattended}

[stages.plan.transitions.build]
condition = "always"

[stages.build]
mode = "autonomous"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-5" }}] }}
max_iterations = 5
available_tools = ["read_file"]

[context.regions]
system = {{ kind = "pinned", max_tokens = 1000 }}
conversation = {{ kind = "sliding_window", max_items = 50, max_tokens = 10000 }}
"#
    )
}

fn parse(unattended: &str, required: &str) -> leviath_core::Blueprint {
    leviath_core::manifest::parse_manifest(&manifest(unattended, required)).expect("fixture parses")
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
    assert!(preflight_lines(&parse("", ""), 3600).is_empty());
}

#[test]
fn the_preflight_names_every_checkpoint_and_the_deadline() {
    let lines = preflight_lines(&parse(r#"unattended = "ask""#, r#""ask_user_text""#), 3600);
    let block = lines.join("\n");
    assert!(block.contains("2 checkpoints"), "{block}");
    assert!(block.contains("plan: plan_approval"), "{block}");
    assert!(block.contains("plan: ask_user_text"), "{block}");
    assert!(block.contains("after 1h"), "{block}");
    assert!(block.contains("lev respond"), "{block}");

    // One checkpoint reads as one, not "1 checkpoints".
    let one = preflight_lines(&parse(r#"unattended = "ask""#, ""), 3600).join("\n");
    assert!(one.contains("1 checkpoint:"), "{one}");
}

/// `interaction_timeout_secs = 0` means wait forever, and saying "after 0s"
/// would be the opposite of the truth.
#[test]
fn a_disabled_timeout_says_the_run_waits() {
    let block = preflight_lines(&parse(r#"unattended = "ask""#, ""), 0).join("\n");
    assert!(block.contains("until somebody answers"), "{block}");
    assert!(!block.contains("stops with an error"), "{block}");
}

#[test]
fn a_timeout_reads_as_the_operator_wrote_it() {
    assert_eq!(human_timeout(0), "indefinitely");
    assert_eq!(human_timeout(7200), "2h");
    assert_eq!(human_timeout(300), "5m");
    assert_eq!(human_timeout(45), "45s");
}
