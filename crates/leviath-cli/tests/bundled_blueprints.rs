//! Integration tests over the bundled blueprints.
//!
//! These read every built-in `agent.toml` and hold its graph to the authoring
//! invariants the shipped agents keep.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

use leviath_blueprint::BlueprintFile;
use leviath_runtime::spec::graph::{
    EdgeCondition, EdgeDef, RegionKind, RunGraph, Seed, StageMode, ToolSelector,
};

/// Root of this crate, which holds the bundled `agents/` directory.
fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every bundled blueprint, by directory name, read.
fn bundled() -> Vec<(String, RunGraph)> {
    let mut found: Vec<(String, RunGraph)> = std::fs::read_dir(crate_root().join("agents"))
        .expect("the agents directory is there")
        .flatten()
        .filter(|e| e.path().join(leviath_blueprint::FILE_NAME).is_file())
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let text = std::fs::read_to_string(e.path().join(leviath_blueprint::FILE_NAME))
                .expect("the blueprint reads");
            let file = BlueprintFile::parse(&text).expect("the blueprint parses");
            (name, file.run_graph())
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// One bundled blueprint's graph.
fn graph_of(name: &str) -> RunGraph {
    bundled()
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, g)| g)
        .expect("the blueprint is bundled")
}

/// A stage's tool names.
fn tools(selectors: &[ToolSelector]) -> Vec<String> {
    selectors
        .iter()
        .filter_map(|t| match t {
            ToolSelector::Tool(name) => Some(name.to_string()),
            ToolSelector::Group(_) => None,
        })
        .collect()
}

/// Whether the model chooses this edge, rather than the runtime firing it.
fn choosable(edge: &EdgeDef) -> bool {
    matches!(edge.when, EdgeCondition::Always | EdgeCondition::LlmChoice)
}

#[test]
fn every_bundled_blueprint_reads_and_holds_together() {
    let all = bundled();
    assert!(
        !all.is_empty(),
        "the binary ships no agents; build.rs found no agents/ directory"
    );
    for (name, graph) in &all {
        assert!(!graph.stages.is_empty(), "agent '{name}' has no stages");
        let checked = graph.validate(&leviath_runtime::spec::issues::SpecPath::root());
        assert!(
            checked.is_ok(),
            "agent '{name}' failed validation: {checked:?}"
        );
        assert!(
            graph.entry_stage().is_some(),
            "agent '{name}' has no entry stage"
        );
    }
}

#[test]
fn coder_has_its_approval_and_implementation_stages() {
    let graph = graph_of("coder");
    // `plan` is the stage that carries the approval checkpoint, and
    // `implement` is what it gates. Both are load-bearing for this agent.
    assert!(graph.stage("plan").is_some());
    assert!(graph.stage("implement").is_some());
    assert!(graph.edges_from("plan").count() > 0);
}

#[test]
fn researcher_has_several_stages_joined_by_edges() {
    let graph = graph_of("researcher");
    assert!(graph.stages.len() > 1);
    assert!(!graph.edges.is_empty());
}

/// A `required` region the AGENT is expected to fill is enforced at runtime
/// by re-running the stage with a nudge until the region has content. But a
/// stage with no context-writing tool cannot fill one, so a blueprint that
/// marks a region `required` while giving no stage `context_write` or
/// `context_append` gets a gate that silently does nothing.
///
/// Regions an input fills are exempt: those are supplied and checked at spawn.
#[test]
fn agent_written_required_regions_have_a_stage_that_can_write_them() {
    for (name, graph) in &bundled() {
        let filled_by_input: HashSet<String> = graph
            .inputs
            .iter()
            .flat_map(|i| i.binds.iter())
            .filter_map(|slot| match slot {
                leviath_runtime::spec::inputs::InputSlot::Region(b) => Some(b.region.to_string()),
                _ => None,
            })
            .collect();
        let agent_written: Vec<&str> = graph
            .layout
            .regions
            .iter()
            .filter(|r| r.required && !filled_by_input.contains(r.name.as_str()))
            .map(|r| r.name.as_str())
            .collect();
        if agent_written.is_empty() {
            continue;
        }
        let writers = graph.stages.iter().any(|s| {
            tools(&s.tools)
                .iter()
                .any(|t| t == "context_write" || t == "context_append")
        });
        assert!(
            writers,
            "agent '{name}' marks {agent_written:?} required but no stage has \
             context_write/context_append - the required-region gate is a no-op"
        );
    }
}

