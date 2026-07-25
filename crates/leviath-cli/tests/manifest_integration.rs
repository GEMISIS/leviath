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

#[test]
fn at_least_nine_builtin_agents_exist() {
    let manifests = discover_agent_manifests();
    assert!(
        manifests.len() >= 9,
        "Expected at least 9 built-in agents, found {}",
        manifests.len()
    );
}
