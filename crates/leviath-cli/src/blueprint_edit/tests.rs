//! The editing model, exercised on the bundled agents and on the starter.

use super::check::{Severity, check};
use super::*;
use crate::bundled::BUNDLED_AGENTS;
use leviath_runtime::spec::graph::RunGraph;
use leviath_runtime::spec::issues::SpecPath;

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

/// A small blueprint written by hand around `body` (stages, edges and the
/// rest of `[graph]`), with one shared region.
fn small(body: &str) -> ManifestDoc {
    let text = format!(
        "[blueprint]\nname = \"t\"\nversion = \"1\"\n\n[graph]\n\
         layout = {{ total_budget_tokens = 0, regions = [{{ name = \"r\", kind = \"pinned\", budget = \"5%\" }}] }}\n\n{body}"
    );
    ManifestDoc::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"))
}

/// The file re-read by the runtime after an edit: every mutator must leave
/// something it reads and whose graph holds together.
fn runtime_ok(doc: &ManifestDoc) -> RunGraph {
    let text = doc.to_toml();
    let file = doc
        .file()
        .unwrap_or_else(|e| panic!("the runtime rejected the edited file: {e}\n{text}"));
    let graph = file.run_graph();
    if let Err(issues) = graph.validate(&SpecPath::root().field("graph")) {
        panic!("the edited graph does not hold together: {issues}\n{text}");
    }
    graph
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
    assert_eq!(EditError::NoStages.to_string(), "the file has no stages");
    assert_eq!(
        EditError::NoBlueprint.to_string(),
        "the file has no [blueprint] table"
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
fn every_bundled_blueprint_round_trips_byte_for_byte() {
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
        let graph = runtime_ok(&doc);
        assert_eq!(graph.title.as_deref(), Some(agent.name));
        // The views see every stage the runtime sees, in its order.
        let runtime: Vec<String> = graph.stages.iter().map(|s| s.name.to_string()).collect();
        assert_eq!(doc.stage_names(), runtime, "{}", agent.name);
        assert_eq!(doc.edges().len(), graph.edges.len(), "{}", agent.name);
    }
}

#[test]
fn parse_refuses_what_the_editor_cannot_stand_on() {
    assert!(matches!(
        ManifestDoc::parse("not = [toml"),
        Err(EditError::Toml(_))
    ));
    let stages = "[[graph.stages]]\nname = \"a\"\n";
    assert_eq!(
        ManifestDoc::parse(stages).unwrap_err(),
        EditError::NoBlueprint
    );
    assert_eq!(
        ManifestDoc::parse(&format!("blueprint = 3\n{stages}")).unwrap_err(),
        EditError::NoBlueprint
    );
    for no_stages in [
        "[blueprint]\nname = \"x\"\n",
        "[blueprint]\nname = \"x\"\n[graph]\nstages = 3\n",
        "[blueprint]\nname = \"x\"\n[graph]\nstages = []\n",
        "graph = 3\n[blueprint]\nname = \"x\"\n",
    ] {
        assert_eq!(
            ManifestDoc::parse(no_stages).unwrap_err(),
            EditError::NoStages,
            "{no_stages}"
        );
    }
    let err = ManifestDoc::parse("not = [toml").unwrap_err().to_string();
    assert!(err.starts_with("not valid TOML: "), "{err}");
}

/// The version the bundled table records for `name`, so a test can assert the
/// view reports it without pinning the number.
fn bundled_version(name: &str) -> &'static str {
    crate::bundled::BUNDLED_AGENTS
        .iter()
        .find(|a| a.name == name)
        .expect("a bundled agent by that name")
        .version
}

#[test]
fn the_views_read_the_coder_the_way_the_lair_does() {
    let doc = coder();
    let agent = doc.agent();
    assert_eq!(agent.name, "coder");
    // Read from the bundled table rather than pinned here: the version moves
    // whenever the blueprint's contents do, and a literal turns every such
    // change into a test edit.
    assert_eq!(agent.version, bundled_version("coder"));
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
    // A model the blueprint names without pinning a route reads back as the
    // bare name. It has to survive the view: this stage lists several such
    // models, and reading only the route-pinned ones showed just the local one.
    assert_eq!(discover.models[0], "claude-sonnet-5-5");
    assert!(discover.models.len() > 1, "{:?}", discover.models);
    assert!(
        discover.models.iter().any(|m| m.starts_with("ollama/")),
        "a local model still pins its route, got {:?}",
        discover.models
    );
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
    assert!(plan.transition_prompt.contains("approved the plan"));

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
    assert!(!review.rules.present);
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
    assert_eq!(task.seed, "task", "the task input fills it");
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
    let conversation = shared
        .regions
        .iter()
        .find(|r| r.name == "conversation")
        .unwrap();
    assert_eq!(conversation.kind, "sliding_window");
    assert_eq!(conversation.max_items, Some(40));
    assert_eq!(
        (conversation.strategy.as_str(), conversation.overflow),
        ("bulk", Some(20))
    );
    let decisions = doc.region(None, "decisions").unwrap();
    assert_eq!(
        (decisions.strategy.as_str(), decisions.overflow),
        ("", None)
    );
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
    assert!(models.contains(&"claude-opus-5-5".to_string()));
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
    assert_eq!(split.fan_out.max_workers, Some(30));
    let worker = doc.stage("review_worker").unwrap();
    assert!(worker.is_terminal);
}

