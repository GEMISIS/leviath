//! The editing model, exercised on the bundled agents and on the starter.

use super::check::{Severity, check};
use super::*;
use crate::bundled::BUNDLED_AGENTS;

fn bundled_text(name: &str) -> &'static str {
    let agent = BUNDLED_AGENTS
        .iter()
        .find(|a| a.name == name)
        .expect("bundled agent exists");
    catalog::bundled_manifest(agent)
}

fn coder() -> ManifestDoc {
    ManifestDoc::parse(bundled_text("coder")).expect("coder parses")
}

fn reviewer() -> ManifestDoc {
    ManifestDoc::parse(bundled_text("reviewer")).expect("reviewer parses")
}

fn starter() -> ManifestDoc {
    ManifestDoc::parse(&templates::empty_blueprint("demo").unwrap()).unwrap()
}

/// The manifest re-read by the runtime after an edit: every mutator must
/// leave something it accepts.
fn runtime_ok(doc: &ManifestDoc) -> leviath_core::Blueprint {
    let text = doc.to_toml();
    doc.blueprint()
        .unwrap_or_else(|e| panic!("the runtime rejected the edited manifest: {e}\n{text}"))
}

#[test]
fn names_are_the_runtimes_charset() {
    for ok in ["plan", "error_recovery", "v1.2", "step-3", "A9"] {
        assert!(is_valid_name(ok), "{ok}");
        assert_eq!(require_name(ok), Ok(()));
    }
    for bad in ["", "two words", "naïve", "a/b", "tab\t"] {
        assert!(!is_valid_name(bad), "{bad:?}");
        assert_eq!(require_name(bad), Err(EditError::BadName(bad.to_string())));
    }
}

#[test]
fn errors_read_as_sentences() {
    assert_eq!(
        EditError::Taken("plan".into()).to_string(),
        "\"plan\" is already taken"
    );
    assert_eq!(
        EditError::NoSuchEdge("a".into(), "b".into()).to_string(),
        "there is no path from \"a\" to \"b\""
    );
    assert_eq!(
        EditError::LastStage.to_string(),
        "an agent needs at least one stage"
    );
    assert_eq!(
        EditError::NoStages.to_string(),
        "the manifest has no stages"
    );
    assert_eq!(
        EditError::NoAgent.to_string(),
        "the manifest has no [agent] table"
    );
    assert_eq!(
        EditError::NoSuchRegion("r".into()).to_string(),
        "there is no region named \"r\""
    );
    assert_eq!(
        EditError::NotATable("k".into()).to_string(),
        "`k` is not a table this editor can write into"
    );
    assert_eq!(EditError::OutOfRange("x".into()).to_string(), "x");
    assert!(
        EditError::BadName("a b".into())
            .to_string()
            .contains("will not work as a name")
    );
    assert_eq!(
        EditError::NoSuchStage("s".into()).to_string(),
        "there is no stage named \"s\""
    );
}

// ─── parse and round-trip ────────────────────────────────────────────────────

#[test]
fn every_bundled_manifest_round_trips_byte_for_byte() {
    for agent in BUNDLED_AGENTS {
        let text = catalog::bundled_manifest(agent);
        let doc = ManifestDoc::parse(text).expect(agent.name);
        // A Windows checkout carries CRLF; the writer keeps them inside
        // strings and normalises the rest, so compare line endings apart.
        assert_eq!(
            doc.to_toml().replace("\r\n", "\n"),
            text.replace("\r\n", "\n"),
            "{} changed on the way through",
            agent.name
        );
        let bp = runtime_ok(&doc);
        assert_eq!(bp.name, agent.name);
        // The views see every stage the runtime sees.
        let mut names = doc.stage_names();
        names.sort();
        let mut runtime: Vec<String> = bp.stages.iter().map(|s| s.name.clone()).collect();
        runtime.sort();
        assert_eq!(names, runtime, "{}", agent.name);
    }
}

#[test]
fn parse_refuses_what_the_editor_cannot_stand_on() {
    assert!(matches!(
        ManifestDoc::parse("not = [toml"),
        Err(EditError::Toml(_))
    ));
    assert_eq!(
        ManifestDoc::parse("[stages.a]\nmode = \"autonomous\"\n").unwrap_err(),
        EditError::NoAgent
    );
    assert_eq!(
        ManifestDoc::parse("agent = 3\n[stages.a]\n").unwrap_err(),
        EditError::NoAgent
    );
    assert_eq!(
        ManifestDoc::parse("[agent]\nname = \"x\"\n").unwrap_err(),
        EditError::NoStages
    );
    assert_eq!(
        ManifestDoc::parse("[agent]\nname = \"x\"\n[stages]\nplan = 3\n").unwrap_err(),
        EditError::NoStages
    );
    let err = ManifestDoc::parse("not = [toml").unwrap_err().to_string();
    assert!(err.starts_with("not valid TOML: "), "{err}");
}

