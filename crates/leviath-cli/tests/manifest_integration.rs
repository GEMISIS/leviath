//! Integration tests for agent manifest parsing.
//!
//! These tests parse every built-in agent manifest and validate the resulting blueprints.

use std::path::PathBuf;

/// Root of this crate, which holds the bundled `agents/` directory.
fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Discover all agent.leviath files in the agents/ directory.
fn discover_agent_manifests() -> Vec<(String, PathBuf)> {
    let agents_dir = crate_root().join("agents");
    let mut manifests = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&agents_dir) {
        for entry in entries.flatten() {
            let manifest = entry.path().join("agent.leviath");
            if manifest.exists() {
                let name = entry.file_name().to_string_lossy().to_string();
                manifests.push((name, manifest));
            }
        }
    }

    manifests.sort_by(|a, b| a.0.cmp(&b.0));
    manifests
}

#[test]
fn all_builtin_agents_parse_successfully() {
    let manifests = discover_agent_manifests();
    assert!(
        !manifests.is_empty(),
        "No agent manifests found - check agents/ directory"
    );

    for (name, path) in &manifests {
        let content = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", path.display(), e));

        let blueprint = leviath_core::manifest::parse_manifest(&content)
            .unwrap_or_else(|e| panic!("Failed to parse agent '{}': {}", name, e));

        // Basic sanity checks
        assert!(
            !blueprint.name.is_empty(),
            "Agent '{}' has empty name",
            name
        );
        assert!(
            !blueprint.stages.is_empty(),
            "Agent '{}' has no stages",
            name
        );
    }
}

#[test]
fn all_builtin_agents_validate() {
    let manifests = discover_agent_manifests();

    for (name, path) in &manifests {
        let content = std::fs::read_to_string(path).unwrap();
        let blueprint = leviath_core::manifest::parse_manifest(&content).unwrap();

        blueprint
            .validate()
            .unwrap_or_else(|e| panic!("Agent '{}' failed validation: {:?}", name, e));
    }
}

#[test]
fn all_builtin_agents_have_valid_entry_stage() {
    let manifests = discover_agent_manifests();

    for (name, path) in &manifests {
        let content = std::fs::read_to_string(path).unwrap();
        let blueprint = leviath_core::manifest::parse_manifest(&content).unwrap();

        let entry = blueprint.resolve_entry_stage_name();
        assert!(
            blueprint.find_stage(&entry).is_some(),
            "Agent '{}' entry stage '{}' not found in stages",
            name,
            entry
        );
    }
}

#[test]
fn all_builtin_agents_transition_targets_exist() {
    let manifests = discover_agent_manifests();

    for (name, path) in &manifests {
        let content = std::fs::read_to_string(path).unwrap();
        let blueprint = leviath_core::manifest::parse_manifest(&content).unwrap();

        for stage in &blueprint.stages {
            if let Some(ref transitions) = stage.transitions {
                for target_name in transitions.keys() {
                    assert!(
                        blueprint.find_stage(target_name).is_some(),
                        "Agent '{}', stage '{}': transition target '{}' not found",
                        name,
                        stage.name,
                        target_name
                    );
                }
            }
        }
    }
}

#[test]
fn specific_agent_coder_has_expected_structure() {
    let path = crate_root().join("agents/coder/agent.leviath");
    let content = std::fs::read_to_string(&path).unwrap();
    let bp = leviath_core::manifest::parse_manifest(&content).unwrap();

    assert_eq!(bp.name, "coder");
    assert!(bp.stages.len() >= 2);
    // `plan` is the stage that carries the approval checkpoint, and `implement`
    // is what it gates. Both are load-bearing for this agent's whole shape.
    assert!(bp.find_stage("plan").is_some());
    assert!(bp.find_stage("implement").is_some());

    // Should have graph transitions
    let plan = bp.find_stage("plan").unwrap();
    assert!(plan.transitions.is_some());
}

