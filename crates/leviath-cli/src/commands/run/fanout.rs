//! Fan-out worker resolution: turn a [`FanOutConfig`] into a concrete worker
//! agent type (blueprint) + entry stage.
//!
//! Sub-agent workers are ordinary agents that just have a parent, so each runs
//! from a *registered* blueprint. This module builds the registry (the local
//! blueprint plus every installed agent under `~/.leviath/agents`) and resolves
//! the three worker sources: a named installed agent (`worker_agent`), a stage
//! in the current blueprint (`worker_stage`), or discovery over installed
//! agents (`worker_query`).
//!
// These helpers are consumed by `run_fan_out_stage` (added in the following
// commit); until then they are exercised only by this module's tests.
#![allow(dead_code)]

use std::collections::HashMap;

use leviath_core::blueprint::FanOutConfig;
use leviath_core::Blueprint;
use leviath_package::AgentInstaller;

use super::manifest::parse_manifest;

/// A resolved fan-out worker: the blueprint to run and the stage to enter at.
#[derive(Debug, Clone)]
pub struct ResolvedWorker {
    /// The worker's agent-type blueprint.
    pub blueprint: Blueprint,
    /// The stage the worker enters at.
    pub entry_stage: String,
}

/// Build the registry of available agent types: the local blueprint plus every
/// installed agent. Installed agents that fail to parse are skipped (logged).
pub fn load_agent_registry(local: &Blueprint) -> HashMap<String, Blueprint> {
    load_agent_registry_with(local, &AgentInstaller::new())
}

/// [`load_agent_registry`] against a specific installer (used in tests to point
/// at a temp install dir instead of the real `~/.leviath/agents`).
pub fn load_agent_registry_with(
    local: &Blueprint,
    installer: &AgentInstaller,
) -> HashMap<String, Blueprint> {
    let mut registry = HashMap::new();
    if let Ok(installed) = installer.list_installed() {
        for agent in installed {
            let manifest = agent.path.join("agent.leviath");
            match std::fs::read_to_string(&manifest)
                .ok()
                .and_then(|c| parse_manifest(&c).ok())
            {
                Some(bp) => {
                    registry.insert(bp.name.clone(), bp);
                }
                None => {
                    tracing::warn!(agent = %agent.name, "skipping installed agent that failed to parse");
                }
            }
        }
    }
    // The local blueprint wins over any installed agent of the same name.
    registry.insert(local.name.clone(), local.clone());
    registry
}

/// Resolve a fan-out stage's worker into a concrete blueprint + entry stage.
///
/// `current` is the blueprint the fan-out stage lives in (used by the
/// `worker_stage` form). `registry` is the set of available agent types.
pub fn resolve_worker(
    config: &FanOutConfig,
    current: &Blueprint,
    registry: &HashMap<String, Blueprint>,
) -> anyhow::Result<ResolvedWorker> {
    if let Some(stage) = &config.worker_stage {
        // Self-as-agent: run this blueprint entered at the named stage. The
        // stage's existence + `allow_as_worker` opt-in are validated at load
        // time (Blueprint::validate_graph).
        return Ok(ResolvedWorker {
            blueprint: current.clone(),
            entry_stage: stage.clone(),
        });
    }

    if let Some(name) = &config.worker_agent {
        let bp = registry.get(name).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "fan_out worker_agent '{}' is not registered. Install it (lev add) first.",
                name
            )
        })?;
        let entry = bp.resolve_entry_stage_name();
        return Ok(ResolvedWorker {
            blueprint: bp,
            entry_stage: entry,
        });
    }

    if let Some(query) = &config.worker_query {
        let bp = discover_worker(query, registry)?;
        let entry = bp.resolve_entry_stage_name();
        return Ok(ResolvedWorker {
            blueprint: bp,
            entry_stage: entry,
        });
    }

    // validate_graph guarantees exactly one source is set, so this is only
    // reachable if a caller bypasses validation.
    Err(anyhow::anyhow!(
        "fan_out stage has no worker source (worker_agent / worker_stage / worker_query)"
    ))
}

/// Discover a worker agent type by matching `query` (case-insensitive substring)
/// against each registered agent's name, description, and `metadata.tags` /
/// `metadata.capabilities`. Requires exactly one match.
pub fn discover_worker(
    query: &str,
    registry: &HashMap<String, Blueprint>,
) -> anyhow::Result<Blueprint> {
    let needle = query.to_lowercase();
    let mut matches: Vec<&Blueprint> = registry
        .values()
        .filter(|bp| agent_matches(bp, &needle))
        .collect();
    // Deterministic ordering for stable error messages.
    matches.sort_by(|a, b| a.name.cmp(&b.name));

    match matches.as_slice() {
        [] => Err(anyhow::anyhow!(
            "fan_out worker_query '{}' matched no installed agent type",
            query
        )),
        [only] => Ok((*only).clone()),
        many => {
            let names: Vec<&str> = many.iter().map(|b| b.name.as_str()).collect();
            Err(anyhow::anyhow!(
                "fan_out worker_query '{}' is ambiguous — matched {}. Name one with worker_agent.",
                query,
                names.join(", ")
            ))
        }
    }
}

