use super::*;

/// A blueprint whose fan-out is written the old way, as a table of its own
/// with a comment, its worker a stage with a description.
const TABLE_FORM: &str = r#"[blueprint]
name = "split"
version = "0.1.0"

[graph]
entry = "split"
layout = { total_budget_tokens = 1000, regions = [{ name = "task", kind = "pinned", budget = 500 }] }
inputs = [{ name = "task", type = "text", binds = [{ region = "task" }] }]

[[graph.stages]]
name = "split"

# Hand the work out.
[graph.stages.mode.fan_out]
worker = { stage = "w" }
max_workers = 4

[[graph.stages]]
name = "w"
description = "Doing one part"
allow_as_worker = true
"#;

/// A fan-out of its stage with one worker, named after the stage it runs,
/// for what that stage's description says; the comment and the other keys
/// stay, and the result reads as a blueprint.
#[test]
fn a_stage_worker_becomes_the_one_worker_of_its_stage() {
    let (text, changed) = rewrite_fan_outs(TABLE_FORM).unwrap().unwrap();
    assert_eq!(
        changed,
        [Rewritten {
            stage: "split".into(),
            worker: "w".into()
        }]
    );
    assert!(text.contains("# Hand the work out."), "{text}");
    assert!(text.contains("max_workers = 4"), "{text}");
    assert!(!text.contains("worker = {"), "{text}");
    let file = leviath_blueprint::BlueprintFile::parse(&text).unwrap();
    let graph = file.run_graph();
    let leviath_runtime::spec::graph::StageMode::FanOut(fan) = &graph.stages[0].mode else {
        panic!("still a fan-out");
    };
    assert_eq!(fan.stage.as_str(), "split");
    let worker = fan.sole_worker().unwrap();
    assert_eq!(worker.name.as_str(), "w");
    assert_eq!(worker.use_for, "Doing one part");
    assert!(
        graph
            .validate(&leviath_runtime::spec::issues::SpecPath::root())
            .is_ok()
    );
}

/// Every old kind of worker, written inline, becomes its entry: a blueprint
/// by name (pinned or not) or by directory, and a query, each for every
/// item. A fan-out that already names its stage keeps it.
#[test]
fn every_old_worker_kind_is_rewritten() {
    let stage = |name: &str, worker: &str| {
        format!(
            "[[graph.stages]]\nname = \"{name}\"\nmode = {{ fan_out = {{ worker = {worker} }} }}\n\n"
        )
    };
    let text = [
        stage("a", r#"{ blueprint = { name = "researcher" } }"#),
        stage("b", r#"{ blueprint = { name = "researcher", digest = "ab" } }"#),
        stage("c", r#"{ blueprint_file = "/agents/reader/" }"#),
        stage("d", r#"{ query = "a reviewer" }"#),
        "[[graph.stages]]\nname = \"e\"\nmode = { fan_out = { stage = \"e\", worker = { stage = \"a\" } } }\n".into(),
    ]
    .concat();
    let (out, changed) = rewrite_fan_outs(&format!("[graph]\n{text}"))
        .unwrap()
        .unwrap();
    let workers: Vec<&str> = changed.iter().map(|c| c.worker.as_str()).collect();
    assert_eq!(
        workers,
        ["researcher", "researcher", "reader", "worker", "a"]
    );
    for line in [
        r#"{ name = "researcher", blueprint = "researcher", use_for = "Takes every item this fan-out hands out" }"#,
        r#"blueprint = "researcher@ab""#,
        r#"{ name = "reader", blueprint = "/agents/reader/""#,
        r#"{ name = "worker", query = "a reviewer""#,
        r#"fan_out = { stage = "e", workers_available = [{ name = "a", stage = "a""#,
    ] {
        assert!(out.contains(line), "{line}\n{out}");
    }
}

/// The old form is told from the file alone: TOML with a fan-out naming a
/// `worker`.
#[test]
fn the_old_form_is_told_from_the_text() {
    assert!(names_one_worker(TABLE_FORM));
    let (new, _) = rewrite_fan_outs(TABLE_FORM).unwrap().unwrap();
    assert!(!names_one_worker(&new));
    assert!(!names_one_worker("not = [toml"));
    assert!(!names_one_worker("[blueprint]\nname = \"x\"\n"));
}

/// A file with no old fan-out changes nothing; one that is not TOML, or
/// whose worker names nothing, is refused.
#[test]
fn nothing_to_rewrite_or_nothing_readable() {
    assert_eq!(
        rewrite_fan_outs("[blueprint]\nname = \"x\"\n").unwrap(),
        None
    );
    let new_form = "[graph]\n[[graph.stages]]\nname = \"a\"\nmode = { fan_out = { stage = \"a\", workers_available = [] } }\n";
    assert_eq!(rewrite_fan_outs(new_form).unwrap(), None);
    assert!(rewrite_fan_outs("not = [toml").is_err());
    let nothing = "[graph]\n[[graph.stages]]\nname = \"a\"\nmode = { fan_out = { worker = { blueprint = {} } } }\n";
    let err = rewrite_fan_outs(nothing).unwrap_err();
    assert!(err.contains("stage 'a'"), "{err}");
}