#[test]
fn the_views_read_the_coder_the_way_the_lair_does() {
    let doc = coder();
    let agent = doc.agent();
    assert_eq!(agent.name, "coder");
    assert_eq!(agent.version, "0.1.0");
    assert_eq!(agent.entry_stage.as_deref(), Some("discover"));
    assert!(agent.description.starts_with("Coding agent"));
    assert_eq!(
        agent.default_model, None,
        "stages start with different models"
    );

    let names = doc.stage_names();
    assert_eq!(names[0], "discover");
    assert_eq!(names[1], "plan");
    assert!(names.contains(&"summary".to_string()));

    let discover = doc.stage("discover").unwrap();
    assert_eq!(discover.mode, StageModeView::Autonomous);
    assert_eq!(discover.max_iterations, Some(8));
    assert_eq!(discover.max_revisits, Some(2));
    assert_eq!(discover.models[0], "anthropic/claude-sonnet-5");
    assert_eq!(discover.models.len(), 5);
    assert_eq!(
        discover.tools,
        [
            "read_file",
            "list_dir",
            "bash",
            "context_write",
            "context_read"
        ]
    );
    assert!(discover.system_prompt.contains("Before any planning"));
    assert!(!discover.is_terminal);
    assert!(!discover.has_own_layout);
    assert_eq!(discover.allow_complete, None);
    assert_eq!(discover.fan_out, FanOutView::default());

    let plan = doc.stage("plan").unwrap();
    assert_eq!(plan.mode, StageModeView::InteractivePoints);

    let edges = doc.edges();
    let to_plan = edges
        .iter()
        .find(|e| e.from == "discover" && e.to == "plan")
        .unwrap();
    assert_eq!(to_plan.kind, EdgeKind::Hint);
    assert_eq!(to_plan.transform, TransformKind::Direct);
    assert!(!to_plan.gated);
    let recovery = doc.edge("discover", "error_recovery").unwrap();
    assert_eq!(recovery.kind, EdgeKind::Error);
    let dead = doc.edge("discover", "summary").unwrap();
    assert_eq!(dead.kind, EdgeKind::DeadEnd);
    let review = doc.edge("implement", "review").unwrap();
    assert!(review.gated);
    assert_eq!(review.transform, TransformKind::Compact);
    let reassess = doc.edge("implement", "reassess").unwrap();
    assert_eq!(reassess.kind, EdgeKind::Stuck);
    assert_eq!(reassess.transform, TransformKind::Custom);
    assert!(reassess.rules.present);
    assert!(reassess.rules.carry.contains(&"plan".to_string()));
    assert_eq!(reassess.rules.compact, ["conversation"]);
    assert_eq!(reassess.rules.clear, ["scratch"]);
    assert!(
        reassess
            .rules
            .compact_prompt
            .starts_with("Summarize what was attempted")
    );
    assert!(doc.edge("discover", "nowhere").is_none());
    assert!(doc.edge("nowhere", "plan").is_none());

    let shared = doc.effective_regions(None);
    assert!(shared.inherited);
    let task = shared.regions.iter().find(|r| r.name == "task").unwrap();
    assert_eq!(task.kind, "pinned");
    assert_eq!(task.budget_percent, Some(2.0));
    // Neither a ceiling nor a floor: the percentage decides the size, and both
    // absolutes are the thing that stopped it deciding anything.
    assert_eq!(task.max_tokens, None);
    assert_eq!(task.min_tokens, None);
    assert!(task.required);
    assert!(task.required_message.starts_with("Describe the task"));
    let conventions = shared
        .regions
        .iter()
        .find(|r| r.name == "conventions")
        .unwrap();
    assert!(conventions.seed_is_table);
    assert_eq!(conventions.seed, "");
    let constraints = shared
        .regions
        .iter()
        .find(|r| r.name == "constraints")
        .unwrap();
    assert_eq!(constraints.seed, "constraints");
    assert!(!constraints.seed_is_table);
    // A stage without its own layout inherits.
    assert!(doc.effective_regions(Some("plan")).inherited);
    assert_eq!(doc.region(None, "task").unwrap().name, "task");
    assert!(doc.region(None, "nope").is_none());
    assert!(doc.regions(Some("plan")).is_empty());

    let routing = doc.tool_routing("discover");
    assert_eq!(routing.default_region.as_deref(), Some("conversation"));
    assert!(
        routing
            .overrides
            .contains(&("read_file".to_string(), "codebase".to_string()))
    );
    assert_eq!(doc.tool_routing("nowhere"), ToolRouting::default());
    let into_codebase = doc.stages_routing_into("codebase");
    assert!(into_codebase.contains(&"discover".to_string()));
    assert!(doc.stages_routing_into("no-such-region").is_empty());

    let tools = doc.known_tools();
    assert!(tools.contains(&"read_file".to_string()));
    assert!(tools.windows(2).all(|w| w[0] < w[1]), "sorted, deduped");
    let models = doc.known_models();
    assert!(models.contains(&"anthropic/claude-opus-5".to_string()));
    assert!(models.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn the_reviewer_shows_its_fan_out_and_worker() {
    let doc = reviewer();
    let split = doc.stage("split_review").unwrap();
    assert_eq!(split.mode, StageModeView::FanOut);
    assert_eq!(
        split.fan_out.worker,
        Some((WorkerKind::Stage, "review_worker".to_string()))
    );
    assert_eq!(split.fan_out.merge_stage.as_deref(), Some("deep_review"));
    let worker = doc.stage("review_worker").unwrap();
    assert!(worker.is_terminal);
}

#[test]
fn a_bare_string_model_and_odd_shapes_read_gently() {
    let doc = ManifestDoc::parse(
        r#"[agent]
name = "odd"
[stages.a]
model = "gpt-4"
mode = "weird"
[stages.a.transitions.b]
condition = "someday"
[stages.a.transitions.c]
[stages.b]
model = { provider = "anthropic", model = "old-form" }
[stages.c]
model = 3
[stages.c.transitions]
"#,
    )
    .unwrap();
    let a = doc.stage("a").unwrap();
    assert_eq!(a.models, ["gpt-4"]);
    assert_eq!(a.mode, StageModeView::Other("weird".into()));
    assert_eq!(a.mode.as_str(), "weird");
    assert_eq!(a.mode.label(), "weird");
    // The unknown condition is left out; the bare table reads as the model's
    // choice.
    let edges = doc.edges();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].to, "c");
    assert_eq!(edges[0].kind, EdgeKind::LlmChoice);
    assert!(doc.edge("a", "b").is_none());
    assert!(doc.stage("b").unwrap().models.is_empty(), "no models list");
    assert!(doc.stage("c").unwrap().models.is_empty());
    assert_eq!(doc.agent().default_model, None);
    assert_eq!(doc.agent().entry_stage, None);
    assert!(doc.stage("ghost").is_none());
    assert!(doc.edge("b", "c").is_none(), "b has no transitions table");
    assert!(doc.regions(Some("ghost")).is_empty());
    assert!(doc.region(Some("ghost"), "x").is_none());
    assert!(doc.regions(Some("b")).is_empty(), "no context table");
    assert!(doc.blueprint().is_err(), "the runtime rejects the odd mode");
    let mut odd = doc.clone();
    odd.rename_stage("c", "d").unwrap();
    assert!(odd.edge("a", "d").is_some());
    odd.delete_stage("d").unwrap();
    assert!(odd.edge("a", "d").is_none());
    assert_eq!(odd.stage_names(), ["a", "b"]);
    // Odder shapes: model entries missing a half, a non-table path, an
    // override that is not a string, a budget that is not a percentage, an
    // unreadable `stages` value, a stage whose transitions are not a table.
    let odder = ManifestDoc::parse(
        r#"[agent]
name = "odder"
[context.regions]
r = { kind = "pinned", budget = "lots" }
[stages.a]
model = { models = [3, { provider = "x" }, { model = "y" }, { provider = "p", model = "m" }] }
transitions = { b = 3, c = { hint = "go" } }
[stages.a.tool_routing.overrides]
bash = 3
read_file = "r"
[stages.c]
transitions = 4
"#,
    )
    .unwrap();
    assert_eq!(odder.stage("a").unwrap().models, ["p/m"]);
    let edges = odder.edges();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].to, "c");
    assert_eq!(
        odder.tool_routing("a").overrides,
        [("read_file".to_string(), "r".to_string())]
    );
    assert_eq!(odder.region(None, "r").unwrap().budget_percent, None);
    let mut odder = odder;
    assert_eq!(
        odder.set_edge_hint("c", "a", "x"),
        Err(EditError::NoSuchEdge("c".into(), "a".into())),
        "transitions is not a table"
    );
    assert_eq!(
        odder.set_edge_hint("b", "a", "x"),
        Err(EditError::NoSuchEdge("b".into(), "a".into())),
        "no such stage reads as no such path"
    );
    assert_eq!(
        odder.set_edge_gate("a", "ghost", true),
        Err(EditError::NoSuchEdge("a".into(), "ghost".into()))
    );
    assert_eq!(
        ManifestDoc::parse("stages = 3\n[agent]\nname = \"x\"\n").unwrap_err(),
        EditError::NoStages
    );
}

#[test]
fn enums_spell_and_label_themselves() {
    for kind in EdgeKind::CHOICES {
        assert!(!kind.label().is_empty());
        assert!(!kind.short().is_empty());
        match kind.condition() {
            Some(c) => assert_eq!(EdgeKind::from_condition(c), Some(kind)),
            None => assert_eq!(kind, EdgeKind::Hint),
        }
    }
    assert_eq!(EdgeKind::from_condition("nope"), None);
    for mode in StageModeView::CHOICES {
        assert_eq!(StageModeView::parse(mode.as_str()), mode);
        assert!(!mode.label().is_empty());
    }
    assert_eq!(
        StageModeView::parse("interactive"),
        StageModeView::Interactive
    );
    assert_eq!(StageModeView::Interactive.label(), "Interactive");
    assert_eq!(StageModeView::Interactive.as_str(), "interactive");
    for t in TransformKind::CHOICES {
        assert_eq!(TransformKind::parse(t.as_str()), t);
        assert!(!t.label().is_empty());
    }
    assert_eq!(TransformKind::parse(""), TransformKind::Direct);
    assert_eq!(TransformKind::parse("summarize"), TransformKind::Compact);
    let other = TransformKind::parse("teleport");
    assert_eq!(other, TransformKind::Other("teleport".into()));
    assert_eq!(other.as_str(), "teleport");
    assert_eq!(other.label(), "teleport");
    assert_eq!(WorkerKind::Agent.key(), "worker_agent");
    assert_eq!(WorkerKind::Query.key(), "worker_query");
    for rule in Rule::ALL {
        assert!(!rule.label().is_empty());
        assert!(!rule.key().is_empty());
    }
    assert_eq!(RegionScope::Shared.stage(), None);
    assert_eq!(RegionScope::Stage("a".into()).stage(), Some("a"));
    assert_eq!(Severity::Warning.tag(), "warning");
    assert_eq!(Severity::Note.tag(), "note");
    assert_eq!(Severity::Error.tag(), "error");
    assert_eq!(catalog::Source::Configured.as_str(), "configured");
    assert_eq!(catalog::Source::Local.as_str(), "local");
    assert_eq!(catalog::Source::Installed.as_str(), "installed");
    assert_eq!(catalog::Source::Bundled.as_str(), "bundled");
}

// ─── agent and stage mutators ────────────────────────────────────────────────

#[test]
fn agent_level_edits() {
    let mut doc = starter();
    doc.set_agent_name("renamed").unwrap();
    assert_eq!(doc.agent().name, "renamed");
    assert_eq!(
        doc.set_agent_name("two words"),
        Err(EditError::BadName("two words".into()))
    );
    doc.set_description("Does things");
    assert_eq!(doc.agent().description, "Does things");
    doc.set_description("");
    assert_eq!(doc.agent().description, "");
    let text = doc.to_toml();
    let agent_table = text.split("[stages").next().unwrap();
    assert!(
        !agent_table.contains("description ="),
        "empty deletes the key: {agent_table}"
    );
    doc.set_entry_stage("finish").unwrap();
    assert_eq!(doc.agent().entry_stage.as_deref(), Some("finish"));
    assert_eq!(
        doc.set_entry_stage("nope"),
        Err(EditError::NoSuchStage("nope".into()))
    );
    // A default model rewrites every stage's chain, keeping the rest behind.
    doc.set_models("work", &["openai/gpt-5".into(), "anthropic/x".into()])
        .unwrap();
    doc.set_default_model("anthropic/x");
    assert_eq!(doc.agent().default_model.as_deref(), Some("anthropic/x"));
    assert_eq!(
        doc.stage("work").unwrap().models,
        ["anthropic/x", "openai/gpt-5"]
    );
    assert_eq!(doc.stage("finish").unwrap().models, ["anthropic/x"]);
    runtime_ok(&doc);
}