/// A `required` region must not also carry a file, glob or command seed: those
/// are run at spawn, where `required` turns any miss into a refused spawn.
#[test]
fn required_regions_are_not_also_seeded_from_the_environment() {
    for (name, graph) in &bundled() {
        for region in graph.layout.regions.iter().filter(|r| r.required) {
            let environmental = matches!(
                region.seed,
                Some(Seed::Files(_) | Seed::Glob(_) | Seed::Command(_))
            );
            assert!(
                !environmental,
                "agent '{name}' region '{}' is required AND seeded from the \
                 environment - a missing file or failing command would fail the \
                 spawn outright",
                region.name
            );
        }
    }
}

/// The context-layout invariants that keep tool routing and message assembly
/// sound:
///   1. every agent declares an explicit `conversation` sliding window;
///   2. no tool-routing target is a sliding window other than `conversation`
///      (only it may hold tool results; a routed result elsewhere drops out of
///      step with its tool_use, and the provider refuses the request);
///   3. every compacting region has a paired compact history.
#[test]
fn all_builtin_agents_have_sound_context_layout() {
    for (name, graph) in &bundled() {
        let regions = &graph.layout.regions;
        let conv = regions.iter().find(|r| r.name.as_str() == "conversation");
        assert!(
            matches!(
                conv.map(|r| &r.kind),
                Some(RegionKind::SlidingWindow { .. })
            ),
            "agent '{name}' must declare an explicit `conversation` sliding_window region"
        );

        let sliding: HashSet<&str> = regions
            .iter()
            .filter(|r| matches!(r.kind, RegionKind::SlidingWindow { .. }))
            .map(|r| r.name.as_str())
            .collect();
        for stage in &graph.stages {
            let Some(routing) = &stage.tool_routing else {
                continue;
            };
            let targets =
                std::iter::once(&routing.default_region).chain(routing.tool_regions.values());
            for t in targets {
                assert!(
                    t.as_str() == "conversation" || !sliding.contains(t.as_str()),
                    "agent '{name}' stage '{}' routes tool results to non-conversation \
                     sliding_window region '{t}'",
                    stage.name
                );
            }
        }

        let hist_sources: HashSet<&str> = regions
            .iter()
            .filter_map(|r| match &r.kind {
                RegionKind::CompactHistory { source } => source.as_ref().map(|s| s.as_str()),
                _ => None,
            })
            .collect();
        for r in regions {
            if matches!(r.kind, RegionKind::Compacting { .. }) {
                assert!(
                    hist_sources.contains(r.name.as_str()),
                    "agent '{name}': compacting region '{}' has no paired compact_history region",
                    r.name
                );
            }
        }
    }
}

/// Every `stuck` edge must be armed with a threshold AND point at a stage
/// with `max_revisits`: an uncapped stuck target is a valid graph, it just
/// lets the source stage bounce out to it on every re-entry for the life of
/// the run.
#[test]
fn all_builtin_stuck_edges_are_armed_and_bounded() {
    for (name, graph) in &bundled() {
        for edge in graph
            .edges
            .iter()
            .filter(|e| e.when == EdgeCondition::Stuck)
        {
            let armed = edge.stuck.as_ref().is_some_and(|s| {
                s.after_iterations.is_some()
                    || s.after_minutes.is_some()
                    || s.after_same_file_edits.is_some()
                    || s.after_tool_calls.is_some()
            });
            assert!(
                armed,
                "agent '{name}': stuck edge {} -> {} has no threshold, so it could never fire",
                edge.from, edge.to
            );
            assert!(
                graph
                    .stage(edge.to.as_str())
                    .is_some_and(|s| s.max_revisits.is_some()),
                "agent '{name}': stuck edge {} -> {} is unbounded - the target needs \
                 max_revisits or the two can ping-pong all run",
                edge.from,
                edge.to
            );
        }
    }
}