/// A `required` region the AGENT is expected to fill is enforced at runtime by
/// `require_context_regions`, which re-runs the stage with a nudge until the
/// region has content. But `unmet_required_regions` returns EMPTY for a stage
/// with no context-writing tool - gating a stage that *can't* populate the
/// region would loop pointlessly - so a blueprint that marks a region `required`
/// while giving no stage `context_write`/`context_append` gets a gate that
/// silently does nothing, and the region stays blank with no error.
///
/// Caller-input regions are exempt: those are supplied and validated at spawn,
/// not written by the agent.
#[test]
fn agent_written_required_regions_have_a_stage_that_can_write_them() {
    use leviath_core::layout::RegionSeed;

    for (name, path) in &discover_agent_manifests() {
        let content = std::fs::read_to_string(path).unwrap();
        let bp = leviath_core::manifest::parse_manifest(&content).unwrap();

        let agent_written: Vec<&str> = bp
            .context_layout
            .regions
            .iter()
            .filter(|r| r.required)
            .filter(|r| !matches!(r.seed, Some(RegionSeed::CallerInput { .. })))
            .map(|r| r.name.as_str())
            .collect();
        if agent_written.is_empty() {
            continue;
        }

        let writers: Vec<&str> = bp
            .stages
            .iter()
            .filter(|s| {
                s.available_tools
                    .iter()
                    .any(|t| t == "context_write" || t == "context_append")
            })
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !writers.is_empty(),
            "agent '{name}' marks {agent_written:?} required but no stage has \
             context_write/context_append - the required-region gate is a no-op \
             and the region silently stays empty"
        );
    }
}