/// Whether an agent's name / description / metadata tags contain `needle`
/// (already lowercased).
fn agent_matches(bp: &Blueprint, needle: &str) -> bool {
    if bp.name.to_lowercase().contains(needle) || bp.description.to_lowercase().contains(needle) {
        return true;
    }
    for key in ["tags", "capabilities"] {
        if let Some(val) = bp.metadata.get(key) {
            if metadata_value_contains(val, needle) {
                return true;
            }
        }
    }
    false
}

/// Recursively check whether a metadata JSON value contains `needle` in any of
/// its strings (handles both a comma string and an array of strings).
fn metadata_value_contains(val: &serde_json::Value, needle: &str) -> bool {
    match val {
        serde_json::Value::String(s) => s.to_lowercase().contains(needle),
        serde_json::Value::Array(items) => items.iter().any(|v| metadata_value_contains(v, needle)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::blueprint::{ModelConfig, Stage, WorkerFailurePolicy};
    use leviath_core::layout::{ContextLayout, RegionDefinition};
    use leviath_core::RegionKind;

    fn bp(name: &str) -> Blueprint {
        let layout = ContextLayout::new(
            vec![RegionDefinition::new("sys".into(), RegionKind::Pinned, 500)],
            10_000,
        );
        Blueprint::new(
            name.into(),
            format!("{name} description"),
            vec![Stage::new(
                "main".into(),
                ModelConfig::new("mock".into(), "m".into()),
            )],
            layout,
        )
    }

    fn cfg() -> FanOutConfig {
        FanOutConfig {
            worker_agent: None,
            worker_stage: None,
            worker_query: None,
            merge_stage: None,
            max_workers: 4,
            on_worker_failure: WorkerFailurePolicy::Continue,
            split_prompt: String::new(),
        }
    }

    #[test]
    fn resolve_worker_stage_uses_current_blueprint() {
        let current = bp("self-agent");
        let mut c = cfg();
        c.worker_stage = Some("main".into());
        let r = resolve_worker(&c, &current, &HashMap::new()).unwrap();
        assert_eq!(r.blueprint.name, "self-agent");
        assert_eq!(r.entry_stage, "main");
    }

    #[test]
    fn resolve_worker_agent_from_registry() {
        let current = bp("root");
        let mut registry = HashMap::new();
        registry.insert("fixer".to_string(), bp("fixer"));
        let mut c = cfg();
        c.worker_agent = Some("fixer".into());
        let r = resolve_worker(&c, &current, &registry).unwrap();
        assert_eq!(r.blueprint.name, "fixer");
        assert_eq!(r.entry_stage, "main");
    }

    #[test]
    fn resolve_worker_agent_missing_errors() {
        let current = bp("root");
        let mut c = cfg();
        c.worker_agent = Some("ghost".into());
        let err = resolve_worker(&c, &current, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("not registered"));
    }

    #[test]
    fn resolve_worker_query_unique_match() {
        let current = bp("root");
        let mut registry = HashMap::new();
        registry.insert("reviewer".to_string(), bp("reviewer"));
        registry.insert("coder".to_string(), bp("coder"));
        let mut c = cfg();
        c.worker_query = Some("review".into());
        let r = resolve_worker(&c, &current, &registry).unwrap();
        assert_eq!(r.blueprint.name, "reviewer");
    }

    #[test]
    fn discover_worker_zero_and_ambiguous() {
        let mut registry = HashMap::new();
        registry.insert("alpha".to_string(), bp("alpha"));
        registry.insert("alto".to_string(), bp("alto"));
        // zero
        assert!(discover_worker("zzz", &registry)
            .unwrap_err()
            .to_string()
            .contains("matched no"));
        // ambiguous ("al" matches both), error lists sorted candidates
        let err = discover_worker("al", &registry).unwrap_err().to_string();
        assert!(err.contains("ambiguous"));
        assert!(err.contains("alpha, alto"));
    }

    #[test]
    fn discover_matches_metadata_tags() {
        let mut agent = bp("tagged");
        agent
            .metadata
            .insert("tags".to_string(), serde_json::json!(["security", "audit"]));
        let mut registry = HashMap::new();
        registry.insert("tagged".to_string(), agent);
        let r = discover_worker("audit", &registry).unwrap();
        assert_eq!(r.name, "tagged");
    }

    #[test]
    fn load_agent_registry_includes_local_and_installed() {
        let tmp = std::env::temp_dir().join(format!("lev-fanout-reg-{}", std::process::id()));
        let agent_dir = tmp.join("installed-worker");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("agent.leviath"),
            r#"
[agent]
name = "installed-worker"
version = "0.1.0"
description = "an installed worker"

[stages.main]
model = { models = [{ provider = "mock", model = "m" }] }
"#,
        )
        .unwrap();

        let installer = AgentInstaller::with_install_dir(tmp.clone());
        let local = bp("local-root");
        let registry = load_agent_registry_with(&local, &installer);

        assert!(registry.contains_key("local-root"));
        assert!(registry.contains_key("installed-worker"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
