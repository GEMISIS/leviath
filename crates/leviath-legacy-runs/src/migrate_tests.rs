use leviath_runtime::spec::graph::{CodeRef, Seed};

use super::*;

#[test]
fn a_migrated_file_leaves_defaults_out_and_writes_short_tables_inline() {
    let old = r#"
[agent]
name = "small"
version = "0.3.0"

[context.regions]
task = { kind = "pinned", max_tokens = 500, seed = "task" }

[stages.main]
system_prompt = "work"
available_tools = ["read_file", "list_dir", "bash", "write_file", "edit_file", "context_read"]
"#;
    let text = migrate(old).unwrap();
    assert!(text.starts_with("[blueprint]\nname = \"small\"\nversion = \"0.3.0\"\n"));
    assert!(!text.contains("description"), "{text}");
    assert!(!text.contains("title"), "{text}");
    assert!(!text.contains("= false"), "{text}");
    assert!(!text.contains("= []"), "{text}");
    assert!(text.contains("budget = 500 }"), "{text}");
    assert!(text.contains("tools = [\n    \"read_file\",\n"), "{text}");
    assert!(text.contains("binds = [{ region = \"task\" }]"), "{text}");
    assert!(
        text.contains("type = { kind = \"text\", multiline = true }"),
        "{text}"
    );
    let file = BlueprintFile::parse(&text).unwrap();
    assert_eq!(file.run_graph().title.as_deref(), Some("small"));
    // Reading the written file gives the graph the manifest describes.
    let expected = crate::old::graph::from_blueprint(&parse_manifest(old).unwrap()).unwrap();
    assert_eq!(file.run_graph(), expected);
}

/// A fan-out the manifest let run every item at once (`max_workers = 0`)
/// leaves `max_workers` out of the graph, and the conversion says so. Any
/// other cap is kept as written.
#[test]
fn a_fan_out_with_no_cap_leaves_max_workers_out_and_says_so() {
    let old = |workers: u32| {
        format!(
            "[agent]\nname = \"f\"\n\n[stages.split]\nmode = \"fan_out\"\nworker_agent = \
             \"helper\"\nmax_workers = {workers}\n"
        )
    };
    let Migrated { text, notes, .. } = migrate_noted(&old(0)).unwrap();
    assert!(!text.contains("max_workers"), "{text}");
    assert_eq!(
        notes,
        vec![
            "stages.split.mode.fan_out.max_workers: max_workers = 0 (no cap) is left out, which \
             is how a graph says no cap"
                .to_string()
        ]
    );
    let Migrated { text, notes, .. } = migrate_noted(&old(5)).unwrap();
    assert!(text.contains("max_workers = 5"), "{text}");
    assert!(notes.is_empty(), "{notes:?}");
}

/// A temperature is written the way the manifest wrote it: `0.2`, not the
/// `0.20000000298023224` a single-precision number widens to.
#[test]
fn a_temperature_is_written_as_it_was_given() {
    let old = r#"
[agent]
name = "warm"

[compaction]
provider = "openai"
model = "gpt-mini"
temperature = 0.2

[stages.main]
system_prompt = "work"

[stages.main.model]
provider = "openai"
model = "gpt-mini"

[stages.main.model.parameters]
temperature = 0.3
"#;
    let text = migrate(old).unwrap();
    assert!(text.contains("temperature = 0.2\n"), "{text}");
    assert!(text.contains("temperature = 0.3"), "{text}");
    assert!(!text.contains("0.2000"), "{text}");
    assert!(!text.contains("0.3000"), "{text}");
}

#[test]
fn a_manifest_that_does_not_parse_is_reported() {
    let problems = migrate("[stages.main]\n").unwrap_err();
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains("[agent]"), "{problems:?}");
}

#[test]
fn every_problem_with_a_manifest_is_reported_at_once() {
    let old = r#"
[agent]
name = " padded"

[stages.main]
system_prompt = "work"
available_tools = ["read file"]
"#;
    let problems = migrate_file(old).unwrap_err();
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(problems[0].contains("tool name"), "{problems:?}");
    assert!(problems[1].contains("[agent] name"), "{problems:?}");
}