/// Every agent with an `error` edge gives the runtime's notes about an
/// abnormal ending a home:
///   1. a pinned `error_report` region, so the note survives every edge into
///      the stage that acts on it;
///   2. not the first pinned region, which stage instructions are put in;
///   3. every error-edge target's system prompt tells the model to read it.
#[test]
fn builtin_error_edges_have_a_pinned_error_report_region() {
    for (name, graph) in &bundled() {
        let error_targets: BTreeSet<&str> = graph
            .edges
            .iter()
            .filter(|e| e.when == EdgeCondition::Error)
            .map(|e| e.to.as_str())
            .collect();
        if error_targets.is_empty() {
            continue;
        }
        let pinned: Vec<&str> = graph
            .layout
            .regions
            .iter()
            .filter(|r| matches!(r.kind, RegionKind::Pinned))
            .map(|r| r.name.as_str())
            .collect();
        assert!(
            pinned.contains(&"error_report"),
            "agent '{name}' has error edges but no pinned `error_report` region"
        );
        assert_ne!(
            pinned.first(),
            Some(&"error_report"),
            "agent '{name}': `error_report` is the FIRST pinned region, so stage \
             instructions would be put into it"
        );
        for target in error_targets {
            let prompt = graph
                .stage(target)
                .and_then(|s| s.system_prompt.as_deref())
                .unwrap_or_default();
            assert!(
                prompt.contains("error_report"),
                "agent '{name}' stage '{target}' is an error-edge target but its \
                 system prompt never mentions `error_report`"
            );
        }
    }
}

/// A stage with two or more edges the model chooses between must explain
/// the choice in a `transition_prompt` or label every branch with a hint.
#[test]
fn branching_stages_explain_how_to_choose() {
    for (name, graph) in &bundled() {
        for stage in &graph.stages {
            let edges: Vec<&EdgeDef> = graph
                .edges_from(stage.name.as_str())
                .filter(|e| choosable(e))
                .collect();
            if edges.len() < 2 {
                continue;
            }
            let unlabeled: Vec<&str> = edges
                .iter()
                .filter(|e| e.hint.is_none())
                .map(|e| e.to.as_str())
                .collect();
            assert!(
                stage.transition_prompt.is_some() || unlabeled.is_empty(),
                "agent '{name}' stage '{}' branches but has no transition_prompt, \
                 and {unlabeled:?} carry no hint either",
                stage.name
            );
        }
    }
}

/// A human tool kept through an unattended run must be one the stage offers,
/// on a stage that declares an interaction point, so no bundled agent parks
/// a `--yolo` run on a prompt by accident.
#[test]
fn builtin_required_tools_are_offered_and_belong_to_an_interactive_stage() {
    for (name, graph) in &bundled() {
        for stage in &graph.stages {
            let offered = tools(&stage.tools);
            for tool in &stage.required_tools {
                assert!(
                    offered.iter().any(|t| t == tool.as_str()),
                    "agent '{name}' stage '{}' keeps '{tool}' through an unattended run \
                     but never offers it",
                    stage.name
                );
            }
            assert!(
                stage.required_tools.is_empty()
                    || matches!(stage.mode, StageMode::InteractivePoints(_)),
                "agent '{name}' stage '{}' holds {:?} for a person, but declares no \
                 interaction point",
                stage.name,
                stage.required_tools
            );
        }
    }
}

/// A stage that can start a fan-out must not also offer the model a plain
/// edge straight to the deliverable: a real `wide-researcher` run once took
/// one and finished "complete" having skipped the fan-out. The escape to the
/// deliverable is a `dead_end` edge, which the engine follows only when
/// nothing else can be.
#[test]
fn a_stage_that_can_fan_out_offers_no_shortcut_past_it() {
    let mut checked = 0;
    for (name, graph) in &bundled() {
        let mode_of = |target: &str| graph.stage(target).map(|s| &s.mode);
        for stage in &graph.stages {
            let edges: Vec<&EdgeDef> = graph.edges_from(stage.name.as_str()).collect();
            if !edges
                .iter()
                .any(|e| matches!(mode_of(e.to.as_str()), Some(StageMode::FanOut(_))))
            {
                continue;
            }
            checked += 1;
            let shortcuts: Vec<&str> = edges
                .iter()
                .filter(|e| {
                    choosable(e) && matches!(mode_of(e.to.as_str()), Some(StageMode::Output))
                })
                .map(|e| e.to.as_str())
                .collect();
            assert!(
                shortcuts.is_empty(),
                "{name}: stage '{}' can fan out, but also offers the model a plain edge \
                 straight to {shortcuts:?}. Make it `when = \"dead_end\"`.",
                stage.name
            );
        }
    }
    assert!(
        checked > 0,
        "no bundled agent has a stage that can fan out, so this proves nothing"
    );
}