#[test]
fn odd_shapes_read_gently() {
    let doc = small(
        r#"[[graph.stages]]
name = "a"
model = { models = ["gpt-4", 3, { provider = "x" }, { model = "y" }, { provider = "", model = "z" }, { provider = "p", model = "m" }] }
mode = "weird"

[[graph.stages]]
name = "b"
model = "not a table"
mode = 3
tool_routing = { default_region = "r", tool_regions = { bash = 3, read_file = "r" } }

[[graph.stages]]
name = "c"
mode = { fan_out = { worker = { blueprint = { name = "w", digest = "sha256:ab" } } } }

[[graph.stages]]
name = "d"
mode = { fan_out = { worker = { blueprint_file = "/agents/w" } } }

[[graph.stages]]
name = "e"
mode = { fan_out = { worker = { query = "reads logs" } } }

[[graph.stages]]
name = "f"
mode = { fan_out = { worker = { blueprint = { digest = "x" } } } }

[[graph.stages]]
name = "g"
mode = { fan_out = { worker = {} } }

[[graph.stages]]
mode = "autonomous"

[[graph.edges]]
name = "b"
from = "a"
to = "b"
when = "someday"

[[graph.edges]]
from = "a"
to = "g"

[[graph.edges]]
name = "z"
to = "c"

[[graph.edges]]
name = "c"
from = "a"
to = "c"

[[graph.edges]]
name = "x"
from = "a"

[[graph.edges]]
name = "d"
from = "a"
to = "d"
when = "always"

[[graph.edges]]
name = "e"
from = "a"
to = "e"
when = "always"
hint = "go"

[[graph.edges]]
name = "f"
from = "a"
to = "f"
carry = 3

[[graph.edges]]
name = "g"
from = "c"
to = "d"
carry = {}

[[graph.edges]]
name = "h"
from = "c"
to = "e"
carry = { compact = { prompt = "Keep the gist" } }
"#,
    );
    let a = doc.stage("a").unwrap();
    // A bare name or `{ model = "y" }` is the open-route form; an empty
    // provider says the same; an entry naming no model is dropped.
    assert_eq!(a.models, ["gpt-4", "y", "z", "p/m"]);
    assert_eq!(a.mode, StageModeView::Other("weird".into()));
    assert_eq!(a.mode.as_str(), "weird");
    assert_eq!(a.mode.label(), "weird");
    let b = doc.stage("b").unwrap();
    assert!(b.models.is_empty());
    assert_eq!(b.mode, StageModeView::Other(String::new()));
    assert_eq!(
        doc.tool_routing("b").overrides,
        [("read_file".to_string(), "r".to_string())]
    );
    let worker = |s: &str| doc.stage(s).unwrap().fan_out.worker;
    assert_eq!(
        worker("c"),
        Some((WorkerKind::Agent, "w@sha256:ab".to_string()))
    );
    assert_eq!(
        worker("d"),
        Some((WorkerKind::Agent, "/agents/w".to_string()))
    );
    assert_eq!(
        worker("e"),
        Some((WorkerKind::Query, "reads logs".to_string()))
    );
    assert_eq!(worker("f"), None, "a blueprint with no name");
    assert_eq!(worker("g"), None, "a worker that names nothing");
    // The stage with no name is no stage the editor can name.
    assert_eq!(doc.stage_names(), ["a", "b", "c", "d", "e", "f", "g"]);
    // A rename steps over edges with no name or no stage to leave.
    let mut renamed = doc.clone();
    renamed.rename_stage("g", "h").unwrap();
    assert!(renamed.edge("a", "h").is_some());
    // The unknown `when` and the edges with no `to` or no `from` are left
    // out.
    let edges = doc.edges();
    let tos: Vec<&str> = edges.iter().map(|e| e.to.as_str()).collect();
    assert_eq!(tos, ["g", "c", "d", "e", "f", "d", "e"]);
    assert_eq!(doc.edge("a", "c").unwrap().kind, EdgeKind::Always);
    assert_eq!(doc.edge("a", "d").unwrap().kind, EdgeKind::Always);
    assert_eq!(doc.edge("a", "e").unwrap().kind, EdgeKind::Hint);
    assert_eq!(
        doc.edge("a", "f").unwrap().transform,
        TransformKind::Other(String::new())
    );
    assert_eq!(
        doc.edge("c", "d").unwrap().transform,
        TransformKind::Other(String::new())
    );
    let compact = doc.edge("c", "e").unwrap();
    assert_eq!(compact.transform, TransformKind::Compact);
    assert_eq!(compact.rules.compact_prompt, "Keep the gist");
    assert!(!compact.rules.present);
    assert!(doc.edge("a", "b").is_none());
    assert_eq!(doc.agent().default_model, None);
    assert_eq!(doc.agent().entry_stage, None);
    assert!(doc.stage("ghost").is_none());
    assert!(doc.regions(Some("ghost")).is_empty());
    assert!(doc.region(Some("ghost"), "x").is_none());
    assert!(doc.regions(Some("b")).is_empty(), "no layout of its own");
    assert!(doc.file().is_err(), "the runtime refuses the odd mode");

    // Regions read gently too: a missing name is no region, a kind that is
    // not there reads empty, an eviction by name has no count, a budget that
    // is not a percentage has none, a fixed budget has no clamps.
    let regions = small(
        "[[graph.inputs]]\nname = \"broken\"\nbinds = 3\n\n\
         [[graph.inputs]]\nbinds = [{ region = \"r\" }]\n\n\
         [[graph.stages]]\nname = \"a\"\n\
         layout = { total_budget_tokens = 0, regions = [\
         { kind = \"pinned\", budget = \"1%\" }, \
         { name = \"k\", budget = \"lots\" }, \
         { name = \"w\", kind = { kind = \"sliding_window\", max_items = 3, eviction = \"per_item\" }, budget = 400 }, \
         { name = \"o\", kind = { kind = \"sliding_window\", max_items = 3, eviction = {} }, budget = { percent = \"7%\", min = 10, max = 90 } }, \
         { name = \"n\", kind = { kind = \"sliding_window\", max_items = 3, eviction = { bulk = \"x\" } }, budget = { min = 3 } }] }\n",
    );
    let names: Vec<String> = regions
        .regions(Some("a"))
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(names, ["k", "w", "o", "n"]);
    let k = regions.region(Some("a"), "k").unwrap();
    assert_eq!((k.kind.as_str(), k.budget_percent), ("", None));
    let w = regions.region(Some("a"), "w").unwrap();
    assert_eq!((w.strategy.as_str(), w.overflow), ("per_item", None));
    assert_eq!((w.budget_percent, w.max_tokens), (None, None));
    let o = regions.region(Some("a"), "o").unwrap();
    assert_eq!((o.strategy.as_str(), o.overflow), ("", None));
    assert_eq!(
        (o.budget_percent, o.min_tokens, o.max_tokens),
        (Some(7.0), Some(10), Some(90))
    );
    let n = regions.region(Some("a"), "n").unwrap();
    assert_eq!((n.strategy.as_str(), n.overflow), ("bulk", None));
    assert_eq!((n.budget_percent, n.min_tokens), (None, Some(3)));
    assert_eq!(regions.region(None, "r").unwrap().seed, "");
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
    let other = TransformKind::parse("teleport");
    assert_eq!(other, TransformKind::Other("teleport".into()));
    assert_eq!(other.as_str(), "teleport");
    assert_eq!(other.label(), "teleport");
    assert_eq!(WorkerKind::Agent.key(), "blueprint");
    assert_eq!(WorkerKind::Stage.key(), "stage");
    assert_eq!(WorkerKind::Query.key(), "query");
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
    let head = text.split("[graph]").next().unwrap();
    assert!(
        !head.contains("description ="),
        "empty deletes the key: {head}"
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
    // The file shows it after work and work's edge, before finish, and the
    // runtime reads it there.
    let text = doc.to_toml();
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle}\n{text}"))
    };
    assert!(at("name = \"work\"") < at("from = \"work\""), "{text}");
    assert!(at("from = \"work\"") < at("name = \"review\""), "{text}");
    assert!(
        at("name = \"review\"") < at("name = \"finish\"\nmode"),
        "{text}"
    );
    let graph = runtime_ok(&doc);
    assert_eq!(graph.stages[1].name.as_str(), "review");
    doc.add_stage("last", None).unwrap();
    assert_eq!(doc.stage_names(), ["work", "review", "finish", "last"]);
    assert!(doc.to_toml().trim_end().ends_with("max_iterations = 20"));
    // A path out of the new stage lands right after it, and the stage stops
    // being terminal; deleting it makes the stage terminal again.
    doc.add_edge("review", "finish").unwrap();
    let text = doc.to_toml();
    let review_at = text.find("name = \"review\"").unwrap();
    let edge_at = text.find("from = \"review\"").unwrap();
    let finish_at = text.find("name = \"finish\"\nmode").unwrap();
    assert!(review_at < edge_at && edge_at < finish_at, "{text}");
    assert!(!doc.stage("review").unwrap().is_terminal);
    doc.delete_edge("review", "finish").unwrap();
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
    // Re-read: the order survives a round trip.
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
    doc.rename_stage("deep_review", "merge").unwrap();
    assert_eq!(
        doc.stage("split_review")
            .unwrap()
            .fan_out
            .merge_stage
            .as_deref(),
        Some("merge")
    );
    doc.rename_stage("discover", "orient").unwrap();
    assert_eq!(doc.agent().entry_stage.as_deref(), Some("orient"));
    // Edges into and out of the renamed stage follow it, and an edge named
    // after it takes the new name; the order does not move.
    doc.rename_stage("report", "wrap_up").unwrap();
    assert!(doc.edge("merge", "wrap_up").is_some());
    assert!(doc.edge("merge", "report").is_none());
    assert!(doc.edge("wrap_up", "summary").is_some());
    assert!(
        doc.to_toml()
            .contains("name = \"wrap_up\"\nfrom = \"merge\"")
    );
    assert_eq!(doc.stage_names()[0], "orient");
    // The comment above the renamed stage's header is still above it.
    let text = doc.to_toml();
    let comment = text.find("# ─── Stage 1: Discover").expect("comment kept");
    let header = text.find("name = \"orient\"").unwrap();
    assert!(comment < header, "{text}");
    assert!(
        text.get(comment..header).unwrap().matches('\n').count() <= 6,
        "{text}"
    );
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
fn renaming_keeps_edge_names_unique_and_follows_inputs() {
    let mut doc = small(
        r#"[[graph.inputs]]
name = "m"
type = "model"
binds = [{ stage_model = "b" }]

[[graph.inputs]]
name = "format"
type = "text"
binds = ["output_format", { region = "r" }]

[[graph.inputs]]
name = "unbound"
type = "text"

[[graph.stages]]
name = "a"

[[graph.stages]]
name = "b"

[[graph.stages]]
name = "c"

[[graph.edges]]
name = "b"
from = "a"
to = "b"

[[graph.edges]]
name = "x"
from = "a"
to = "c"

[[graph.edges]]
name = "b"
from = "c"
to = "b"
"#,
    );
    doc.rename_stage("b", "x").unwrap();
    let text = doc.to_toml();
    // a already leaves by an edge called `x`, so a's edge into the renamed
    // stage keeps its name; c's takes the new one.
    assert!(
        text.contains("name = \"b\"\nfrom = \"a\"\nto = \"x\""),
        "{text}"
    );
    assert!(
        text.contains("name = \"x\"\nfrom = \"c\"\nto = \"x\""),
        "{text}"
    );
    assert!(text.contains("{ stage_model = \"x\" }"), "{text}");
    runtime_ok(&doc);
}