/// A one-stage manifest named `t` with `stage` written into its stage table
/// and `rest` after it.
fn manifest(stage: &str, rest: &str) -> String {
    format!("[agent]\nname = \"t\"\n\n[stages.main]\nsystem_prompt = \"p\"\n{stage}\n{rest}")
}

/// A bare model id, as a stage's `model` or as the only key of its model
/// table, stays a model with no provider, so the operator's routes pick one.
#[test]
fn a_bare_model_keeps_no_provider() {
    for model in ["model = \"gpt-x\"", "model = { model = \"gpt-x\" }"] {
        let file = migrate_file(&manifest(model, "")).unwrap();
        let models = &file.graph.stages[0].model.models;
        assert_eq!(models.len(), 1, "{model}");
        assert_eq!(models[0].provider, None, "{model}");
        assert_eq!(models[0].model.as_str(), "gpt-x", "{model}");
    }
}

/// Every key nothing reads was accepted and ignored by the release that
/// wrote the manifest, so the conversion leaves it out and names it in a
/// note, wherever it sits, rather than refusing the blueprint.
#[test]
fn a_key_nothing_reads_is_left_out_and_named_with_its_value() {
    let old = r#"
[agent]
name = "t"
colour = "blue"

[nudge]
max = 3

[context.regions]
log = { kind = "pinned", max_tokens = 100, max_stored = 5 }

[stages.main]
system_prompt = "p"
"#;
    let Migrated {
        name,
        text,
        notes,
        dropped,
    } = migrate_noted(old).unwrap();
    assert_eq!(name, "t");
    assert!(!text.contains("max_stored"), "{text}");
    assert!(!text.contains("colour"), "{text}");
    assert!(notes.is_empty(), "{notes:?}");
    let notes: Vec<String> = dropped.iter().map(ToString::to_string).collect();
    assert_eq!(notes.len(), 3, "{notes:?}");
    assert!(
        notes
            .iter()
            .any(|p| p.contains("`nudge = ") && p.contains("[agent.nudge]")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|p| p.contains("region 'log'") && p.contains("`max_stored = 5`")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|p| p.contains("[agent]") && p.contains("`colour = \"blue\"`")),
        "{notes:?}"
    );
}

/// `[read_paths]` and `[safe_commands]` carry over.
#[test]
fn read_paths_and_safe_commands_carry_over() {
    let file = migrate_file(&manifest(
        "",
        "[read_paths]\nallow = [\"~/docs\"]\n\n[safe_commands]\ntools = [\"read_file\"]\nshell = [\"cargo test\"]\n",
    ))
    .unwrap();
    assert_eq!(file.graph.read_paths, ["~/docs"]);
    assert_eq!(file.graph.safe_commands.tools[0].as_str(), "read_file");
    assert_eq!(file.graph.safe_commands.shell, ["cargo test"]);
}

/// A stage that turns its sandbox off says so in the new file, rather than
/// writing an empty table a reader has to decode.
#[test]
fn a_sandbox_turned_off_says_so() {
    let text = migrate(&manifest("", "[stages.main.sandbox]\nkind = \"none\"\n")).unwrap();
    assert!(text.contains("sandbox = { kind = \"none\" }"), "{text}");
}

/// A code seed the blueprint ships, written with the `blueprint:` prefix,
/// becomes a file beside the blueprint, which is what a code file is.
#[test]
fn a_shipped_code_seed_loses_its_prefix() {
    let file = migrate_file(&manifest(
        "",
        "[context.regions]\nplan = { kind = \"pinned\", max_tokens = 100, seed = { rhai = \"blueprint:seeds/plan.rhai\" } }\n",
    ))
    .unwrap();
    assert_eq!(
        file.graph.layout.regions[0].seed,
        Some(Seed::Code(CodeRef::File("seeds/plan.rhai".to_string())))
    );
}

/// A stage that re-declares the task region binds the task to it once.
#[test]
fn a_region_declared_twice_is_bound_once() {
    let file = migrate_file(&manifest(
        "",
        "[context.regions]\ntask = { kind = \"pinned\", max_tokens = 100 }\n\n\
         [stages.main.context.regions]\ntask = { kind = \"pinned\", max_tokens = 100 }\n",
    ))
    .unwrap();
    let task = &file.graph.inputs[0];
    assert_eq!(task.name.as_str(), "task");
    assert_eq!(task.binds.len(), 1, "{:?}", task.binds);
}
