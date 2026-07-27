//! Integration tests for agent manifest parsing.
//!
//! These tests parse every built-in agent manifest and validate the resulting blueprints.

use std::path::PathBuf;

/// Get the workspace root (assumes tests run from leviath-cli crate).
fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Discover all agent.leviath files in the agents/ directory.
fn discover_agent_manifests() -> Vec<(String, PathBuf)> {
    let agents_dir = workspace_root().join("agents");
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
        "No agent manifests found — check agents/ directory"
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
    let path = workspace_root().join("agents/coder/agent.leviath");
    let content = std::fs::read_to_string(&path).unwrap();
    let bp = leviath_core::manifest::parse_manifest(&content).unwrap();

    assert_eq!(bp.name, "coder");
    assert!(bp.stages.len() >= 2); // at least analyze + implement
    assert!(bp.find_stage("analyze").is_some());
    assert!(bp.find_stage("implement").is_some());

    // Should have graph transitions
    let analyze = bp.find_stage("analyze").unwrap();
    assert!(analyze.transitions.is_some());
}

/// A `required` region the AGENT is expected to fill is enforced at runtime by
/// `require_context_regions`, which re-runs the stage with a nudge until the
/// region has content. But `unmet_required_regions` returns EMPTY for a stage
/// with no context-writing tool — gating a stage that *can't* populate the
/// region would loop pointlessly — so a blueprint that marks a region `required`
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
             context_write/context_append — the required-region gate is a no-op \
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
                 environment — a missing file or failing command would fail the \
                 spawn outright",
                region.name
            );
        }
    }
}

#[test]
fn specific_agent_researcher_has_graph_transitions() {
    let path = workspace_root().join("agents/researcher/agent.leviath");
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
///      (only `conversation` may hold ToolResult message blocks — a routed
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
                    "agent '{name}' stage '{}': stuck edge → '{target}' is unbounded — \
                     '{target}' needs max_revisits or the two can ping-pong all run",
                    stage.name
                );
            }
        }
    }
}

/// A stage with two or more *choosable* outgoing edges is routed by an LLM
/// (`resolve_transition_sync` returns `Choose`, and `build_transition_prompt`
/// asks the model to name a stage). Without a `transition_prompt` the model gets
/// only the bare stage names plus whatever `hint`s exist, and has to guess the
/// selection criteria — so a branching stage must either explain the choice in a
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
                 transition_prompt, and {unlabeled:?} carry no hint either — the \
                 model is left to guess which branch to take",
                stage.name
            );
        }
    }
}

#[test]
fn at_least_nine_builtin_agents_exist() {
    let manifests = discover_agent_manifests();
    assert!(
        manifests.len() >= 9,
        "Expected at least 9 built-in agents, found {}",
        manifests.len()
    );
}