/// A `required` region must not also carry a file/glob/command seed: those are
/// resolved at spawn, where `required` turns any miss (absent file, empty glob,
/// failing command) into a hard spawn error. An agent-populated region should be
/// left unseeded so `resolve_seeds` skips it and the runtime gate handles it.
#[test]
fn required_regions_are_not_also_seeded_from_the_environment() {
    use leviath_core::layout::RegionSeed;

    for (name, path) in &discover_agent_manifests() {
        let content = std::fs::read_to_string(path).unwrap();
        let bp = leviath_core::manifest::parse_manifest(&content).unwrap();

        for region in bp.context_layout.regions.iter().filter(|r| r.required) {
            let environmental = matches!(
                region.seed,
                Some(
                    RegionSeed::Files { .. } | RegionSeed::Glob { .. } | RegionSeed::Command { .. }
                )
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

#[test]
fn specific_agent_researcher_has_graph_transitions() {
    let path = crate_root().join("agents/researcher/agent.leviath");
    let content = std::fs::read_to_string(&path).unwrap();
    let bp = leviath_core::manifest::parse_manifest(&content).unwrap();

    assert_eq!(bp.name, "researcher");
    // Researcher should have multiple stages with transitions
    let has_transitions = bp.stages.iter().any(|s| s.transitions.is_some());
    assert!(
        has_transitions || bp.stages.len() > 1,
        "Researcher agent should have transitions or multiple stages"
    );
}

/// Enforce the context-layout invariants that keep tool_routing + message
/// assembly sound (see the runtime fixes in pipeline.rs / context_setup.rs):
///   1. every agent declares an explicit `conversation` SlidingWindow region
///      (an auto-added one is dropped on the first stage transition);
///   2. no tool-routing target is a SlidingWindow OTHER than `conversation`
///      (only `conversation` may hold ToolResult message blocks - a routed
///      result elsewhere desyncs from its tool_use → Anthropic 400);
///   3. every Compacting region has a paired CompactHistory region.
#[test]
fn all_builtin_agents_have_sound_context_layout() {
    use leviath_core::RegionKind;

    for (name, path) in &discover_agent_manifests() {
        let content = std::fs::read_to_string(path).unwrap();
        let bp = leviath_core::manifest::parse_manifest(&content).unwrap();
        let regions = &bp.context_layout.regions;

        // (1) explicit conversation sliding_window
        let conv = regions.iter().find(|r| r.name == "conversation");
        assert!(
            matches!(
                conv.map(|r| &r.kind),
                Some(RegionKind::SlidingWindow { .. })
            ),
            "agent '{name}' must declare an explicit `conversation` sliding_window region"
        );

        // (2) no routing targets a non-conversation sliding_window
        let sliding: std::collections::HashSet<&str> = regions
            .iter()
            .filter(|r| matches!(r.kind, RegionKind::SlidingWindow { .. }))
            .map(|r| r.name.as_str())
            .collect();
        for stage in &bp.stages {
            if let Some(routing) = &stage.tool_result_routing {
                let mut targets = vec![routing.default_region.as_str()];
                targets.extend(routing.tool_overrides.values().map(String::as_str));
                for t in targets {
                    assert!(
                        t == "conversation" || !sliding.contains(t),
                        "agent '{name}' stage '{}' routes tool results to non-conversation \
                         sliding_window region '{t}' (would desync tool_result from tool_use)",
                        stage.name
                    );
                }
            }
        }

        // (3) every compacting region has a compact_history pair
        let hist_sources: std::collections::HashSet<&str> = regions
            .iter()
            .filter_map(|r| match &r.kind {
                RegionKind::CompactHistory { source_region } => Some(source_region.as_str()),
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

/// Every `condition = "stuck"` edge in every built-in agent must be armed with
/// a threshold AND point at a `max_revisits`-capped stage.
///
/// The arming half is also enforced by `Blueprint::validate`, but the revisit
/// cap cannot be: an uncapped stuck target is a perfectly valid graph, it just
/// lets the source stage bounce out to it on every re-entry for the life of the
/// run. That's an authoring invariant for the shipped agents, so it lives here.
#[test]
fn all_builtin_stuck_edges_are_armed_and_bounded() {
    use leviath_core::TransitionCondition;

    for (name, path) in &discover_agent_manifests() {
        let content = std::fs::read_to_string(path).unwrap();
        let bp = leviath_core::manifest::parse_manifest(&content).unwrap();

        for stage in &bp.stages {
            let Some(transitions) = &stage.transitions else {
                continue;
            };
            for (target, edge) in transitions {
                if edge.condition != TransitionCondition::Stuck {
                    continue;
                }
                assert!(
                    edge.stuck.is_some_and(|c| c.is_armed()),
                    "agent '{name}' stage '{}': stuck edge → '{target}' has no threshold, \
                     so it could never fire",
                    stage.name
                );
                assert!(
                    bp.find_stage(target)
                        .is_some_and(|s| s.max_revisits.is_some()),
                    "agent '{name}' stage '{}': stuck edge → '{target}' is unbounded - \
                     '{target}' needs max_revisits or the two can ping-pong all run",
                    stage.name
                );
            }
        }
    }
}

/// Every built-in agent with an `error`-conditioned edge must give the runtime's
/// abnormal-ending notes (issue #154: inference errors, iteration caps) a home:
///   1. a pinned `error_report` region, so the note survives every edge
///      transform into the stage that acts on it (the runtime falls back to
///      `conversation`, but a bundled agent should use the durable region);
///   2. that region must NOT be the agent's first pinned region -
///      `apply_stage_context` injects stage instructions into the first pinned
///      region, and a 2k-token report region would swallow them;
///   3. every error-edge target's system prompt must tell the model to read
///      `error_report` - the region is useless if no prompt points at it.
#[test]
fn builtin_error_edges_have_a_pinned_error_report_region() {
    use leviath_core::{RegionKind, TransitionCondition};

    for (name, path) in &discover_agent_manifests() {
        let content = std::fs::read_to_string(path).unwrap();
        let bp = leviath_core::manifest::parse_manifest(&content).unwrap();

        let error_targets: std::collections::BTreeSet<&str> = bp
            .stages
            .iter()
            .filter_map(|s| s.transitions.as_ref())
            .flatten()
            .filter(|(_, e)| e.condition == TransitionCondition::Error)
            .map(|(target, _)| target.as_str())
            .collect();
        if error_targets.is_empty() {
            continue; // a stage may legitimately declare no error edge
        }

        let pinned: Vec<&str> = bp
            .context_layout
            .regions
            .iter()
            .filter(|r| matches!(r.kind, RegionKind::Pinned))
            .map(|r| r.name.as_str())
            .collect();
        assert!(
            pinned.contains(&"error_report"),
            "agent '{name}' has error edges but no pinned `error_report` region - \
             the runtime's error/iteration-cap notes would land in the evictable \
             `conversation` window instead"
        );
        assert_ne!(
            pinned.first(),
            Some(&"error_report"),
            "agent '{name}': `error_report` is the FIRST pinned region, so stage \
             instructions would be injected into it (apply_stage_context targets \
             the first pinned region) - declare it after the other pinned regions"
        );

        for target in error_targets {
            let stage = bp
                .find_stage(target)
                .unwrap_or_else(|| panic!("agent '{name}': error edge → unknown stage '{target}'"));
            assert!(
                stage
                    .config
                    .get("system_prompt")
                    .and_then(|v| v.as_str())
                    .is_some_and(|p| p.contains("error_report")),
                "agent '{name}' stage '{target}' is an error-edge target but its \
                 system prompt never mentions `error_report` - the model won't \
                 know where the runtime put the error text"
            );
        }
    }
}

/// A stage with two or more *choosable* outgoing edges is routed by an LLM
/// (`resolve_transition_sync` returns `Choose`, and `build_transition_prompt`
/// asks the model to name a stage). Without a `transition_prompt` the model gets
/// only the bare stage names plus whatever `hint`s exist, and has to guess the
/// selection criteria - so a branching stage must either explain the choice in a
/// prompt or label every branch with a hint.
///
/// `allow_complete` stages are included: they reach the same lane with a single
/// edge, since DONE is itself one of the choices.
#[test]
fn branching_stages_explain_how_to_choose() {
    use leviath_core::TransitionCondition;

    for (name, path) in &discover_agent_manifests() {
        let content = std::fs::read_to_string(path).unwrap();
        let bp = leviath_core::manifest::parse_manifest(&content).unwrap();

        for stage in &bp.stages {
            let Some(transitions) = &stage.transitions else {
                continue;
            };
            // Only Always/LlmChoice edges are offered to the model; conditioned
            // edges (error/max_iterations/stuck) fire from the runtime instead.
            let choosable: Vec<&str> = transitions
                .values()
                .filter(|e| {
                    matches!(
                        e.condition,
                        TransitionCondition::Always | TransitionCondition::LlmChoice
                    )
                })
                .map(|e| e.target.as_str())
                .collect();
            if choosable.len() < 2 {
                continue;
            }
            let unlabeled: Vec<&str> = transitions
                .values()
                .filter(|e| {
                    matches!(
                        e.condition,
                        TransitionCondition::Always | TransitionCondition::LlmChoice
                    ) && e.hint.is_none()
                })
                .map(|e| e.target.as_str())
                .collect();
            assert!(
                stage.transition_prompt.is_some() || unlabeled.is_empty(),
                "agent '{name}' stage '{}' branches to {choosable:?} but has no \
                 transition_prompt, and {unlabeled:?} carry no hint either - the \
                 model is left to guess which branch to take",
                stage.name
            );
        }
    }
}

/// A human tool kept through an unattended run has to be one the stage can
/// actually call, and it only makes sense where a person is genuinely expected.
///
/// `Blueprint::validate` already rejects the first half; this pins the intent
/// for the shipped catalog: nothing opts out of the unattended cut except a
/// stage that also declares an interaction point, so no bundled agent can start
/// parking `--yolo` runs on a prompt by accident (issue #204).
#[test]
fn builtin_required_tools_are_offered_and_belong_to_an_interactive_stage() {
    use leviath_core::blueprint::StageMode;

    for (name, path) in &discover_agent_manifests() {
        let content = std::fs::read_to_string(path).unwrap();
        let blueprint = leviath_core::manifest::parse_manifest(&content).unwrap();

        for stage in &blueprint.stages {
            for tool in &stage.required_tools {
                assert!(
                    stage.available_tools.contains(tool),
                    "agent '{name}' stage '{}' keeps '{tool}' through an unattended run \
                     but never offers it",
                    stage.name
                );
            }
            assert!(
                stage.required_tools.is_empty()
                    || matches!(stage.mode, StageMode::InteractivePoints { .. }),
                "agent '{name}' stage '{}' holds {:?} for a person, but declares no \
                 interaction point - an unattended run would park there with nobody \
                 watching",
                stage.name,
                stage.required_tools
            );
        }
    }
}

/// The set is deliberately small, and every one of them is discovered rather
/// than named here: an assertion listing agents by name has to be edited every
/// time the set changes, which is how it stops being a check and starts being a
/// chore. What matters is that agents ship at all, and that each one parses.
#[test]
fn the_bundled_agents_exist_and_are_discoverable() {
    let manifests = discover_agent_manifests();
    assert!(
        !manifests.is_empty(),
        "the binary ships no agents; build.rs found no agents/ directory"
    );
}

/// A stage that can start a fan-out must not also offer the model a way
/// straight to the deliverable.
///
/// This is what a real `wide-researcher` run did: `survey` listed
/// `investigate` (the fan-out), `compare`, and `summarize`, and the model
/// picked `summarize` on its first transition. The run finished "complete"
/// having skipped the fan-out and the two stages after it, with no sub-agents
/// and nothing to say anything had been skipped.
///
/// The escape to the deliverable is meant for one case - every looping target
/// out of revisits - and `condition = "dead_end"` is how you say that. The
/// engine consults such an edge only when nothing else can be followed, so it
/// is not on the menu. A plain edge to the same stage is offered every single
/// turn, which is the shape the lint's own fix text warns against.
#[test]
fn a_stage_that_can_fan_out_offers_no_shortcut_past_it() {
    use leviath_core::blueprint::{StageMode, TransitionCondition};

    let manifests = discover_agent_manifests();
    let mut checked = 0;

    for (name, path) in &manifests {
        let content = std::fs::read_to_string(path).unwrap();
        let blueprint = leviath_core::manifest::parse_manifest(&content).unwrap();

        let is_fan_out = |target: &str| {
            blueprint
                .stages
                .iter()
                .any(|s| s.name == target && matches!(s.mode, StageMode::FanOut { .. }))
        };
        let is_output = |target: &str| {
            blueprint
                .stages
                .iter()
                .any(|s| s.name == target && matches!(s.mode, StageMode::Output))
        };

        for stage in &blueprint.stages {
            let Some(edges) = &stage.transitions else {
                continue;
            };
            if !edges.values().any(|e| is_fan_out(&e.target)) {
                continue;
            }
            checked += 1;
            let shortcuts: Vec<&str> = edges
                .values()
                .filter(|e| {
                    is_output(&e.target)
                        && matches!(
                            e.condition,
                            TransitionCondition::Always | TransitionCondition::LlmChoice
                        )
                })
                .map(|e| e.target.as_str())
                .collect();
            assert!(
                shortcuts.is_empty(),
                "{name}: stage '{}' can fan out, but also offers the model a \
                 plain edge straight to {shortcuts:?}. Gate it with \
                 condition = \"dead_end\" so it is taken only when nothing \
                 else can be.",
                stage.name
            );
        }
    }

    assert!(
        checked > 0,
        "no bundled agent has a stage that can fan out, so this proves nothing"
    );
}