#[test]
fn stages_are_added_in_place_and_refused_when_wrong() {
    let mut doc = starter();
    doc.add_stage("review", Some("work")).unwrap();
    assert_eq!(doc.stage_names(), ["work", "review", "finish"]);
    let review = doc.stage("review").unwrap();
    assert_eq!(review.mode, StageModeView::Autonomous);
    assert_eq!(review.max_iterations, Some(20));
    assert!(review.is_terminal, "nowhere to go yet");
    // The file shows it between the two, and the runtime reads it.
    let text = doc.to_toml();
    let work = text.find("[stages.work]").unwrap();
    let rev = text.find("[stages.review]").unwrap();
    let fin = text.find("[stages.finish]").unwrap();
    assert!(work < rev && rev < fin, "{text}");
    runtime_ok(&doc);
    doc.add_stage("last", None).unwrap();
    assert_eq!(doc.stage_names(), ["work", "review", "finish", "last"]);
    // A path out of the new stage: its transitions table stops being written
    // as an empty header, and comes back when the last path goes.
    doc.add_edge("review", "finish").unwrap();
    let text = doc.to_toml();
    assert!(!text.contains("[stages.review.transitions]\n"), "{text}");
    assert!(
        text.contains("[stages.review.transitions.finish]"),
        "{text}"
    );
    doc.delete_edge("review", "finish").unwrap();
    assert!(
        doc.to_toml().contains("[stages.review.transitions]\n"),
        "{}",
        doc.to_toml()
    );
    assert!(doc.stage("review").unwrap().is_terminal);
    // Refusals leave the document alone.
    let before = doc.to_toml();
    assert_eq!(
        doc.add_stage("review", None),
        Err(EditError::Taken("review".into()))
    );
    assert_eq!(
        doc.add_stage("bad name", None),
        Err(EditError::BadName("bad name".into()))
    );
    assert_eq!(
        doc.add_stage("x", Some("ghost")),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    assert_eq!(doc.to_toml(), before);
    // Re-read: the positions survive a round trip.
    let again = ManifestDoc::parse(&doc.to_toml()).unwrap();
    assert_eq!(again.stage_names(), ["work", "review", "finish", "last"]);
}

#[test]
fn renaming_a_stage_rewrites_what_names_it() {
    let mut doc = reviewer();
    doc.rename_stage("review_worker", "checker").unwrap();
    assert!(doc.has_stage("checker") && !doc.has_stage("review_worker"));
    assert_eq!(
        doc.stage("split_review").unwrap().fan_out.worker,
        Some((WorkerKind::Stage, "checker".to_string()))
    );
    doc.rename_stage("discover", "orient").unwrap();
    assert_eq!(doc.agent().entry_stage.as_deref(), Some("orient"));
    // Paths into the renamed stage follow it; the file order does not move.
    doc.rename_stage("report", "wrap_up").unwrap();
    assert!(doc.edge("deep_review", "wrap_up").is_some());
    assert!(doc.edge("deep_review", "report").is_none());
    let names = doc.stage_names();
    assert_eq!(names[0], "orient");
    assert_eq!(
        names.last().map(String::as_str),
        doc.stage_names().last().map(String::as_str)
    );
    // The comment above the renamed stage's header is still above it.
    let text = doc.to_toml();
    assert!(text.contains("# ─── Stage 1: Discover"), "{text}");
    let comment = text.find("# ─── Stage 1: Discover").unwrap();
    let header = text.find("[stages.orient]").unwrap();
    assert!(comment < header, "{text}");
    let between = text.get(comment..header).unwrap();
    assert!(between.matches('\n').count() <= 4, "{text}");
    runtime_ok(&doc);
    // No-op and refusals.
    let before = doc.to_toml();
    doc.rename_stage("orient", "orient").unwrap();
    assert_eq!(
        doc.rename_stage("orient", "scan"),
        Err(EditError::Taken("scan".into()))
    );
    assert_eq!(
        doc.rename_stage("orient", "a b"),
        Err(EditError::BadName("a b".into()))
    );
    assert_eq!(
        doc.rename_stage("ghost", "x"),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    assert_eq!(doc.to_toml(), before);
}

#[test]
fn deleting_a_stage_takes_its_paths_and_repoints_the_entry() {
    let mut doc = starter();
    doc.add_stage("review", Some("work")).unwrap();
    doc.add_edge("review", "finish").unwrap();
    doc.delete_stage("work").unwrap();
    assert_eq!(doc.stage_names(), ["review", "finish"]);
    assert_eq!(doc.agent().entry_stage.as_deref(), Some("review"));
    assert!(doc.edges().iter().all(|e| e.to != "work"));
    doc.delete_stage("finish").unwrap();
    assert!(doc.edges().is_empty(), "the path into finish went with it");
    assert_eq!(doc.delete_stage("review"), Err(EditError::LastStage));
    assert_eq!(
        doc.delete_stage("ghost"),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    assert_eq!(doc.stage_names(), ["review"]);
}

#[test]
fn moving_a_stage_swaps_it_with_its_neighbour_in_the_file() {
    let mut doc = coder();
    let names = doc.stage_names();
    doc.move_stage("plan", true).unwrap();
    let moved = doc.stage_names();
    assert_eq!(moved[0], "plan");
    assert_eq!(moved[1], "discover");
    assert_eq!(&moved[2..], &names[2..]);
    // The file agrees, subtables and all, and still parses.
    let text = doc.to_toml();
    assert!(text.find("[stages.plan]").unwrap() < text.find("[stages.discover]").unwrap());
    assert!(
        text.find("[stages.plan.transitions.implement]").unwrap()
            < text.find("[stages.discover]").unwrap()
    );
    assert!(
        text.find("[[stages.plan.interaction_points]]").unwrap()
            < text.find("[stages.discover]").unwrap()
    );
    runtime_ok(&doc);
    doc.move_stage("plan", false).unwrap();
    assert_eq!(doc.stage_names(), names);
    // At the ends nothing moves; a ghost is refused.
    doc.move_stage("discover", true).unwrap();
    assert_eq!(doc.stage_names(), names);
    let last = names.last().unwrap().clone();
    doc.move_stage(&last, false).unwrap();
    assert_eq!(doc.stage_names(), names);
    assert_eq!(
        doc.move_stage("ghost", true),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    // Re-read after renumbering: same order.
    assert_eq!(
        ManifestDoc::parse(&doc.to_toml()).unwrap().stage_names(),
        names
    );
}

#[test]
fn stage_fields_write_and_delete_the_way_the_lair_does() {
    let mut doc = starter();
    doc.set_stage_mode("work", &StageModeView::FanOut).unwrap();
    doc.set_fan_out(
        "work",
        FanOutField::Worker(Some((WorkerKind::Stage, "finish".into()))),
    )
    .unwrap();
    doc.set_fan_out("work", FanOutField::MergeStage(Some("finish".into())))
        .unwrap();
    doc.set_fan_out("work", FanOutField::MaxWorkers(Some(4)))
        .unwrap();
    doc.set_fan_out("work", FanOutField::MaxItems(Some(9)))
        .unwrap();
    doc.set_fan_out(
        "work",
        FanOutField::OnWorkerFailure(Some("fail_all".into())),
    )
    .unwrap();
    let work = doc.stage("work").unwrap();
    assert_eq!(work.mode, StageModeView::FanOut);
    assert_eq!(
        work.fan_out,
        FanOutView {
            worker: Some((WorkerKind::Stage, "finish".into())),
            merge_stage: Some("finish".into()),
            max_workers: Some(4),
            max_items: Some(9),
            on_worker_failure: Some("fail_all".into()),
        }
    );
    // Switching the worker kind drops the other keys; clearing removes.
    doc.set_fan_out(
        "work",
        FanOutField::Worker(Some((WorkerKind::Agent, "researcher".into()))),
    )
    .unwrap();
    assert!(!doc.to_toml().contains("worker_stage"));
    doc.set_fan_out("work", FanOutField::Worker(None)).unwrap();
    doc.set_fan_out("work", FanOutField::MergeStage(None))
        .unwrap();
    doc.set_fan_out("work", FanOutField::MaxWorkers(None))
        .unwrap();
    doc.set_fan_out("work", FanOutField::MaxItems(None))
        .unwrap();
    doc.set_fan_out("work", FanOutField::OnWorkerFailure(None))
        .unwrap();
    assert_eq!(doc.stage("work").unwrap().fan_out, FanOutView::default());
    // Leaving fan-out sweeps the fan-out keys.
    doc.set_fan_out("work", FanOutField::MaxWorkers(Some(2)))
        .unwrap();
    doc.set_stage_mode("work", &StageModeView::Autonomous)
        .unwrap();
    assert!(!doc.to_toml().contains("max_workers"));

    doc.set_stage_text("work", StageText::Description, "Plan it")
        .unwrap();
    doc.set_stage_text("work", StageText::SystemPrompt, "line one\nline two\n")
        .unwrap();
    doc.set_stage_text("work", StageText::TransitionPrompt, "pick")
        .unwrap();
    let work = doc.stage("work").unwrap();
    assert_eq!(work.description, "Plan it");
    assert_eq!(work.system_prompt, "line one\nline two\n");
    assert_eq!(work.transition_prompt, "pick");
    assert!(
        doc.to_toml().contains("\"\"\""),
        "a multi-line prompt is a multi-line string"
    );
    doc.set_stage_text("work", StageText::TransitionPrompt, "")
        .unwrap();
    assert!(!doc.to_toml().contains("transition_prompt"));

    doc.set_max_iterations("work", Some(0)).unwrap();
    assert_eq!(
        doc.stage("work").unwrap().max_iterations,
        Some(1),
        "at least one"
    );
    doc.set_max_iterations("work", None).unwrap();
    assert_eq!(doc.stage("work").unwrap().max_iterations, None);
    doc.set_max_revisits("work", Some(3)).unwrap();
    assert_eq!(doc.stage("work").unwrap().max_revisits, Some(3));
    doc.set_max_revisits("work", None).unwrap();
    assert_eq!(doc.stage("work").unwrap().max_revisits, None);
    doc.set_allow_complete("work", Some(true)).unwrap();
    assert_eq!(doc.stage("work").unwrap().allow_complete, Some(true));
    doc.set_allow_complete("work", None).unwrap();
    assert_eq!(doc.stage("work").unwrap().allow_complete, None);

    doc.set_tools("work", &["read_file".into(), "bash".into()])
        .unwrap();
    assert_eq!(doc.stage("work").unwrap().tools, ["read_file", "bash"]);
    doc.set_tools("work", &[]).unwrap();
    assert!(!doc.to_toml().contains("available_tools"));

    // Models: the table form, keeping a sibling key; slashless keeps the
    // model visible with an empty provider; empty deletes.
    doc.set_models("work", &["anthropic/claude".into(), "gpt-4".into()])
        .unwrap();
    let text = doc.to_toml();
    assert!(text.contains(r#"model = { models = [{ provider = "anthropic", model = "claude" }, { provider = "", model = "gpt-4" }] }"#), "{text}");
    assert_eq!(
        doc.stage("work").unwrap().models,
        ["anthropic/claude", "/gpt-4"]
    );
    let mut kept = ManifestDoc::parse(
        "[agent]\nname = \"m\"\n[stages.a]\nmodel = { allow_user_default = false, models = [{ provider = \"x\", model = \"y\" }] }\n",
    )
    .unwrap();
    kept.set_models("a", &["openai/o".into()]).unwrap();
    assert!(
        kept.to_toml().contains("allow_user_default = false"),
        "{}",
        kept.to_toml()
    );
    assert_eq!(kept.stage("a").unwrap().models, ["openai/o"]);
    kept.set_models("a", &[]).unwrap();
    assert!(!kept.to_toml().contains("model"));
    runtime_ok(&doc);

    for err in [
        doc.set_stage_mode("ghost", &StageModeView::Output),
        doc.set_stage_text("ghost", StageText::Description, "x"),
        doc.set_max_iterations("ghost", None),
        doc.set_max_revisits("ghost", None),
        doc.set_allow_complete("ghost", None),
        doc.set_models("ghost", &[]),
        doc.set_tools("ghost", &[]),
        doc.set_fan_out("ghost", FanOutField::MaxItems(None)),
    ] {
        assert_eq!(err, Err(EditError::NoSuchStage("ghost".into())));
    }
}

// ─── paths ───────────────────────────────────────────────────────────────────

#[test]
fn paths_are_added_kinded_gated_and_deleted() {
    let mut doc = starter();
    doc.add_edge("finish", "work").unwrap();
    let edge = doc.edge("finish", "work").unwrap();
    assert_eq!(edge.kind, EdgeKind::Hint);
    assert_eq!(edge.hint.as_deref(), Some(edges::NEW_EDGE_HINT));
    // Again: left alone. To itself: a self-loop, the runtime's shape.
    doc.set_edge_hint("finish", "work", "Go round again")
        .unwrap();
    doc.add_edge("finish", "work").unwrap();
    assert_eq!(
        doc.edge("finish", "work").unwrap().hint.as_deref(),
        Some("Go round again")
    );
    doc.add_edge("work", "work").unwrap();
    assert!(doc.edge("work", "work").is_some());
    assert_eq!(
        doc.add_edge("work", "ghost"),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    assert_eq!(
        doc.add_edge("ghost", "work"),
        Err(EditError::NoSuchStage("ghost".into()))
    );

    // Hint on a path that has a hint keeps the text.
    doc.set_edge_kind("finish", "work", EdgeKind::Hint).unwrap();
    assert_eq!(
        doc.edge("finish", "work").unwrap().hint.as_deref(),
        Some("Go round again")
    );
    for kind in EdgeKind::CHOICES {
        doc.set_edge_kind("finish", "work", kind).unwrap();
        let edge = doc.edge("finish", "work").unwrap();
        assert_eq!(edge.kind, kind, "{kind:?}");
        if kind == EdgeKind::Hint {
            assert!(edge.hint.is_some(), "a hint kind keeps or makes a hint");
        } else {
            assert_eq!(edge.hint, None, "a condition drops the hint");
        }
    }
    // Back to hint with no text left: an empty hint appears.
    doc.set_edge_kind("finish", "work", EdgeKind::Hint).unwrap();
    assert_eq!(
        doc.edge("finish", "work").unwrap().hint.as_deref(),
        Some("")
    );

    doc.set_edge_gate("finish", "work", true).unwrap();
    assert!(doc.edge("finish", "work").unwrap().gated);
    assert!(
        doc.to_toml()
            .contains(r#"gate = { message = "Approve to continue" }"#)
    );
    // A richer gate survives "on"; "off" removes whatever is there.
    let mut rich = coder();
    rich.set_edge_gate("implement", "review", true).unwrap();
    assert!(rich.to_toml().contains("require_modifications = true"));
    rich.set_edge_gate("implement", "review", false).unwrap();
    assert!(!rich.edge("implement", "review").unwrap().gated);

    doc.delete_edge("finish", "work").unwrap();
    assert!(doc.edge("finish", "work").is_none());
    assert_eq!(
        doc.delete_edge("finish", "work"),
        Err(EditError::NoSuchEdge("finish".into(), "work".into()))
    );
    assert_eq!(
        doc.delete_edge("ghost", "work"),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    let mut bare = ManifestDoc::parse("[agent]\nname = \"b\"\n[stages.a]\n").unwrap();
    assert_eq!(
        bare.delete_edge("a", "a"),
        Err(EditError::NoSuchEdge("a".into(), "a".into())),
        "no transitions table"
    );
    assert_eq!(
        doc.set_edge_kind("work", "ghost", EdgeKind::Always),
        Err(EditError::NoSuchEdge("work".into(), "ghost".into()))
    );
    assert_eq!(
        doc.set_edge_hint("ghost", "work", "x"),
        Err(EditError::NoSuchEdge("ghost".into(), "work".into()))
    );
    runtime_ok(&doc);
}

#[test]
fn transforms_and_their_rules() {
    let mut doc = coder();
    // Direct is written as absent; the others by name; an unknown one as is.
    doc.set_transform("discover", "plan", &TransformKind::Clear)
        .unwrap();
    assert_eq!(
        doc.edge("discover", "plan").unwrap().transform,
        TransformKind::Clear
    );
    doc.set_transform("discover", "plan", &TransformKind::Direct)
        .unwrap();
    assert!(!edge_text(&doc, "discover", "plan").contains("transform ="));
    doc.set_transform("discover", "plan", &TransformKind::Other("teleport".into()))
        .unwrap();
    assert_eq!(
        doc.edge("discover", "plan").unwrap().transform,
        TransformKind::Other("teleport".into())
    );
    // The first switch to custom seeds carry with every non-pinned region the
    // stage sees; a second switch does not touch the config.
    doc.set_transform("discover", "plan", &TransformKind::Custom)
        .unwrap();
    let rules = doc.edge("discover", "plan").unwrap().rules;
    assert!(rules.present);
    assert!(rules.carry.contains(&"conversation".to_string()));
    assert!(
        !rules.carry.contains(&"task".to_string()),
        "pinned regions are always carried"
    );
    doc.set_transform_rule("discover", "plan", "conversation", Rule::Compact)
        .unwrap();
    doc.set_transform("discover", "plan", &TransformKind::Compact)
        .unwrap();
    doc.set_transform("discover", "plan", &TransformKind::Custom)
        .unwrap();
    let rules = doc.edge("discover", "plan").unwrap().rules;
    assert_eq!(rules.compact, ["conversation"]);
    assert!(!rules.carry.contains(&"conversation".to_string()));
    // A region files under exactly one list; emptied lists go.
    doc.set_transform_rule("discover", "plan", "conversation", Rule::Clear)
        .unwrap();
    let rules = doc.edge("discover", "plan").unwrap().rules;
    assert_eq!(rules.clear, ["conversation"]);
    assert!(rules.compact.is_empty());
    assert!(!edge_text(&doc, "discover", "plan").contains("compact = ["));
    doc.set_compact_prompt("discover", "plan", "Keep the gist")
        .unwrap();
    assert_eq!(
        doc.edge("discover", "plan").unwrap().rules.compact_prompt,
        "Keep the gist"
    );
    doc.set_compact_prompt("discover", "plan", "").unwrap();
    assert_eq!(
        doc.edge("discover", "plan").unwrap().rules.compact_prompt,
        ""
    );
    runtime_ok(&doc);
    // A prompt on a path with no config makes one; clearing one that has no
    // config does nothing.
    let mut fresh = starter();
    fresh.set_compact_prompt("work", "finish", "").unwrap();
    assert!(!fresh.edge("work", "finish").unwrap().rules.present);
    fresh
        .set_compact_prompt("work", "finish", "Summarize")
        .unwrap();
    assert!(fresh.edge("work", "finish").unwrap().rules.present);
    // Custom on a stage with no regions at all still turns custom.
    fresh
        .set_transform("work", "finish", &TransformKind::Custom)
        .unwrap();
    assert_eq!(
        fresh.edge("work", "finish").unwrap().transform,
        TransformKind::Custom
    );
    assert!(fresh.edge("work", "finish").unwrap().rules.carry.is_empty());
    for err in [
        fresh.set_transform("work", "ghost", &TransformKind::Clear),
        fresh.set_transform_rule("work", "ghost", "r", Rule::Carry),
        fresh.set_compact_prompt("work", "ghost", "x"),
    ] {
        assert_eq!(
            err,
            Err(EditError::NoSuchEdge("work".into(), "ghost".into()))
        );
    }
    // A `transform_config` that is not a table is refused, not clobbered.
    let mut odd = ManifestDoc::parse(
        "[agent]\nname = \"o\"\n[context.regions]\nr = { kind = \"temporary\" }\n[stages.a]\n[stages.a.transitions.b]\ntransform_config = 3\n[stages.b]\n",
    )
    .unwrap();
    for err in [
        odd.set_transform("a", "b", &TransformKind::Custom),
        odd.set_transform_rule("a", "b", "r", Rule::Carry),
        odd.set_compact_prompt("a", "b", "x"),
    ] {
        assert_eq!(err, Err(EditError::NotATable("transform_config".into())));
    }
}

fn edge_text(doc: &ManifestDoc, from: &str, to: &str) -> String {
    let text = doc.to_toml();
    let header = format!("[stages.{from}.transitions.{to}]");
    let start = text.find(&header).expect("edge header");
    let rest = text.get(start + header.len()..).unwrap();
    let end = rest.find("\n[").unwrap_or(rest.len());
    rest.get(..end).unwrap().to_string()
}

// ─── regions and routing ─────────────────────────────────────────────────────

#[test]
fn regions_are_added_renamed_deleted_and_edited_in_both_scopes() {
    let mut doc = starter();
    assert!(doc.regions(None).is_empty());
    doc.add_region(&RegionScope::Shared, "notes").unwrap();
    let notes = doc.region(None, "notes").unwrap();
    // A starter region is a percentage and nothing else: a default ceiling is
    // exactly what stops the percentage deciding anything on a real window.
    assert_eq!(
        (notes.kind.as_str(), notes.budget_percent, notes.max_tokens),
        ("pinned", Some(5.0), None)
    );
    assert!(
        doc.to_toml().contains("[context.regions]"),
        "{}",
        doc.to_toml()
    );
    assert_eq!(
        doc.add_region(&RegionScope::Shared, "notes"),
        Err(EditError::Taken("notes".into()))
    );
    assert_eq!(
        doc.add_region(&RegionScope::Shared, "no way"),
        Err(EditError::BadName("no way".into()))
    );
    assert_eq!(
        doc.add_region(&RegionScope::Stage("ghost".into()), "x"),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    // Routing follows a rename in inheriting stages, and clears on delete.
    // A default alone follows a rename; overrides alone follow one too.
    doc.set_tool_routing_default("work", "notes").unwrap();
    doc.rename_region(&RegionScope::Shared, "notes", "notes2")
        .unwrap();
    assert_eq!(
        doc.tool_routing("work").default_region.as_deref(),
        Some("notes2")
    );
    doc.set_tool_routing_default("finish", "").unwrap();
    doc.set_tool_routing_override("finish", "bash", "notes2")
        .unwrap();
    doc.rename_region(&RegionScope::Shared, "notes2", "notes")
        .unwrap();
    assert_eq!(
        doc.tool_routing("finish").overrides,
        [("bash".to_string(), "notes".to_string())]
    );
    doc.set_tool_routing_override("finish", "bash", "").unwrap();
    doc.set_tool_routing_override("work", "bash", "notes")
        .unwrap();
    doc.rename_region(&RegionScope::Shared, "notes", "memo")
        .unwrap();
    assert_eq!(
        doc.tool_routing("work").default_region.as_deref(),
        Some("memo")
    );
    assert_eq!(
        doc.tool_routing("work").overrides,
        [("bash".to_string(), "memo".to_string())]
    );
    assert_eq!(doc.stages_routing_into("memo"), ["work"]);
    doc.rename_region(&RegionScope::Shared, "memo", "memo")
        .unwrap();
    assert_eq!(
        doc.rename_region(&RegionScope::Shared, "memo", "a b"),
        Err(EditError::BadName("a b".into()))
    );
    assert_eq!(
        doc.rename_region(&RegionScope::Shared, "ghost", "x"),
        Err(EditError::NoSuchRegion("ghost".into()))
    );
    assert_eq!(
        doc.rename_region(&RegionScope::Stage("finish".into()), "memo", "x"),
        Err(EditError::NoSuchRegion("memo".into())),
        "finish has no layout of its own"
    );
    for err in [
        doc.rename_region(&RegionScope::Stage("ghost".into()), "memo", "x"),
        doc.delete_region(&RegionScope::Stage("ghost".into()), "memo"),
        doc.set_region_field(
            &RegionScope::Stage("ghost".into()),
            "memo",
            RegionField::Kind,
            RegionValue::Text("pinned".into()),
        ),
    ] {
        assert_eq!(err, Err(EditError::NoSuchRegion("memo".into())));
    }
    doc.add_region(&RegionScope::Shared, "other").unwrap();
    assert_eq!(
        doc.rename_region(&RegionScope::Shared, "memo", "other"),
        Err(EditError::Taken("other".into()))
    );
    // Deleting a region another override still shares the table with keeps
    // the table; deleting the last reference tidies it away.
    doc.add_region(&RegionScope::Shared, "keep").unwrap();
    doc.set_tool_routing_override("work", "read_file", "keep")
        .unwrap();
    doc.delete_region(&RegionScope::Shared, "memo").unwrap();
    assert_eq!(
        doc.tool_routing("work").overrides,
        [("read_file".to_string(), "keep".to_string())]
    );
    assert_eq!(doc.tool_routing("work").default_region, None);
    doc.delete_region(&RegionScope::Shared, "keep").unwrap();
    assert_eq!(doc.tool_routing("work"), ToolRouting::default());
    assert!(
        !doc.to_toml().contains("tool_routing"),
        "tidied away: {}",
        doc.to_toml()
    );
    assert_eq!(
        doc.delete_region(&RegionScope::Shared, "memo"),
        Err(EditError::NoSuchRegion("memo".into()))
    );
    assert_eq!(
        doc.delete_region(&RegionScope::Stage("finish".into()), "x"),
        Err(EditError::NoSuchRegion("x".into()))
    );

    // A stage's own layout: a copy of the shared regions, then independent.
    doc.create_stage_override("finish").unwrap();
    let own = doc.effective_regions(Some("finish"));
    assert!(!own.inherited);
    assert_eq!(
        own.regions
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        ["other"]
    );
    doc.create_stage_override("finish").unwrap(); // already has one: no change
    doc.add_region(&RegionScope::Stage("finish".into()), "scratch")
        .unwrap();
    assert_eq!(doc.regions(Some("finish")).len(), 2);
    assert_eq!(doc.regions(None).len(), 1, "the shared layout is untouched");
    assert!(doc.stage("finish").unwrap().has_own_layout);
    // A stage region's routing rename touches that stage only; the shared
    // rename skips stages with their own layout.
    doc.set_tool_routing_override("finish", "read_file", "scratch")
        .unwrap();
    doc.set_tool_routing_override("work", "read_file", "other")
        .unwrap();
    doc.rename_region(&RegionScope::Stage("finish".into()), "scratch", "pad")
        .unwrap();
    assert_eq!(
        doc.tool_routing("finish").overrides,
        [("read_file".to_string(), "pad".to_string())]
    );
    doc.set_tool_routing_override("finish", "bash", "other")
        .unwrap();
    doc.rename_region(&RegionScope::Shared, "other", "shared_other")
        .unwrap();
    assert_eq!(
        doc.tool_routing("work").overrides,
        [("read_file".to_string(), "shared_other".to_string())]
    );
    assert_eq!(
        doc.tool_routing("finish")
            .overrides
            .iter()
            .find(|(t, _)| t == "bash")
            .unwrap()
            .1,
        "other",
        "its own layout: not rewritten"
    );
    doc.remove_stage_override("finish").unwrap();
    assert!(doc.effective_regions(Some("finish")).inherited);
    assert!(!doc.stage("finish").unwrap().has_own_layout);
    assert_eq!(
        doc.remove_stage_override("ghost"),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    assert_eq!(
        doc.create_stage_override("ghost"),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    // Override on an agent with no shared regions starts empty.
    let mut bare = starter();
    bare.create_stage_override("work").unwrap();
    assert!(!bare.effective_regions(Some("work")).inherited);
    assert!(bare.effective_regions(Some("work")).regions.is_empty());
    runtime_ok(&doc);
}

#[test]
fn region_fields_write_the_lairs_way() {
    use RegionField as F;
    use RegionValue as V;
    let mut doc = starter();
    doc.add_region(&RegionScope::Shared, "r").unwrap();
    let s = RegionScope::Shared;
    doc.set_region_field(&s, "r", F::Kind, V::Text("sliding_window".into()))
        .unwrap();
    doc.set_region_field(&s, "r", F::Kind, V::Text("".into()))
        .unwrap();
    assert_eq!(
        doc.region(None, "r").unwrap().kind,
        "sliding_window",
        "kind is never emptied"
    );
    doc.set_region_field(&s, "r", F::BudgetPercent, V::Number(Some(150)))
        .unwrap();
    assert_eq!(
        doc.region(None, "r").unwrap().budget_percent,
        Some(100.0),
        "clamped"
    );
    doc.set_region_field(&s, "r", F::BudgetPercent, V::Number(None))
        .unwrap();
    assert_eq!(doc.region(None, "r").unwrap().budget_percent, None);
    doc.set_region_field(&s, "r", F::MaxTokens, V::Number(Some(0)))
        .unwrap();
    assert_eq!(doc.region(None, "r").unwrap().max_tokens, Some(1));
    doc.set_region_field(&s, "r", F::MaxTokens, V::Number(None))
        .unwrap();
    assert_eq!(doc.region(None, "r").unwrap().max_tokens, None);
    doc.set_region_field(&s, "r", F::MaxItems, V::Number(Some(12)))
        .unwrap();
    doc.set_region_field(&s, "r", F::Overflow, V::Number(Some(3)))
        .unwrap();
    doc.set_region_field(&s, "r", F::Strategy, V::Text("bulk".into()))
        .unwrap();
    let r = doc.region(None, "r").unwrap();
    assert_eq!(
        (r.max_items, r.overflow, r.strategy.as_str()),
        (Some(12), Some(3), "bulk")
    );
    doc.set_region_field(&s, "r", F::Required, V::Flag(true))
        .unwrap();
    doc.set_region_field(&s, "r", F::RequiredMessage, V::Text("Fill me".into()))
        .unwrap();
    doc.set_region_field(&s, "r", F::Description, V::Text("What it holds".into()))
        .unwrap();
    let r = doc.region(None, "r").unwrap();
    assert!(r.required);
    assert_eq!(r.required_message, "Fill me");
    assert_eq!(r.description, "What it holds");
    doc.set_region_field(&s, "r", F::Required, V::Flag(false))
        .unwrap();
    assert!(!doc.region(None, "r").unwrap().required);
    assert!(!doc.to_toml().contains("required = "));
    doc.set_region_field(&s, "r", F::Seed, V::Text("task".into()))
        .unwrap();
    assert_eq!(doc.region(None, "r").unwrap().seed, "task");
    doc.set_region_field(&s, "r", F::Seed, V::Text("".into()))
        .unwrap();
    assert_eq!(doc.region(None, "r").unwrap().seed, "");
    assert!(!doc.to_toml().contains("seed"));
    // A table seed is displayed empty and never clobbered by clearing.
    let mut coder = coder();
    coder
        .set_region_field(&s, "conventions", F::Seed, V::Text("".into()))
        .unwrap();
    assert!(coder.region(None, "conventions").unwrap().seed_is_table);
    // Mismatched value shapes are refused, unknown regions too.
    assert!(matches!(
        doc.set_region_field(&s, "r", F::Kind, V::Flag(true)),
        Err(EditError::OutOfRange(_))
    ));
    assert_eq!(
        doc.set_region_field(&s, "ghost", F::Kind, V::Text("x".into())),
        Err(EditError::NoSuchRegion("ghost".into()))
    );
    assert_eq!(
        doc.set_region_field(
            &RegionScope::Stage("finish".into()),
            "r",
            F::Kind,
            V::Text("x".into())
        ),
        Err(EditError::NoSuchRegion("r".into()))
    );
    runtime_ok(&doc);
}

#[test]
fn tool_routing_is_created_and_tidied() {
    let mut doc = starter();
    doc.set_tool_routing_default("work", "").unwrap();
    assert!(!doc.to_toml().contains("tool_routing"));
    doc.set_tool_routing_override("work", "bash", "").unwrap();
    assert!(!doc.to_toml().contains("tool_routing"));
    doc.set_tool_routing_default("work", "conversation")
        .unwrap();
    doc.set_tool_routing_override("work", "bash", "scratch")
        .unwrap();
    doc.set_tool_routing_override("work", "read_file", "scratch")
        .unwrap();
    let routing = doc.tool_routing("work");
    assert_eq!(routing.default_region.as_deref(), Some("conversation"));
    assert_eq!(routing.overrides.len(), 2);
    doc.set_tool_routing_override("work", "bash", "").unwrap();
    assert_eq!(doc.tool_routing("work").overrides.len(), 1);
    // The default goes but an override keeps the table.
    doc.set_tool_routing_default("work", "").unwrap();
    assert!(doc.to_toml().contains("tool_routing"));
    doc.set_tool_routing_override("work", "read_file", "")
        .unwrap();
    assert!(!doc.to_toml().contains("tool_routing"), "{}", doc.to_toml());
    // Clearing an override that is not there, with a routing table that has
    // no overrides, is a no-op.
    doc.set_tool_routing_default("work", "conversation")
        .unwrap();
    doc.set_tool_routing_override("work", "bash", "").unwrap();
    assert!(doc.to_toml().contains("tool_routing"));
    doc.set_tool_routing_default("work", "").unwrap();
    assert_eq!(
        doc.set_tool_routing_default("ghost", "x"),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    assert_eq!(
        doc.set_tool_routing_override("ghost", "t", "x"),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    let mut odd = ManifestDoc::parse(
        "[agent]\nname = \"o\"\n[stages.a]\n[stages.a.tool_routing]\noverrides = 3\n",
    )
    .unwrap();
    assert_eq!(
        odd.set_tool_routing_override("a", "bash", "r"),
        Err(EditError::NotATable("overrides".into()))
    );
}

#[test]
fn odd_content_is_refused_rather_than_clobbered() {
    let mut doc = ManifestDoc::parse(
        "[agent]\nname = \"odd\"\n[stages.a]\ncontext = \"nope\"\ntool_routing = 4\ntransitions = 7\n",
    )
    .unwrap();
    assert_eq!(
        doc.add_region(&RegionScope::Stage("a".into()), "r"),
        Err(EditError::NotATable("context".into()))
    );
    assert_eq!(
        doc.create_stage_override("a"),
        Err(EditError::NotATable("context".into()))
    );
    assert_eq!(
        doc.set_tool_routing_default("a", "r"),
        Err(EditError::NotATable("tool_routing".into()))
    );
    assert_eq!(
        doc.set_tool_routing_override("a", "t", "r"),
        Err(EditError::NotATable("tool_routing".into()))
    );
    assert_eq!(
        doc.add_edge("a", "a"),
        Err(EditError::NotATable("transitions".into()))
    );
    // A `[stages]` written inline gets inline children.
    let mut inline = ManifestDoc::parse(
        "stages = { a = { mode = \"autonomous\", transitions = { } } }\n[agent]\nname = \"i\"\n",
    )
    .unwrap();
    inline.add_stage("b", Some("a")).unwrap();
    inline.add_edge("a", "b").unwrap();
    assert_eq!(inline.stage_names(), ["a", "b"]);
    let text = inline.to_toml();
    assert!(text.contains("b = { mode = \"autonomous\""), "{text}");
    assert!(inline.edge("a", "b").is_some());
    runtime_ok(&inline);
    inline.rename_stage("a", "start").unwrap();
    assert!(inline.edge("start", "b").is_some());
    inline.move_stage("b", true).unwrap();
    assert_eq!(
        inline.stage_names(),
        ["start", "b"],
        "inline stages have no position to move"
    );
    // An inline `context = {}` gets an inline copy of the shared regions.
    let mut ctx = ManifestDoc::parse(
        "[agent]\nname = \"c\"\n[context.regions]\nnotes = { kind = \"pinned\" }\n[stages.a]\ncontext = { }\n",
    )
    .unwrap();
    ctx.create_stage_override("a").unwrap();
    assert!(!ctx.effective_regions(Some("a")).inherited);
    assert_eq!(ctx.regions(Some("a"))[0].name, "notes");
}

// ─── check ───────────────────────────────────────────────────────────────────

#[test]
fn check_reports_parse_validate_and_lint_in_that_order() {
    let dir = std::env::temp_dir().join("lev-blueprint-edit-check");
    let _ = std::fs::create_dir_all(&dir);
    let parse = check("not = [toml", &dir);
    assert_eq!(parse.error_count(), 1);
    assert_eq!(parse.items[0].code, "parse");
    assert!(!parse.is_saveable());
    assert_eq!(parse.first().map(|p| p.severity), Some(Severity::Error));

    let mut doc = starter();
    doc.add_edge("work", "ghost").unwrap_err();
    // A dangling worker stage fails validation on the stage.
    doc.set_stage_mode("work", &StageModeView::FanOut).unwrap();
    doc.set_fan_out(
        "work",
        FanOutField::Worker(Some((WorkerKind::Stage, "nope".into()))),
    )
    .unwrap();
    let invalid = check(&doc.to_toml(), &dir);
    assert_eq!(invalid.items[0].code, "validate");
    assert_eq!(invalid.items[0].stage.as_deref(), Some("work"));
    assert_eq!(invalid.for_stage("work").len(), 1);
    // A validation error that names no stage.
    let mut noentry = starter();
    noentry
        .set_stage_mode("work", &StageModeView::Autonomous)
        .unwrap();
    let text = noentry
        .to_toml()
        .replace("entry_stage = \"work\"", "entry_stage = \"ghost\"");
    let graph = check(&text, &dir);
    assert_eq!(graph.items[0].code, "validate");
    assert_eq!(graph.items[0].stage, None);

    // A path into a stage that is not there is a transition error, filed
    // under the stage it leaves.
    let dangling = starter().to_toml().replace(
        "[stages.finish]",
        "[stages.work.transitions.ghost]\nhint = \"x\"\n\n[stages.finish]",
    );
    let problems = check(&dangling, &dir);
    assert_eq!(problems.items[0].code, "validate");
    assert_eq!(
        problems.items[0].stage.as_deref(),
        Some("work"),
        "{problems:?}"
    );
    // A command seed is worth a note, which sorts after warnings.
    let noted = starter().to_toml()
        + "\n[context.regions]\nenv = { kind = \"pinned\", seed = { command = \"echo hi\" } }\n";
    let problems = check(&noted, &dir);
    assert!(
        problems
            .items
            .iter()
            .any(|p| p.severity == Severity::Note && p.code == "command-seed"),
        "{problems:?}"
    );
    let last = problems.items.last().unwrap();
    assert_eq!(last.severity, Severity::Note);
    // The starter itself: no errors, warnings about what it leaves to defaults.
    let starter_problems = check(&starter().to_toml(), &dir);
    assert!(starter_problems.is_saveable(), "{starter_problems:?}");
    assert!(starter_problems.warning_count() > 0);
    assert!(
        starter_problems
            .items
            .iter()
            .all(|p| p.severity != Severity::Error)
    );
    assert!(
        starter_problems
            .items
            .iter()
            .any(|p| p.code == "stage-missing-model" && p.stage.as_deref() == Some("work"))
    );
    // Errors sort before warnings; a lint error blocks a save.
    let mut lint_err = starter();
    lint_err
        .set_tools("work", &["no_such_tool".into()])
        .unwrap();
    let problems = check(&lint_err.to_toml(), &dir);
    assert!(!problems.is_saveable());
    assert_eq!(problems.items[0].severity, Severity::Error);
    assert_eq!(problems.items[0].code, "unknown-tool");
    assert!(problems.items[0].fix.is_some() || problems.items[0].message.contains("no_such_tool"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ─── templates ───────────────────────────────────────────────────────────────

#[test]
fn the_starter_and_the_clone() {
    let text = templates::empty_blueprint("demo").unwrap();
    let doc = ManifestDoc::parse(&text).unwrap();
    assert_eq!(doc.agent().name, "demo");
    assert_eq!(doc.stage_names(), ["work", "finish"]);
    assert_eq!(
        doc.edge("work", "finish").unwrap().hint.as_deref(),
        Some("The work is done and verified")
    );
    assert!(doc.stage("finish").unwrap().is_terminal);
    runtime_ok(&doc);
    assert_eq!(
        templates::empty_blueprint("no way"),
        Err(EditError::BadName("no way".into()))
    );
    let clone = templates::clone_of(bundled_text("coder"), "my-coder").unwrap();
    let cloned = ManifestDoc::parse(&clone).unwrap();
    assert_eq!(cloned.agent().name, "my-coder");
    assert_eq!(cloned.stage_names(), coder().stage_names());
    assert!(matches!(
        templates::clone_of("nope = [", "x"),
        Err(EditError::Toml(_))
    ));
    assert_eq!(
        templates::clone_of(bundled_text("coder"), "no way"),
        Err(EditError::BadName("no way".into()))
    );
}

// ─── layout store ────────────────────────────────────────────────────────────

#[test]
fn the_layout_store_remembers_per_agent_and_survives_a_reload() {
    let dir = std::env::temp_dir().join("lev-blueprint-edit-layouts");
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("nested").join("graph-layouts.json");
    let mut store = LayoutStore::open(path.clone());
    assert_eq!(store.path(), Some(path.as_path()));
    assert!(store.positions("coder").is_none());
    let mut positions = Positions::new();
    positions.insert("plan".into(), (10.0, 4.5));
    store.set("coder", positions.clone());
    store.copy("coder", "my-coder");
    store.copy("ghost", "nothing");
    store.set("empty", Positions::new());
    store.save().unwrap();
    let again = LayoutStore::open(path.clone());
    assert_eq!(again.positions("coder"), Some(&positions));
    assert_eq!(again.positions("my-coder"), Some(&positions));
    assert!(again.positions("nothing").is_none());
    assert!(again.positions("empty").is_none());
    let mut again = again;
    again.forget("coder");
    again.set("my-coder", Positions::new());
    assert!(again.positions("coder").is_none());
    again.save().unwrap();
    let reloaded = LayoutStore::open(path);
    assert!(reloaded.positions("coder").is_none());
    assert!(
        reloaded.positions("my-coder").is_none(),
        "an empty arrangement is forgotten"
    );
    // Garbage on disk reads as empty; a memory store never writes.
    std::fs::write(dir.join("bad.json"), "{{{").unwrap();
    assert!(
        LayoutStore::open(dir.join("bad.json"))
            .positions("x")
            .is_none()
    );
    // A path under a file cannot be created.
    std::fs::write(dir.join("blocker"), "x").unwrap();
    let mut blocked = LayoutStore::open(dir.join("blocker").join("sub").join("l.json"));
    blocked.set("a", positions.clone());
    assert!(blocked.save().is_err());
    let mut mem = LayoutStore::in_memory();
    mem.set("a", positions);
    mem.save().unwrap();
    assert_eq!(mem.path(), None);
    assert!(LayoutStore::default_path().is_some_and(|p| p.ends_with("dash/graph-layouts.json")));
    let _ = std::fs::remove_dir_all(&dir);
}

// ─── catalog ─────────────────────────────────────────────────────────────────

#[test]
fn the_catalog_lists_every_source_and_writes_deletes_and_resets() {
    use catalog::{Source, discover};
    let root =
        std::env::temp_dir().join(format!("lev-blueprint-edit-catalog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let agents = root.join("agents");
    let configured = root.join("configured");
    let cwd = root.join("cwd");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::create_dir_all(configured.join("mine")).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    // An installed agent of our own, an installed bundled one (edited), a
    // configured one, a local one.
    catalog::write_agent(&agents, "own", &templates::empty_blueprint("own").unwrap()).unwrap();
    catalog::reset_bundled(&agents, "coder").unwrap();
    let coder_manifest = agents.join("coder").join("agent.leviath");
    let edited = std::fs::read_to_string(&coder_manifest)
        .unwrap()
        .replace("entry_stage = \"discover\"", "entry_stage = \"plan\"");
    std::fs::write(&coder_manifest, edited).unwrap();
    std::fs::write(
        configured.join("mine").join("agent.leviath"),
        templates::empty_blueprint("mine").unwrap(),
    )
    .unwrap();
    std::fs::write(
        cwd.join("agent.leviath"),
        templates::empty_blueprint("here").unwrap(),
    )
    .unwrap();
    // A directory without a readable manifest is skipped by the list.
    std::fs::create_dir_all(agents.join("junk")).unwrap();
    std::fs::write(agents.join("junk").join("agent.leviath"), "not = [").unwrap();
    let config = crate::config::Config {
        agent_paths: vec![configured.clone()],
        ..Default::default()
    };

    let entries = discover(&agents, &cwd, &config);
    let by_name = |n: &str| {
        entries
            .iter()
            .find(|e| e.name == n)
            .unwrap_or_else(|| panic!("{n} in {entries:?}"))
    };
    let own = by_name("own");
    assert_eq!(own.source, Source::Installed);
    assert!(own.deletable());
    assert!(!own.bundled);
    assert_eq!(own.stages, ["work", "finish"]);
    assert!(own.manifest.as_deref().unwrap().contains("name = \"own\""));
    assert_eq!(own.dir.as_deref(), Some(agents.join("own").as_path()));
    let coder = by_name("coder");
    assert_eq!(coder.source, Source::Installed);
    assert!(coder.bundled && coder.differs_from_bundled);
    let mine = by_name("mine");
    assert_eq!(mine.source, Source::Configured);
    assert!(!mine.deletable());
    let here = by_name("here");
    assert_eq!(here.source, Source::Local);
    assert_eq!(here.dir.as_deref(), Some(cwd.as_path()));
    let reviewer = by_name("reviewer");
    assert_eq!(reviewer.source, Source::Bundled);
    assert!(reviewer.bundled && !reviewer.differs_from_bundled);
    assert!(reviewer.dir.is_none());
    assert!(reviewer.manifest.is_some());
    assert!(reviewer.stages.contains(&"split_review".to_string()));
    assert!(reviewer.description.starts_with("Code review agent"));
    assert!(entries.windows(2).all(|w| w[0].name <= w[1].name), "sorted");
    assert!(!entries.iter().any(|e| e.name == "junk"));

    // Reset puts the embedded copy back; the entry no longer differs.
    catalog::reset_bundled(&agents, "coder").unwrap();
    assert!(
        !discover(&agents, &cwd, &config)
            .iter()
            .find(|e| e.name == "coder")
            .unwrap()
            .differs_from_bundled
    );
    assert!(
        catalog::reset_bundled(&agents, "own")
            .unwrap_err()
            .contains("not a bundled agent")
    );
    // Extras of a bundled agent land next to a cloned manifest.
    let researcher = catalog::bundled("researcher").unwrap();
    catalog::write_agent(
        &agents,
        "my-researcher",
        &templates::clone_of(catalog::bundled_manifest(researcher), "my-researcher").unwrap(),
    )
    .unwrap();
    catalog::copy_bundled_extras(&agents, "my-researcher", researcher).unwrap();
    assert!(
        agents
            .join("my-researcher")
            .join("tools")
            .join("web_search.rhai")
            .exists()
    );
    // A script whose path is already a directory cannot be written.
    let clash = agents.join("clash");
    std::fs::create_dir_all(clash.join("tools").join("web_search.rhai")).unwrap();
    assert!(catalog::copy_bundled_extras(&agents, "clash", researcher).is_err());
    assert!(catalog::bundled("nope").is_none());
    // Writes into a directory that is a file fail, and so does a reset there.
    let blocked = root.join("blocked");
    std::fs::write(&blocked, "x").unwrap();
    assert!(catalog::write_agent(&blocked, "own", "x").is_err());
    assert!(catalog::copy_bundled_extras(&blocked, "own", researcher).is_err());
    assert!(catalog::reset_bundled(&blocked, "coder").is_err());
    // A manifest that cannot be written where its directory could.
    let dir_as_file = agents.join("taken");
    std::fs::create_dir_all(&dir_as_file).unwrap();
    std::fs::create_dir_all(dir_as_file.join("agent.leviath")).unwrap();
    assert!(catalog::write_agent(&agents, "taken", "x").is_err());
    // Delete.
    catalog::delete_agent(&agents, "own").unwrap();
    assert!(!agents.join("own").exists());
    assert!(catalog::delete_agent(&agents, "own").is_err());
    let _ = std::fs::remove_dir_all(&root);
}

// ─── order ───────────────────────────────────────────────────────────────────

#[test]
fn the_written_order_walks_arrays_of_tables_and_renumbers_them() {
    use order::{Seg, renumber, written_order};
    let mut doc: toml_edit::DocumentMut = "[a]\nv = 1\n[[b]]\nx = 1\n[[b]]\nx = 2\n[b.c]\n[d]\n"
        .parse()
        .unwrap();
    let order = written_order(&doc);
    assert_eq!(
        order,
        vec![
            vec![Seg::Key("a".into())],
            vec![Seg::Key("b".into()), Seg::Index(0)],
            vec![Seg::Key("b".into()), Seg::Index(1)],
            vec![Seg::Key("b".into()), Seg::Index(1), Seg::Key("c".into())],
            vec![Seg::Key("d".into())],
        ]
    );
    // Reversed and renumbered: the file follows.
    let mut reversed = order.clone();
    reversed.reverse();
    // A path that leads nowhere is skipped, an index under a plain table too.
    reversed.push(vec![Seg::Key("ghost".into())]);
    reversed.push(vec![Seg::Key("a".into()), Seg::Key("ghost".into())]);
    reversed.push(vec![
        Seg::Key("a".into()),
        Seg::Key("v".into()),
        Seg::Key("w".into()),
    ]);
    reversed.push(vec![Seg::Key("a".into()), Seg::Index(0)]);
    reversed.push(vec![Seg::Key("b".into()), Seg::Index(9)]);
    reversed.push(vec![Seg::Key("b".into()), Seg::Index(1), Seg::Index(0)]);
    reversed.push(vec![
        Seg::Key("b".into()),
        Seg::Index(1),
        Seg::Key("ghost".into()),
    ]);
    renumber(&mut doc, &reversed);
    let text = doc.to_string();
    assert!(
        text.find("[d]").unwrap() < text.find("[a]").unwrap(),
        "{text}"
    );
    assert_eq!(written_order(&doc)[0], vec![Seg::Key("d".into())]);
}

#[test]
fn renaming_an_agent_moves_its_directory_and_the_name_in_its_manifest() {
    let root =
        std::env::temp_dir().join(format!("lev-blueprint-edit-rename-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let agents = root.join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    let text = format!("# kept\n{}", templates::empty_blueprint("own").unwrap());
    catalog::write_agent(&agents, "own", &text).unwrap();
    catalog::write_agent(
        &agents,
        "taken",
        &templates::empty_blueprint("taken").unwrap(),
    )
    .unwrap();
    // Refusals leave everything alone.
    assert!(catalog::rename_agent(&agents, "own", "bad name").is_err());
    assert!(
        catalog::rename_agent(&agents, "own", "taken")
            .unwrap_err()
            .contains("already exists")
    );
    assert!(
        catalog::rename_agent(&agents, "ghost", "other")
            .unwrap_err()
            .contains("Could not read")
    );
    std::fs::create_dir_all(agents.join("junk")).unwrap();
    std::fs::write(agents.join("junk").join("agent.leviath"), "= not toml").unwrap();
    assert!(catalog::rename_agent(&agents, "junk", "other").is_err());
    assert!(agents.join("own").exists());
    // The same name is nothing to do.
    assert_eq!(
        catalog::rename_agent(&agents, "own", "own").unwrap(),
        agents.join("own")
    );
    // A move the disk refuses: the manifest already carries the new name
    // under the old directory, and says so.
    let err = catalog::rename_agent_with(&agents, "own", "mine", &mut |_, _| {
        Err(std::io::Error::other("disk says no"))
    })
    .unwrap_err();
    assert!(err.contains("disk says no"), "{err}");
    assert!(agents.join("own").exists());
    let stuck = std::fs::read_to_string(agents.join("own").join("agent.leviath")).unwrap();
    assert!(stuck.contains("name = \"mine\""));
    std::fs::write(agents.join("own").join("agent.leviath"), &text).unwrap();
    // A manifest that cannot be written: said, nothing moved.
    let manifest = agents.join("own").join("agent.leviath");
    let writable = std::fs::metadata(&manifest).unwrap().permissions();
    let mut locked = writable.clone();
    locked.set_readonly(true);
    std::fs::set_permissions(&manifest, locked).unwrap();
    let err = catalog::rename_agent(&agents, "own", "mine").unwrap_err();
    assert!(err.contains("Could not write"), "{err}");
    assert!(agents.join("own").exists());
    std::fs::set_permissions(&manifest, writable).unwrap();
    // The rename: the directory moves, the name changes, the comment stays.
    let new = catalog::rename_agent(&agents, "own", "mine").unwrap();
    assert_eq!(new, agents.join("mine"));
    assert!(!agents.join("own").exists());
    let moved = std::fs::read_to_string(new.join("agent.leviath")).unwrap();
    assert!(moved.starts_with("# kept\n"), "{moved}");
    assert!(moved.contains("name = \"mine\""), "{moved}");
    let _ = std::fs::remove_dir_all(&root);
}