#[test]
fn deleting_a_stage_takes_its_paths_and_repoints_the_entry() {
    let mut doc = starter();
    doc.add_stage("review", Some("work")).unwrap();
    doc.add_edge("review", "finish").unwrap();
    doc.delete_stage("work").unwrap();
    assert_eq!(doc.stage_names(), ["review", "finish"]);
    assert_eq!(doc.agent().entry_stage.as_deref(), Some("review"));
    assert!(
        doc.edges()
            .iter()
            .all(|e| e.to != "work" && e.from != "work")
    );
    doc.delete_stage("finish").unwrap();
    assert!(doc.edges().is_empty(), "the path into finish went with it");
    assert_eq!(doc.delete_stage("review"), Err(EditError::LastStage));
    assert_eq!(
        doc.delete_stage("ghost"),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    assert_eq!(doc.stage_names(), ["review"]);
    // A graph with no edges at all, and an entry left alone.
    let mut bare = small("[[graph.stages]]\nname = \"a\"\n[[graph.stages]]\nname = \"b\"\n");
    bare.delete_stage("b").unwrap();
    assert_eq!(bare.stage_names(), ["a"]);
    assert_eq!(bare.agent().entry_stage, None);
    // Renaming in a graph with no edges and no inputs touches only the stage.
    bare.rename_stage("a", "z").unwrap();
    assert_eq!(bare.stage_names(), ["z"]);
}

#[test]
fn stage_fields_write_and_delete_the_way_the_lair_does() {
    let mut doc = starter();
    // A fan-out setting needs a fan-out stage.
    assert!(matches!(
        doc.set_fan_out("work", FanOutField::MaxWorkers(Some(2))),
        Err(EditError::OutOfRange(_))
    ));
    // A fan-out reads from the start: its worker is the first other stage,
    // or the stage itself when it is the only one.
    doc.set_stage_mode("work", &StageModeView::FanOut).unwrap();
    assert!(
        doc.to_toml()
            .contains("mode = { fan_out = { worker = { stage = \"finish\" } } }"),
        "{}",
        doc.to_toml()
    );
    assert!(doc.file().is_ok());
    let mut alone = small("[[graph.stages]]\nname = \"a\"\n");
    alone.set_stage_mode("a", &StageModeView::FanOut).unwrap();
    assert_eq!(
        alone.stage("a").unwrap().fan_out.worker,
        Some((WorkerKind::Stage, "a".to_string()))
    );
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
    // Picking fan-out again keeps the settings.
    doc.set_stage_mode("work", &StageModeView::FanOut).unwrap();
    assert_eq!(doc.stage("work").unwrap().fan_out.max_workers, Some(4));
    // A blueprint worker by name, pinned, or by directory; a query.
    for (kind, value, written) in [
        (
            WorkerKind::Agent,
            "researcher",
            r#"worker = { blueprint = { name = "researcher" } }"#,
        ),
        (
            WorkerKind::Agent,
            "researcher@sha256:ab",
            r#"worker = { blueprint = { name = "researcher", digest = "sha256:ab" } }"#,
        ),
        (
            WorkerKind::Agent,
            "/agents/r",
            r#"worker = { blueprint_file = "/agents/r" }"#,
        ),
        (
            WorkerKind::Query,
            "reads logs",
            r#"worker = { query = "reads logs" }"#,
        ),
    ] {
        doc.set_fan_out("work", FanOutField::Worker(Some((kind, value.into()))))
            .unwrap();
        assert!(doc.to_toml().contains(written), "{}", doc.to_toml());
        assert_eq!(
            doc.stage("work").unwrap().fan_out.worker,
            Some((kind, value.to_string()))
        );
    }
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
    // Leaving fan-out drops the fan-out settings; interaction points start
    // empty; any other mode is written by name.
    doc.set_fan_out("work", FanOutField::MaxWorkers(Some(2)))
        .unwrap();
    doc.set_stage_mode("work", &StageModeView::Autonomous)
        .unwrap();
    assert!(!doc.to_toml().contains("max_workers"));
    doc.set_stage_mode("work", &StageModeView::InteractivePoints)
        .unwrap();
    assert!(doc.to_toml().contains("mode = { interactive_points = [] }"));
    assert_eq!(
        doc.stage("work").unwrap().mode,
        StageModeView::InteractivePoints
    );
    runtime_ok(&doc);
    doc.set_stage_mode("work", &StageModeView::Other("odd".into()))
        .unwrap();
    assert!(doc.to_toml().contains("mode = \"odd\""));
    doc.set_stage_mode("work", &StageModeView::Autonomous)
        .unwrap();

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

    doc.set_tools("work", &["read_file".into(), "@builtin".into()])
        .unwrap();
    assert_eq!(doc.stage("work").unwrap().tools, ["read_file", "@builtin"]);
    runtime_ok(&doc);
    doc.set_tools("work", &[]).unwrap();
    assert!(!doc.to_toml().contains("tools ="));
    doc.set_connectors("work", &["github".into()]).unwrap();
    assert_eq!(doc.stage("work").unwrap().connectors, ["github"]);
    assert!(doc.to_toml().contains("connectors = [\"github\"]"));
    doc.set_connectors("work", &[]).unwrap();
    assert!(!doc.to_toml().contains("connectors"));
    assert!(doc.set_connectors("ghost", &[]).is_err());

    // Models: a slash pins the route; a slashless entry names a model and
    // leaves the route open. Both survive a round trip; empty deletes.
    doc.set_models("work", &["anthropic/claude".into(), "gpt-4".into()])
        .unwrap();
    let text = doc.to_toml();
    assert!(
        text.contains(
            r#"model = { models = [{ provider = "anthropic", model = "claude" }, { model = "gpt-4" }] }"#
        ),
        "{text}"
    );
    assert_eq!(
        doc.stage("work").unwrap().models,
        ["anthropic/claude", "gpt-4"],
        "what was written reads back unchanged"
    );
    runtime_ok(&doc);
    let mut kept = small(
        "[[graph.stages]]\nname = \"a\"\nmodel = { allow_user_default = false, models = [{ provider = \"x\", model = \"y\" }] }\n",
    );
    kept.set_models("a", &["openai/o".into()]).unwrap();
    assert!(kept.to_toml().contains("allow_user_default = false"));
    assert_eq!(kept.stage("a").unwrap().models, ["openai/o"]);
    kept.set_models("a", &[]).unwrap();
    assert!(!kept.to_toml().contains("model"));

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
    assert!(doc.to_toml().contains("name = \"work\"\nfrom = \"finish\""));
    // Again: left alone. To itself: a self-loop.
    doc.set_edge_hint("finish", "work", "Go round again")
        .unwrap();
    doc.add_edge("finish", "work").unwrap();
    assert_eq!(
        doc.edge("finish", "work").unwrap().hint.as_deref(),
        Some("Go round again")
    );
    doc.add_edge("work", "work").unwrap();
    assert!(doc.edge("work", "work").is_some());
    // The new edge lands after the stage's other edge.
    let text = doc.to_toml();
    let first = text.find("name = \"finish\"\nfrom = \"work\"").unwrap();
    let second = text.find("name = \"work\"\nfrom = \"work\"").unwrap();
    let next_stage = text.find("name = \"finish\"\nmode").unwrap();
    assert!(first < second && second < next_stage, "{text}");
    assert_eq!(
        doc.add_edge("work", "ghost"),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    assert_eq!(
        doc.add_edge("ghost", "work"),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    runtime_ok(&doc);

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
            assert_eq!(edge.hint, None, "any other kind drops the hint");
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
    assert_eq!(
        doc.set_edge_kind("work", "ghost", EdgeKind::Always),
        Err(EditError::NoSuchEdge("work".into(), "ghost".into()))
    );
    assert_eq!(
        doc.set_edge_hint("ghost", "work", "x"),
        Err(EditError::NoSuchEdge("ghost".into(), "work".into()))
    );
    assert_eq!(
        doc.set_edge_gate("work", "ghost", true),
        Err(EditError::NoSuchEdge("work".into(), "ghost".into()))
    );
    runtime_ok(&doc);
}

#[test]
fn a_new_path_gets_a_free_name_and_the_first_edge_list_is_made() {
    // No edges yet: the list is made in the stages' shape, and the first
    // edge of a stage lands after it, before the next stage.
    let mut doc = small(
        "[[graph.stages]]\nname = \"a\"\n\n[[graph.stages]]\nname = \"b\"\n\n\
         [[graph.edges]]\nname = \"b\"\nfrom = \"b\"\nto = \"a\"\n",
    );
    doc.add_edge("a", "b").unwrap();
    // The edge into b called `b` exists under a different stage, so the
    // name is free here.
    assert!(doc.to_toml().contains("name = \"b\"\nfrom = \"a\""));
    let text = doc.to_toml();
    assert!(
        text.find("from = \"a\"").unwrap() < text.find("[[graph.stages]]\nname = \"b\"").unwrap(),
        "{text}"
    );
    runtime_ok(&doc);
    let mut fresh = small("[[graph.stages]]\nname = \"a\"\n");
    fresh.add_edge("a", "a").unwrap();
    assert!(
        fresh.to_toml().contains("[[graph.edges]]"),
        "{}",
        fresh.to_toml()
    );
    runtime_ok(&fresh);
    // A taken name gets a number: `to-2`, `to-3`.
    let mut taken = small(
        "[[graph.stages]]\nname = \"a\"\n\n[[graph.stages]]\nname = \"b\"\n\n\
         [[graph.edges]]\nname = \"b\"\nfrom = \"a\"\nto = \"a\"\n\n\
         [[graph.edges]]\nname = \"b-2\"\nfrom = \"a\"\nto = \"a\"\nwhen = \"error\"\n",
    );
    taken.add_edge("a", "b").unwrap();
    assert!(
        taken.to_toml().contains("name = \"b-3\""),
        "{}",
        taken.to_toml()
    );
    runtime_ok(&taken);
    // Edges written inline stay inline; a list that is not a list is refused.
    let mut inline = ManifestDoc::parse(
        "[blueprint]\nname = \"i\"\nversion = \"1\"\n[graph]\n\
         layout = { total_budget_tokens = 0, regions = [] }\n\
         stages = [{ name = \"a\" }]\nedges = []\n",
    )
    .unwrap();
    inline.add_stage("b", Some("a")).unwrap();
    inline.add_edge("a", "b").unwrap();
    let text = inline.to_toml();
    assert!(
        text.contains("stages = [{ name = \"a\" }, { name = \"b\""),
        "{text}"
    );
    assert!(
        text.contains("edges = [{ name = \"b\", from = \"a\", to = \"b\""),
        "{text}"
    );
    runtime_ok(&inline);
    inline.rename_stage("a", "start").unwrap();
    assert!(inline.edge("start", "b").is_some());
    // A third entry on one line is spaced like the second.
    inline.add_stage("c", None).unwrap();
    assert!(
        inline.to_toml().contains("}, { name = \"c\""),
        "{}",
        inline.to_toml()
    );
    let mut fresh_inline = ManifestDoc::parse(
        "[blueprint]\nname = \"i\"\nversion = \"1\"\n[graph]\n\
         layout = { total_budget_tokens = 0, regions = [] }\nstages = [{ name = \"a\" }]\n",
    )
    .unwrap();
    fresh_inline.add_edge("a", "a").unwrap();
    assert!(fresh_inline.to_toml().contains("edges = [{ name = \"a\""));
    let mut odd = small("edges = 3\n[[graph.stages]]\nname = \"a\"\n");
    assert_eq!(
        odd.add_edge("a", "a"),
        Err(EditError::NotATable("edges".into()))
    );
    assert!(odd.edges().is_empty());
}

#[test]
fn transforms_and_their_rules() {
    let mut doc = coder();
    // Direct is written as absent; clear by name.
    doc.set_transform("discover", "plan", &TransformKind::Clear)
        .unwrap();
    assert_eq!(
        doc.edge("discover", "plan").unwrap().transform,
        TransformKind::Clear
    );
    assert!(doc.to_toml().contains("carry = \"clear\""));
    doc.set_transform("discover", "plan", &TransformKind::Direct)
        .unwrap();
    assert_eq!(
        doc.edge("discover", "plan").unwrap().transform,
        TransformKind::Direct
    );
    doc.set_transform("discover", "plan", &TransformKind::Other("teleport".into()))
        .unwrap();
    assert_eq!(
        doc.edge("discover", "plan").unwrap().transform,
        TransformKind::Other("teleport".into())
    );
    // The first switch to custom files every non-pinned region the stage
    // sees under carry; picking custom again keeps the rules.
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
    // A compacting carry keeps its prompt in `prompt`.
    doc.set_transform("discover", "plan", &TransformKind::Compact)
        .unwrap();
    doc.set_compact_prompt("discover", "plan", "Short").unwrap();
    assert!(
        doc.to_toml()
            .contains("carry = { compact = { prompt = \"Short\" } }")
    );
    assert_eq!(
        doc.edge("discover", "plan").unwrap().rules.compact_prompt,
        "Short"
    );
    doc.set_compact_prompt("discover", "plan", "").unwrap();
    assert!(doc.to_toml().contains("carry = { compact = {} }"));
    runtime_ok(&doc);
    // A rule or a prompt on a direct path makes it custom; clearing a prompt
    // that is not there does nothing.
    let mut fresh = starter();
    fresh.set_compact_prompt("work", "finish", "").unwrap();
    assert_eq!(
        fresh.edge("work", "finish").unwrap().transform,
        TransformKind::Direct
    );
    fresh
        .set_compact_prompt("work", "finish", "Summarize")
        .unwrap();
    assert_eq!(
        fresh.edge("work", "finish").unwrap().transform,
        TransformKind::Custom
    );
    fresh.set_compact_prompt("work", "finish", "").unwrap();
    fresh
        .set_transform("work", "finish", &TransformKind::Direct)
        .unwrap();
    fresh
        .set_transform_rule("work", "finish", "conversation", Rule::Carry)
        .unwrap();
    assert_eq!(
        fresh.edge("work", "finish").unwrap().rules.carry,
        ["conversation"]
    );
    runtime_ok(&fresh);
    // Custom on a stage that sees only pinned regions starts empty.
    let mut pinned = small(
        "[[graph.stages]]\nname = \"a\"\n[[graph.edges]]\nname = \"a\"\nfrom = \"a\"\nto = \"a\"\n",
    );
    pinned
        .set_transform("a", "a", &TransformKind::Custom)
        .unwrap();
    assert!(pinned.to_toml().contains("carry = { custom = {} }"));
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
    // Rules that are not a table are refused, not clobbered.
    let mut odd = small(
        "[[graph.stages]]\nname = \"a\"\n[[graph.edges]]\nname = \"a\"\nfrom = \"a\"\nto = \"a\"\ncarry = { custom = 3 }\n",
    );
    assert_eq!(
        odd.set_transform_rule("a", "a", "r", Rule::Carry),
        Err(EditError::NotATable("custom".into()))
    );
    assert_eq!(
        odd.set_compact_prompt("a", "a", "x"),
        Err(EditError::NotATable("custom".into()))
    );
}

// ─── regions and routing ─────────────────────────────────────────────────────

#[test]
fn regions_are_added_renamed_deleted_and_edited_in_both_scopes() {
    let mut doc = starter();
    assert_eq!(doc.regions(None).len(), 2);
    doc.add_region(&RegionScope::Shared, "notes").unwrap();
    let notes = doc.region(None, "notes").unwrap();
    // A starter region is a percentage and nothing else: a default ceiling is
    // exactly what stops the percentage deciding anything on a real window.
    assert_eq!(
        (notes.kind.as_str(), notes.budget_percent, notes.max_tokens),
        ("pinned", Some(5.0), None)
    );
    assert!(
        doc.to_toml()
            .contains("    { name = \"notes\", kind = \"pinned\", budget = \"5%\" },\n]"),
        "one entry per line, like its neighbours: {}",
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
    runtime_ok(&doc);
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

    // A stage's own layout: a copy of the shared layout, then independent.
    doc.create_stage_override("finish").unwrap();
    let own = doc.effective_regions(Some("finish"));
    assert!(!own.inherited);
    assert_eq!(
        own.regions
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        ["task", "conversation", "other"]
    );
    runtime_ok(&doc);
    doc.create_stage_override("finish").unwrap(); // already has one: no change
    doc.add_region(&RegionScope::Stage("finish".into()), "scratch")
        .unwrap();
    assert_eq!(doc.regions(Some("finish")).len(), 4);
    assert_eq!(doc.regions(None).len(), 3, "the shared layout is untouched");
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
    doc.delete_region(&RegionScope::Stage("finish".into()), "pad")
        .unwrap();
    assert_eq!(doc.regions(Some("finish")).len(), 3);
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
    // A stage gets a layout of its own by adding a region to it, and a graph
    // with no shared layout gives an override that starts empty.
    let mut bare = starter();
    bare.add_region(&RegionScope::Stage("work".into()), "solo")
        .unwrap();
    assert_eq!(bare.regions(Some("work")).len(), 1);
    let mut no_layout = ManifestDoc::parse(
        "[blueprint]\nname = \"n\"\nversion = \"1\"\n[[graph.stages]]\nname = \"a\"\n",
    )
    .unwrap();
    no_layout.create_stage_override("a").unwrap();
    assert!(no_layout.regions(Some("a")).is_empty());
    assert!(!no_layout.effective_regions(Some("a")).inherited);
    no_layout.add_region(&RegionScope::Shared, "first").unwrap();
    assert_eq!(no_layout.regions(None).len(), 1);
    runtime_ok(&no_layout);
    // A layout with no regions list gets one.
    let mut no_list =
        small("[[graph.stages]]\nname = \"a\"\nlayout = { total_budget_tokens = 0 }\n");
    no_list
        .add_region(&RegionScope::Stage("a".into()), "x")
        .unwrap();
    assert_eq!(no_list.regions(Some("a")).len(), 1);
    // A layout that is not a table is refused.
    let mut odd = small("[[graph.stages]]\nname = \"a\"\nlayout = 3\n");
    assert_eq!(
        odd.add_region(&RegionScope::Stage("a".into()), "x"),
        Err(EditError::NotATable("layout".into()))
    );
}

#[test]
fn a_regions_input_is_bound_unbound_and_followed() {
    use RegionField as F;
    use RegionValue as V;
    let s = RegionScope::Shared;
    let mut doc = starter();
    doc.add_region(&s, "notes").unwrap();
    // A new input name declares a text input bound to the region.
    doc.set_region_field(&s, "notes", F::Seed, V::Text("brief".into()))
        .unwrap();
    assert_eq!(doc.region(None, "notes").unwrap().seed, "brief");
    assert!(
        doc.to_toml()
            .contains("name = \"brief\"\ntype = \"text\"\nbinds = [{ region = \"notes\" }]"),
        "{}",
        doc.to_toml()
    );
    runtime_ok(&doc);
    // Binding the same input again changes nothing; binding an existing
    // input adds the region to its binds and drops the old one, which had
    // nowhere else to go.
    let before = doc.to_toml();
    doc.set_region_field(&s, "notes", F::Seed, V::Text("brief".into()))
        .unwrap();
    assert_eq!(doc.to_toml(), before);
    doc.set_region_field(&s, "notes", F::Seed, V::Text("task".into()))
        .unwrap();
    assert_eq!(doc.region(None, "notes").unwrap().seed, "task");
    assert!(!doc.to_toml().contains("brief"), "{}", doc.to_toml());
    assert!(
        doc.to_toml()
            .contains("binds = [{ region = \"task\" }, { region = \"notes\" }]")
    );
    // A rename of the region follows the binding; a delete unbinds it.
    doc.rename_region(&s, "notes", "memo").unwrap();
    assert_eq!(doc.region(None, "memo").unwrap().seed, "task");
    runtime_ok(&doc);
    doc.delete_region(&s, "memo").unwrap();
    assert!(doc.to_toml().contains("binds = [{ region = \"task\" }]"));
    // Clearing unbinds, and drops an input left binding nothing.
    doc.set_region_field(&s, "task", F::Seed, V::Text(String::new()))
        .unwrap();
    assert_eq!(doc.region(None, "task").unwrap().seed, "");
    assert!(
        !doc.to_toml().contains("[[graph.inputs]]"),
        "{}",
        doc.to_toml()
    );
    // An input with no binds gets a list; a name an input cannot have is
    // refused; binds that are not a list are refused.
    let mut odd = small(
        "[[graph.inputs]]\nname = \"loose\"\ntype = \"text\"\n\n\
         [[graph.inputs]]\nname = \"stiff\"\ntype = \"text\"\nbinds = 3\n\n\
         [[graph.stages]]\nname = \"a\"\n",
    );
    odd.set_region_field(&s, "r", F::Seed, V::Text("loose".into()))
        .unwrap();
    assert_eq!(odd.region(None, "r").unwrap().seed, "loose");
    assert_eq!(
        odd.set_region_field(&s, "r", F::Seed, V::Text("9lives".into())),
        Err(EditError::BadName("9lives".into()))
    );
    assert_eq!(
        odd.set_region_field(&s, "r", F::Seed, V::Text("stiff".into())),
        Err(EditError::NotATable("binds".into()))
    );
    // A region with a seed of its own is left alone.
    let mut coder = coder();
    let before = coder.to_toml();
    coder
        .set_region_field(&s, "conventions", F::Seed, V::Text("task".into()))
        .unwrap();
    assert_eq!(coder.to_toml(), before);
    assert_eq!(
        coder.set_region_field(&s, "ghost", F::Seed, V::Text("task".into())),
        Err(EditError::NoSuchRegion("ghost".into()))
    );
    // A stage region's rename and delete leave the inputs alone while
    // another layout still has a region of the name, and follow it once
    // none does.
    let mut staged = starter();
    staged.create_stage_override("work").unwrap();
    let scope = RegionScope::Stage("work".into());
    staged.rename_region(&scope, "task", "job").unwrap();
    assert_eq!(staged.region(None, "task").unwrap().seed, "task");
    staged.delete_region(&scope, "job").unwrap();
    assert_eq!(staged.region(None, "task").unwrap().seed, "task");
    staged.add_region(&scope, "notes").unwrap();
    staged
        .set_region_field(&scope, "notes", F::Seed, V::Text("brief".into()))
        .unwrap();
    assert_eq!(staged.region(Some("work"), "notes").unwrap().seed, "brief");
    staged.rename_region(&scope, "notes", "memo").unwrap();
    assert_eq!(staged.region(Some("work"), "memo").unwrap().seed, "brief");
    runtime_ok(&staged);
    staged.delete_region(&scope, "memo").unwrap();
    assert!(!staged.to_toml().contains("brief"), "{}", staged.to_toml());
    staged.delete_region(&scope, "conversation").unwrap();
    assert!(staged.region(None, "conversation").is_some());
    let mut no_list = small("inputs = 3\n[[graph.stages]]\nname = \"a\"\n");
    assert_eq!(
        no_list.set_region_field(&s, "r", F::Seed, V::Text("brief".into())),
        Err(EditError::NotATable("inputs".into()))
    );
}

#[test]
fn region_fields_write_the_lairs_way() {
    use RegionField as F;
    use RegionValue as V;
    let mut doc = starter();
    doc.add_region(&RegionScope::Shared, "r").unwrap();
    let s = RegionScope::Shared;
    let r = |doc: &ManifestDoc| doc.region(None, "r").unwrap();
    // A sliding window is written with a size; the same kind again keeps it.
    doc.set_region_field(&s, "r", F::Kind, V::Text("sliding_window".into()))
        .unwrap();
    assert_eq!(
        (r(&doc).kind.as_str(), r(&doc).max_items),
        ("sliding_window", Some(10))
    );
    doc.set_region_field(&s, "r", F::MaxItems, V::Number(Some(12)))
        .unwrap();
    doc.set_region_field(&s, "r", F::Kind, V::Text("sliding_window".into()))
        .unwrap();
    assert_eq!(r(&doc).max_items, Some(12));
    doc.set_region_field(&s, "r", F::Kind, V::Text("".into()))
        .unwrap();
    assert_eq!(r(&doc).kind, "sliding_window", "kind is never emptied");
    runtime_ok(&doc);
    // Eviction: bulk and compact carry a count, kept across a switch;
    // per_item is the default and is written as absent.
    assert!(matches!(
        doc.set_region_field(&s, "r", F::Overflow, V::Number(Some(3))),
        Err(EditError::OutOfRange(_))
    ));
    doc.set_region_field(&s, "r", F::Strategy, V::Text("bulk".into()))
        .unwrap();
    assert_eq!(
        (r(&doc).strategy.as_str(), r(&doc).overflow),
        ("bulk", Some(10))
    );
    doc.set_region_field(&s, "r", F::Overflow, V::Number(Some(3)))
        .unwrap();
    doc.set_region_field(&s, "r", F::Strategy, V::Text("compact".into()))
        .unwrap();
    assert_eq!(
        (r(&doc).strategy.as_str(), r(&doc).overflow),
        ("compact", Some(3))
    );
    runtime_ok(&doc);
    doc.set_region_field(&s, "r", F::Overflow, V::Number(Some(0)))
        .unwrap();
    assert_eq!(r(&doc).overflow, Some(1));
    doc.set_region_field(&s, "r", F::Overflow, V::Number(None))
        .unwrap();
    assert_eq!(r(&doc).overflow, Some(10));
    assert!(matches!(
        doc.set_region_field(&s, "r", F::Strategy, V::Text("lifo".into())),
        Err(EditError::OutOfRange(_))
    ));
    doc.set_region_field(&s, "r", F::Strategy, V::Text("per_item".into()))
        .unwrap();
    assert_eq!(r(&doc).strategy, "");
    // Taking the size off a window leaves the bare name.
    doc.set_region_field(&s, "r", F::MaxItems, V::Number(None))
        .unwrap();
    assert!(doc.to_toml().contains("kind = \"sliding_window\""));
    doc.set_region_field(&s, "r", F::MaxItems, V::Number(Some(0)))
        .unwrap();
    assert_eq!(r(&doc).max_items, Some(1));
    doc.set_region_field(&s, "r", F::Kind, V::Text("pinned".into()))
        .unwrap();
    assert!(doc.to_toml().contains("{ name = \"r\", kind = \"pinned\""));
    // A size given to a kind written by name turns it into a table.
    doc.set_region_field(&s, "r", F::Strategy, V::Text("".into()))
        .unwrap();
    assert!(doc.to_toml().contains("{ name = \"r\", kind = \"pinned\""));
    runtime_ok(&doc);

    // The budget's share, clamped; a floor and a ceiling turn it into a
    // table, and taking both off turns it back.
    doc.set_region_field(&s, "r", F::BudgetPercent, V::Number(Some(150)))
        .unwrap();
    assert_eq!(r(&doc).budget_percent, Some(100.0), "clamped");
    doc.set_region_field(&s, "r", F::MaxTokens, V::Number(Some(0)))
        .unwrap();
    assert_eq!(r(&doc).max_tokens, Some(1));
    doc.set_region_field(&s, "r", F::MaxTokens, V::Number(Some(1000)))
        .unwrap();
    doc.set_region_field(&s, "r", F::MinTokens, V::Number(Some(50)))
        .unwrap();
    assert!(
        doc.to_toml()
            .contains("budget = { percent = \"100%\", max = 1000, min = 50 }"),
        "{}",
        doc.to_toml()
    );
    doc.set_region_field(&s, "r", F::BudgetPercent, V::Number(Some(20)))
        .unwrap();
    assert_eq!(
        (r(&doc).budget_percent, r(&doc).min_tokens),
        (Some(20.0), Some(50))
    );
    runtime_ok(&doc);
    doc.set_region_field(&s, "r", F::MaxTokens, V::Number(None))
        .unwrap();
    doc.set_region_field(&s, "r", F::MinTokens, V::Number(None))
        .unwrap();
    assert!(doc.to_toml().contains("budget = \"20%\""));
    doc.set_region_field(&s, "r", F::BudgetPercent, V::Number(None))
        .unwrap();
    assert_eq!(r(&doc).budget_percent, None);
    // No percentage: a clamp is refused, taking one off is nothing.
    assert!(matches!(
        doc.set_region_field(&s, "r", F::MaxTokens, V::Number(Some(5))),
        Err(EditError::OutOfRange(_))
    ));
    assert!(matches!(
        doc.set_region_field(&s, "r", F::MinTokens, V::Number(Some(5))),
        Err(EditError::OutOfRange(_))
    ));
    doc.set_region_field(&s, "r", F::MinTokens, V::Number(None))
        .unwrap();
    doc.set_region_field(&s, "r", F::BudgetPercent, V::Number(Some(5)))
        .unwrap();

    doc.set_region_field(&s, "r", F::Required, V::Flag(true))
        .unwrap();
    doc.set_region_field(&s, "r", F::RequiredMessage, V::Text("Fill me".into()))
        .unwrap();
    doc.set_region_field(&s, "r", F::Description, V::Text("What it holds".into()))
        .unwrap();
    let region = r(&doc);
    assert!(region.required);
    assert_eq!(region.required_message, "Fill me");
    assert_eq!(region.description, "What it holds");
    doc.set_region_field(&s, "r", F::Required, V::Flag(false))
        .unwrap();
    assert!(!r(&doc).required);
    assert!(
        !doc.to_toml()
            .contains("{ name = \"r\", kind = \"pinned\", budget = \"5%\", required =")
    );
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
fn mime_keys_write_the_way_the_runtime_reads_them() {
    use ArtifactField as A;
    let mut doc = starter();
    let s = RegionScope::Shared;
    doc.add_region(&s, "shots").unwrap();
    doc.set_region_field(
        &s,
        "shots",
        RegionField::Accepts,
        RegionValue::Text("Image/*, audio/wav image/*".into()),
    )
    .unwrap();
    assert_eq!(
        doc.region(None, "shots").unwrap().accepts,
        ["image/*", "audio/wav"]
    );
    runtime_ok(&doc);
    doc.set_region_field(
        &s,
        "shots",
        RegionField::Accepts,
        RegionValue::Text(" ".into()),
    )
    .unwrap();
    assert!(doc.region(None, "shots").unwrap().accepts.is_empty());
    assert!(!doc.to_toml().contains("accepts"), "{}", doc.to_toml());
    // The stage's input lists come and go.
    assert_eq!(
        doc.set_stage_input("ghost", InputList::Accepts, &[]),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    doc.set_stage_input("work", InputList::Accepts, &["video/*".to_string()])
        .unwrap();
    doc.set_stage_input("work", InputList::AsText, &["model/obj".to_string()])
        .unwrap();
    let w = doc.stage("work").unwrap();
    assert_eq!(w.input_accepts, ["video/*"]);
    assert_eq!(w.input_as_text, ["model/obj"]);
    runtime_ok(&doc);
    doc.set_stage_input("work", InputList::Accepts, &[])
        .unwrap();
    doc.set_stage_input("work", InputList::AsText, &[]).unwrap();
    assert!(!doc.to_toml().contains("input_"), "{}", doc.to_toml());
    // Artifacts: an inline list under `output`, edited by index, refused
    // when wrong.
    assert!(doc.artifacts("work").is_empty());
    assert!(doc.artifacts("ghost").is_empty());
    doc.add_artifact("work", "final").unwrap();
    assert_eq!(
        doc.add_artifact("work", "final"),
        Err(EditError::Taken("final".into()))
    );
    assert_eq!(
        doc.add_artifact("work", "no way"),
        Err(EditError::BadName("no way".into()))
    );
    assert_eq!(
        doc.add_artifact("ghost", "x"),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    doc.add_artifact("work", "track").unwrap();
    assert!(
        doc.to_toml().contains(
            "output = { artifacts = [{ name = \"final\", mime_type = \"*/*\" }, { name = \"track\", mime_type = \"*/*\" }] }"
        ),
        "{}",
        doc.to_toml()
    );
    doc.set_artifact("work", 0, A::Type("video/mp4".into()))
        .unwrap();
    doc.set_artifact("work", 0, A::Required(true)).unwrap();
    doc.set_artifact("work", 0, A::Description("the cut".into()))
        .unwrap();
    doc.set_artifact("work", 1, A::Name("audio".into()))
        .unwrap();
    assert_eq!(
        doc.artifacts("work"),
        vec![
            ArtifactView {
                name: "final".into(),
                mime_type: "video/mp4".into(),
                required: true,
                description: "the cut".into(),
            },
            ArtifactView {
                name: "audio".into(),
                mime_type: "*/*".into(),
                required: false,
                description: String::new(),
            },
        ]
    );
    assert_eq!(doc.stage("work").unwrap().artifacts.len(), 2);
    assert_eq!(
        doc.set_artifact("work", 1, A::Name("final".into())),
        Err(EditError::Taken("final".into()))
    );
    assert_eq!(
        doc.set_artifact("work", 1, A::Name("a b".into())),
        Err(EditError::BadName("a b".into()))
    );
    assert!(matches!(
        doc.set_artifact("work", 0, A::Type(String::new())),
        Err(EditError::OutOfRange(_))
    ));
    assert!(matches!(
        doc.set_artifact("work", 5, A::Required(true)),
        Err(EditError::OutOfRange(_))
    ));
    assert_eq!(
        doc.set_artifact("ghost", 0, A::Required(true)),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    let graph = runtime_ok(&doc);
    let declared = &graph.stages[0].output.as_ref().unwrap().artifacts;
    assert_eq!(declared.len(), 2);
    assert!(declared[0].required && declared[0].mime_type.as_str() == "video/mp4");
    doc.set_artifact("work", 0, A::Required(false)).unwrap();
    doc.set_artifact("work", 0, A::Description(String::new()))
        .unwrap();
    assert!(
        !doc.to_toml().contains("required = true }"),
        "{}",
        doc.to_toml()
    );
    // Deleting: out of range refused; the last one takes the table with it.
    assert!(matches!(
        doc.delete_artifact("work", 2),
        Err(EditError::OutOfRange(_))
    ));
    doc.delete_artifact("work", 0).unwrap();
    assert_eq!(doc.artifacts("work")[0].name, "audio");
    doc.delete_artifact("work", 0).unwrap();
    assert!(!doc.to_toml().contains("output"), "{}", doc.to_toml());
    assert!(matches!(
        doc.delete_artifact("work", 0),
        Err(EditError::OutOfRange(_))
    ));
    assert_eq!(
        doc.delete_artifact("ghost", 0),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    // An output table with other keys keeps them when its list empties; a
    // headed list stays headed.
    let mut shaped = small(
        "[[graph.stages]]\nname = \"a\"\n[graph.stages.output]\nformat = \"json\"\n\
         [[graph.stages.output.artifacts]]\nname = \"x\"\nmime_type = \"*/*\"\n",
    );
    assert_eq!(shaped.artifacts("a").len(), 1);
    shaped.add_artifact("a", "y").unwrap();
    assert!(
        shaped
            .to_toml()
            .contains("[[graph.stages.output.artifacts]]\nname = \"y\""),
        "{}",
        shaped.to_toml()
    );
    runtime_ok(&shaped);
    assert!(matches!(
        shaped.delete_artifact("a", 5),
        Err(EditError::OutOfRange(_))
    ));
    shaped.delete_artifact("a", 0).unwrap();
    shaped.delete_artifact("a", 0).unwrap();
    let text = shaped.to_toml();
    assert!(
        text.contains("format = \"json\"") && !text.contains("artifacts"),
        "{text}"
    );
    // A list that is not a list is refused rather than clobbered, and an
    // entry that is not a table is skipped.
    let mut odd = small(
        "[[graph.stages]]\nname = \"a\"\noutput = { artifacts = 3 }\n\
         [[graph.stages]]\nname = \"b\"\noutput = { artifacts = [1, { name = \"x\", mime_type = \"y/z\" }] }\n\
         [[graph.stages]]\nname = \"c\"\noutput = \"nope\"\n",
    );
    assert_eq!(
        odd.add_artifact("a", "x"),
        Err(EditError::NotATable("a list".into()))
    );
    assert!(matches!(
        odd.set_artifact("a", 0, A::Required(true)),
        Err(EditError::OutOfRange(_))
    ));
    assert!(matches!(
        odd.delete_artifact("a", 0),
        Err(EditError::OutOfRange(_))
    ));
    assert_eq!(odd.artifacts("b").len(), 1);
    odd.set_artifact("b", 0, A::Type("z/z".into())).unwrap();
    assert_eq!(odd.artifacts("b")[0].mime_type, "z/z");
    assert!(matches!(
        odd.delete_artifact("b", 1),
        Err(EditError::OutOfRange(_))
    ));
    odd.delete_artifact("b", 0).unwrap();
    assert!(odd.artifacts("b").is_empty());
    assert_eq!(
        odd.add_artifact("c", "x"),
        Err(EditError::NotATable("output".into()))
    );
    assert!(matches!(
        odd.delete_artifact("c", 0),
        Err(EditError::OutOfRange(_))
    ));
    // The typed-list splitter.
    assert_eq!(split_list(" a/b,, c/D\tc/d "), ["a/b", "c/d"]);
    assert!(split_list(", ").is_empty());
    // The answer format comes and goes with its table.
    let mut doc = starter();
    doc.set_output_format("work", "markdown").unwrap();
    assert_eq!(doc.stage("work").unwrap().output_format, "markdown");
    assert!(doc.to_toml().contains("output = { format = \"markdown\" }"));
    doc.set_output_format("work", "").unwrap();
    assert!(!doc.to_toml().contains("output"), "{}", doc.to_toml());
    doc.set_output_format("work", "markdown").unwrap();
    doc.add_artifact("work", "final").unwrap();
    doc.set_output_format("work", "").unwrap();
    let text = doc.to_toml();
    assert!(
        !text.contains("format") && text.contains("artifacts"),
        "{text}"
    );
    doc.delete_artifact("work", 0).unwrap();
    assert!(!doc.to_toml().contains("output"), "{}", doc.to_toml());
    doc.set_output_format("work", "json").unwrap();
    assert!(matches!(
        doc.delete_artifact("work", 0),
        Err(EditError::OutOfRange(_))
    ));
    doc.set_output_format("work", "").unwrap();
    assert_eq!(
        doc.set_output_format("ghost", "x"),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    // What a tool may be handed: written per tool, read in order, lifted by
    // an empty list, the table going with the last one.
    doc.set_tool_accepts(
        "work",
        "spawn_agent",
        &["image/*".to_string(), "audio/wav".to_string()],
    )
    .unwrap();
    doc.set_tool_accepts("work", "context_export", &["text/*".to_string()])
        .unwrap();
    assert_eq!(
        doc.stage("work").unwrap().tool_accepts,
        vec![
            (
                "spawn_agent".to_string(),
                vec!["image/*".to_string(), "audio/wav".to_string()]
            ),
            ("context_export".to_string(), vec!["text/*".to_string()]),
        ]
    );
    let graph = runtime_ok(&doc);
    assert_eq!(graph.stages[0].tool_accepts.len(), 2);
    doc.set_tool_accepts("work", "spawn_agent", &[]).unwrap();
    assert_eq!(doc.stage("work").unwrap().tool_accepts.len(), 1);
    doc.set_tool_accepts("work", "context_export", &[]).unwrap();
    assert!(!doc.to_toml().contains("tool_accepts"), "{}", doc.to_toml());
    doc.set_tool_accepts("work", "context_export", &[]).unwrap();
    assert_eq!(
        doc.set_tool_accepts("work", "no way", &["x/y".to_string()]),
        Err(EditError::BadName("no way".into()))
    );
    assert_eq!(
        doc.set_tool_accepts("ghost", "t", &["x/y".to_string()]),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    // A limit that is not a list is skipped by the view and refused by the
    // writer; a format table that is not a table is refused too.
    let mut odd = small(
        "[[graph.stages]]\nname = \"a\"\ntool_accepts = { t = 3, u = [\"x/y\"] }\noutput = \"nope\"\n\
         [[graph.stages]]\nname = \"b\"\ntool_accepts = 3\n",
    );
    assert_eq!(
        odd.stage("a").unwrap().tool_accepts,
        vec![("u".to_string(), vec!["x/y".to_string()])]
    );
    assert_eq!(odd.stage("a").unwrap().output_format, "");
    assert_eq!(
        odd.set_output_format("a", "json"),
        Err(EditError::NotATable("output".into()))
    );
    odd.set_output_format("a", "").unwrap();
    odd.set_tool_accepts("a", "t", &["a/b".to_string()])
        .unwrap();
    assert_eq!(odd.stage("a").unwrap().tool_accepts.len(), 2);
    assert!(odd.stage("b").unwrap().tool_accepts.is_empty());
    assert_eq!(
        odd.set_tool_accepts("b", "t", &["a/b".to_string()]),
        Err(EditError::NotATable("tool_accepts".into()))
    );
    odd.set_tool_accepts("b", "t", &[]).unwrap();
    // The graph's own mime rows, by pattern.
    let rows = small(
        "[graph.mime_types]\n\"model/x\" = { text = false }\n[[graph.stages]]\nname = \"a\"\n",
    );
    assert_eq!(mime_type_keys(&rows), ["model/x"]);
    assert!(mime_type_keys(&starter()).is_empty());
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
    doc.set_tool_routing_override("work", "bash", "task")
        .unwrap();
    doc.set_tool_routing_override("work", "read_file", "task")
        .unwrap();
    let routing = doc.tool_routing("work");
    assert_eq!(routing.default_region.as_deref(), Some("conversation"));
    assert_eq!(routing.overrides.len(), 2);
    assert!(doc.to_toml().contains(
        "tool_routing = { default_region = \"conversation\", tool_regions = { bash = \"task\", read_file = \"task\" } }"
    ));
    runtime_ok(&doc);
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
    let mut odd = small(
        "[[graph.stages]]\nname = \"a\"\ntool_routing = { default_region = \"r\", tool_regions = 3 }\n\
         [[graph.stages]]\nname = \"b\"\ntool_routing = 4\n",
    );
    assert_eq!(
        odd.set_tool_routing_override("a", "bash", "r"),
        Err(EditError::NotATable("tool_regions".into()))
    );
    assert_eq!(
        odd.set_tool_routing_default("b", "r"),
        Err(EditError::NotATable("tool_routing".into()))
    );
    odd.set_tool_routing_default("b", "").unwrap();
    odd.set_tool_routing_override("b", "t", "").unwrap();
    assert_eq!(
        odd.set_tool_routing_override("b", "t", "r"),
        Err(EditError::NotATable("tool_routing".into()))
    );
}

#[test]
fn output_routing_and_reset_round_trip_through_the_file() {
    let mut doc =
        small("[[graph.stages]]\nname = \"draw\"\n\n[[graph.stages]]\nname = \"describe\"\n");
    // A ghost stage is refused for both setters.
    assert_eq!(
        doc.set_output_routing("ghost", &[("image/*".into(), "r".into())]),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    assert_eq!(
        doc.set_context_reset("ghost", &["r".into()]),
        Err(EditError::NoSuchStage("ghost".into()))
    );
    // output_routing: written in the given order, read back, then rewritten.
    doc.set_output_routing(
        "draw",
        &[
            ("image/*".into(), "r".into()),
            ("application/pdf".into(), "r".into()),
        ],
    )
    .unwrap();
    assert_eq!(
        doc.stage("draw").unwrap().output_routing,
        [
            ("image/*".to_string(), "r".to_string()),
            ("application/pdf".to_string(), "r".to_string()),
        ]
    );
    runtime_ok(&doc);
    doc.set_output_routing("draw", &[("image/*".into(), "r".into())])
        .unwrap();
    assert_eq!(
        doc.stage("draw").unwrap().output_routing,
        [("image/*".to_string(), "r".to_string())]
    );
    doc.set_output_routing("draw", &[]).unwrap();
    assert!(doc.stage("draw").unwrap().output_routing.is_empty());
    assert!(
        !doc.to_toml().contains("output_routing"),
        "{}",
        doc.to_toml()
    );
    // reset: set, read back, then cleared.
    doc.set_context_reset("describe", &["r".into()]).unwrap();
    assert_eq!(doc.stage("describe").unwrap().context_reset, ["r"]);
    runtime_ok(&doc);
    doc.set_context_reset("describe", &[]).unwrap();
    assert!(!doc.to_toml().contains("reset"), "{}", doc.to_toml());
    // A non-table `output_routing` is refused, not clobbered; an entry that
    // is not a string is left out of the view.
    let mut odd = small(
        "[[graph.stages]]\nname = \"a\"\noutput_routing = 3\n\
         [[graph.stages]]\nname = \"b\"\noutput_routing = { \"image/*\" = 3, \"text/*\" = \"r\" }\n",
    );
    assert_eq!(
        odd.set_output_routing("a", &[("image/*".into(), "r".into())]),
        Err(EditError::NotATable("output_routing".into()))
    );
    assert_eq!(
        odd.stage("b").unwrap().output_routing,
        [("text/*".to_string(), "r".to_string())]
    );
}

// ─── check ───────────────────────────────────────────────────────────────────

#[test]
fn check_reports_parse_validate_and_lint_in_that_order() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let parse = check("not = [toml", &dir);
    assert_eq!(parse.error_count(), 1);
    assert_eq!(parse.items[0].code, "parse");
    assert!(!parse.is_saveable());
    assert_eq!(parse.first().map(|p| p.severity), Some(Severity::Error));

    // A dangling worker stage fails validation on the stage.
    let mut doc = starter();
    doc.set_stage_mode("work", &StageModeView::FanOut).unwrap();
    doc.set_fan_out(
        "work",
        FanOutField::Worker(Some((WorkerKind::Stage, "nope".into()))),
    )
    .unwrap();
    let invalid = check(&doc.to_toml(), &dir);
    assert_eq!(invalid.items[0].code, "validate");
    assert_eq!(
        invalid.items[0].stage.as_deref(),
        Some("work"),
        "{invalid:?}"
    );
    assert!(!invalid.for_stage("work").is_empty());
    // A validation error that names no stage.
    let text = starter()
        .to_toml()
        .replace("entry = \"work\"", "entry = \"ghost\"");
    let graph = check(&text, &dir);
    assert_eq!(graph.items[0].code, "validate");
    assert_eq!(graph.items[0].stage, None);
    assert!(graph.items[0].message.contains("ghost"), "{graph:?}");
    // An edge into a stage that is not there is filed under the stage it
    // leaves.
    let dangling = starter()
        .to_toml()
        .replace("to = \"finish\"", "to = \"ghost\"");
    let problems = check(&dangling, &dir);
    assert_eq!(problems.items[0].code, "validate");
    assert_eq!(
        problems.items[0].stage.as_deref(),
        Some("work"),
        "{problems:?}"
    );
    // A stage declared twice is filed under its name.
    let twice = starter()
        .to_toml()
        .replace("name = \"finish\"\nmode", "name = \"work\"\nmode");
    let problems = check(&twice, &dir);
    let duplicate = problems
        .items
        .iter()
        .find(|p| p.message.contains("declared twice"))
        .unwrap_or_else(|| panic!("{problems:?}"));
    assert_eq!(duplicate.stage.as_deref(), Some("work"));
    // An issue under some other list names no stage.
    let bad_input = starter().to_toml().replace(
        "binds = [{ region = \"task\" }]",
        "binds = [{ region = \"ghost\" }]",
    );
    let problems = check(&bad_input, &dir);
    assert_eq!(problems.items[0].code, "validate");
    assert_eq!(problems.items[0].stage, None, "{problems:?}");
    // A command seed is worth a note, which sorts after warnings.
    let noted = starter().to_toml().replace(
        "{ name = \"task\", kind = \"pinned\", budget = \"5%\" },",
        "{ name = \"task\", kind = \"pinned\", budget = \"5%\" },\n    { name = \"env\", kind = \"pinned\", budget = \"1%\", seed = { command = \"echo hi\" } },",
    );
    let problems = check(&noted, &dir);
    assert!(
        problems
            .items
            .iter()
            .any(|p| p.severity == Severity::Note && p.code == "command-seed"),
        "{problems:?}"
    );
    assert_eq!(problems.items.last().unwrap().severity, Severity::Note);
    // The starter itself: no errors, warnings about what it leaves to defaults.
    let starter_problems = check(&starter().to_toml(), &dir);
    assert!(starter_problems.is_saveable(), "{starter_problems:?}");
    assert!(starter_problems.warning_count() > 0);
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
    let graph = runtime_ok(&doc);
    assert_eq!(graph.inputs.len(), 1, "the task reaches the work");
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
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
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
    let coder_file = agents.join("coder").join("agent.toml");
    let edited = std::fs::read_to_string(&coder_file)
        .unwrap()
        .replace("entry = \"discover\"", "entry = \"plan\"");
    std::fs::write(&coder_file, edited).unwrap();
    std::fs::write(
        configured.join("mine").join("agent.toml"),
        templates::empty_blueprint("mine").unwrap(),
    )
    .unwrap();
    std::fs::write(
        cwd.join("agent.toml"),
        templates::empty_blueprint("here").unwrap(),
    )
    .unwrap();
    // A directory without a readable file is skipped by the list.
    std::fs::create_dir_all(agents.join("junk")).unwrap();
    std::fs::write(agents.join("junk").join("agent.toml"), "not = [").unwrap();
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
    // Extras of a bundled agent land next to a cloned file.
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
    // A file that cannot be written where its directory could.
    let dir_as_file = agents.join("taken");
    std::fs::create_dir_all(&dir_as_file).unwrap();
    std::fs::create_dir_all(dir_as_file.join("agent.toml")).unwrap();
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
    use order::{Seg, Spot, move_block, renumber, written_order};
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
    // A block moves after a table, or before one (or to the end when the
    // table is not there); nothing moves for a block with no table or an
    // anchor that is not there.
    let a = [Seg::Key("a".into())];
    let d = [Seg::Key("d".into())];
    move_block(&mut doc, &a, Spot::Before(&d));
    assert_eq!(written_order(&doc)[0], a.to_vec());
    move_block(&mut doc, &a, Spot::After(&d));
    assert_eq!(written_order(&doc)[1], a.to_vec());
    move_block(&mut doc, &d, Spot::Before(&[Seg::Key("ghost".into())]));
    assert_eq!(written_order(&doc).last().unwrap(), &d.to_vec());
    let before = doc.to_string();
    move_block(&mut doc, &[Seg::Key("ghost".into())], Spot::After(&d));
    move_block(&mut doc, &a, Spot::After(&[Seg::Key("ghost".into())]));
    assert_eq!(doc.to_string(), before);
}

#[test]
fn renaming_an_agent_moves_its_directory_and_the_name_in_its_file() {
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
    std::fs::write(agents.join("junk").join("agent.toml"), "= not toml").unwrap();
    assert!(catalog::rename_agent(&agents, "junk", "other").is_err());
    assert!(agents.join("own").exists());
    // The same name is nothing to do.
    assert_eq!(
        catalog::rename_agent(&agents, "own", "own").unwrap(),
        agents.join("own")
    );
    // A move the disk refuses: the file already carries the new name under
    // the old directory, and says so.
    let err = catalog::rename_agent_with(&agents, "own", "mine", &mut |_, _| {
        Err(std::io::Error::other("disk says no"))
    })
    .unwrap_err();
    assert!(err.contains("disk says no"), "{err}");
    assert!(agents.join("own").exists());
    let stuck = std::fs::read_to_string(agents.join("own").join("agent.toml")).unwrap();
    assert!(stuck.contains("name = \"mine\""));
    std::fs::write(agents.join("own").join("agent.toml"), &text).unwrap();
    // A file that cannot be written: said, nothing moved.
    let file = agents.join("own").join("agent.toml");
    let writable = std::fs::metadata(&file).unwrap().permissions();
    let mut locked = writable.clone();
    locked.set_readonly(true);
    std::fs::set_permissions(&file, locked).unwrap();
    let err = catalog::rename_agent(&agents, "own", "mine").unwrap_err();
    assert!(err.contains("Could not write"), "{err}");
    assert!(agents.join("own").exists());
    std::fs::set_permissions(&file, writable).unwrap();
    // The rename: the directory moves, the name changes, the comment stays.
    let new = catalog::rename_agent(&agents, "own", "mine").unwrap();
    assert_eq!(new, agents.join("mine"));
    assert!(!agents.join("own").exists());
    let moved = std::fs::read_to_string(new.join("agent.toml")).unwrap();
    assert!(moved.starts_with("# kept\n"), "{moved}");
    assert!(moved.contains("name = \"mine\""), "{moved}");
    let _ = std::fs::remove_dir_all(&root);
}
