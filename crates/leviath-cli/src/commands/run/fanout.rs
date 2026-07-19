//! Fan-out worker resolution: turn a [`FanOutConfig`] into a concrete worker
//! agent type (blueprint) + entry stage.
//!
//! Sub-agent workers are ordinary agents that just have a parent, so each runs
//! from a *registered* blueprint. This module builds the registry (the local
//! blueprint plus every installed agent under `~/.leviath/agents`) and resolves
//! the three worker sources: a named installed agent (`worker_agent`), a stage
//! in the current blueprint (`worker_stage`), or discovery over installed
//! agents (`worker_query`).

use std::collections::HashMap;
use std::sync::Arc;

use bevy_ecs::prelude::Entity;
use leviath_core::Blueprint;
use leviath_core::blueprint::{FanOutConfig, ModelConfig, WorkerFailurePolicy};
use leviath_package::AgentInstaller;
use leviath_runtime::{
    AgentPool, AgentState, AgentStatus, ContextWindow, EngineHandle, ParentRef, ToolResultsFuture,
    run_inference_loop_shared,
};

use super::io::RunIO;
use super::manifest::parse_manifest;
use super::tool_source::StageToolSource;
use crate::tools::spawn_child_agent;

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
    // `list_installed` only errors on a filesystem race; treat that as "no
    // installed agents" (the same as an empty/absent install dir).
    for agent in installer.list_installed().unwrap_or_default() {
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
        if let Some(val) = bp.metadata.get(key)
            && metadata_value_contains(val, needle)
        {
            return true;
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

/// What a fan-out stage decided after its workers finished.
#[derive(Debug, PartialEq, Eq)]
pub enum FanOutOutcome {
    /// Jump into the named merge stage to reconcile results.
    Merge(String),
    /// Transition normally (no merge stage configured).
    Proceed,
    /// A worker failed under `on_worker_failure = fail_all`; route the error edge.
    FailAll,
}

/// One unit of work produced by the splitter prompt.
#[derive(serde::Deserialize)]
struct WorkItem {
    #[serde(default)]
    id: String,
    #[serde(default)]
    context: serde_json::Value,
}

/// Parse the splitter's output into work items. Tolerates markdown fences and
/// surrounding prose by extracting the outermost `[ ... ]`.
fn parse_work_items(content: &str) -> anyhow::Result<Vec<WorkItem>> {
    let trimmed = content.trim();
    let slice = match (trimmed.find('['), trimmed.rfind(']')) {
        (Some(s), Some(e)) if e > s => &trimmed[s..=e],
        _ => return Err(anyhow::anyhow!("split output is not a JSON array")),
    };
    serde_json::from_str(slice)
        .map_err(|e| anyhow::anyhow!("split output is not a valid JSON array of work items: {e}"))
}

/// Whether a tool is an interactive prompt (unavailable inside a fan-out worker).
fn is_interaction_tool(name: &str) -> bool {
    matches!(
        name,
        "present_for_review" | "ask_user_text" | "ask_user_choice" | "ask_user_confirm"
    )
}

/// Filter the full tool set down to a stage's `available_tools` allowlist.
fn filter_tools(source: &dyn StageToolSource, filter: &[String]) -> Vec<leviath_providers::Tool> {
    if filter.is_empty() {
        return Vec::new();
    }
    source
        .all_tool_defs()
        .into_iter()
        .filter(|t| filter.iter().any(|f| f == &t.name))
        .collect()
}

/// Resolve a worker stage's model against the registered providers, falling back
/// to the fan-out stage's own (provider, model) when none is available.
async fn resolve_worker_model(
    engine: &EngineHandle,
    model: &ModelConfig,
    fallback: (&str, &str),
) -> (String, String) {
    let eng = engine.read().await;
    for entry in &model.models {
        if eng.providers().has(&entry.provider) {
            return (entry.provider.clone(), entry.model.clone());
        }
    }
    (fallback.0.to_string(), fallback.1.to_string())
}

/// Inject a consolidated results blob into the parent's conversation so the
/// merge / next stage (which runs on the parent entity) can see it.
async fn inject_results(engine: &EngineHandle, parent_entity: Entity, text: &str) {
    let mut eng = engine.write().await;
    if let Some(mut w) = eng.world_mut().get_mut::<ContextWindow>(parent_entity) {
        let tokens = text.len() / 4 + 1;
        let _ = w.add_typed_entry(
            "conversation",
            leviath_core::EntryKind::UserMessage,
            text.to_string(),
            tokens,
        );
    }
}

/// Run a fan-out stage: split work into items, drive one in-process sub-agent
/// worker per item concurrently (bounded by `max_workers`), then reconcile.
///
/// Returns the [`FanOutOutcome`] the caller uses to decide the transition.
#[allow(clippy::too_many_arguments)]
pub async fn run_fan_out_stage(
    engine: &EngineHandle,
    parent_entity: Entity,
    blueprint: &Blueprint,
    config: &FanOutConfig,
    registry: &HashMap<String, Blueprint>,
    tool_source: &dyn StageToolSource,
    provider_name: &str,
    model_name: &str,
    io: &mut dyn RunIO,
) -> anyhow::Result<FanOutOutcome> {
    // ── 1. Split phase: one inference (no tools) that yields JSON work items ──
    // Append the split prompt to the parent conversation (empty is harmless).
    {
        let mut eng = engine.write().await;
        let mut w = eng
            .world_mut()
            .get_mut::<ContextWindow>(parent_entity)
            .expect("parent entity always has a ContextWindow");
        let tokens = config.split_prompt.len() / 4 + 1;
        let _ = w.add_typed_entry(
            "conversation",
            leviath_core::EntryKind::UserMessage,
            config.split_prompt.clone(),
            tokens,
        );
    }
    let mut noop = |_: Vec<leviath_providers::ToolCall>| -> ToolResultsFuture<'static> {
        Box::pin(async { Vec::new() })
    };
    let split = run_inference_loop_shared(
        engine,
        parent_entity,
        provider_name,
        model_name,
        Vec::new(),
        1,
        None,
        None,
        &mut noop,
        None,
        None,
    )
    .await?;

    let items = match parse_work_items(&split.content) {
        Ok(items) => items,
        Err(e) => {
            io.on_error(&format!("fan_out split failed: {e}")).await;
            return Ok(FanOutOutcome::FailAll);
        }
    };
    if items.is_empty() {
        inject_results(
            engine,
            parent_entity,
            "[fan_out: split produced no work items]",
        )
        .await;
        return Ok(match &config.merge_stage {
            Some(m) => FanOutOutcome::Merge(m.clone()),
            None => FanOutOutcome::Proceed,
        });
    }

    // ── 2. Resolve the worker agent type + entry stage ──
    let worker = resolve_worker(config, blueprint, registry)?;
    let worker_stage = worker
        .blueprint
        .find_stage(&worker.entry_stage)
        .ok_or_else(|| anyhow::anyhow!("worker entry stage '{}' not found", worker.entry_stage))?;
    let worker_max_iter = worker_stage.max_iterations.unwrap_or(20);
    let worker_tools = filter_tools(tool_source, &worker_stage.available_tools);
    // Owned, 'static tool executor cloned into each detached worker task.
    let tool_caller = tool_source.tool_caller();
    let (wp, wm) =
        resolve_worker_model(engine, &worker_stage.model, (provider_name, model_name)).await;

    let (parent_agent_id, parent_depth) = {
        let eng = engine.read().await;
        let aid = eng
            .world()
            .get::<AgentState>(parent_entity)
            .map(|s| s.agent_id.clone())
            .unwrap_or_default();
        let depth = eng
            .world()
            .get::<ParentRef>(parent_entity)
            .map(|p| p.depth)
            .unwrap_or(0);
        (aid, depth)
    };
    let max_depth = blueprint.max_child_depth.unwrap_or(3);

    // ── 3. Spawn one child worker per work item ──
    let mut pool = AgentPool::new(worker.blueprint.clone());
    let mut children: Vec<(String, Entity)> = Vec::new();
    for item in &items {
        let ctx_json = serde_json::to_string(&item.context).unwrap_or_default();
        let seed = format!("Work item id: {}\nContext: {}", item.id, ctx_json);
        let (cid, ce) = spawn_child_agent(
            engine,
            &mut pool,
            parent_entity,
            &parent_agent_id,
            &worker.blueprint,
            &worker.entry_stage,
            parent_depth + 1,
            max_depth,
            Some(&seed),
        )
        .await;
        children.push((cid, ce));
    }

    // ── 4. Drive workers concurrently, bounded by max_workers ──
    let sem = Arc::new(tokio::sync::Semaphore::new(config.max_workers.max(1)));
    let mut set = tokio::task::JoinSet::new();
    for (cid, ce) in children {
        let engine = engine.clone();
        let sem = sem.clone();
        let tool_caller = tool_caller.clone();
        let worker_tools = worker_tools.clone();
        let (wp, wm) = (wp.clone(), wm.clone());
        set.spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore is never closed");

            // Shared context-window copy so context_* tools operate on the
            // worker's own regions; mirrors the root worker's setup. Seed it
            // from the worker entity's current window.
            let shared_cw: Arc<tokio::sync::Mutex<Option<ContextWindow>>> =
                Arc::new(tokio::sync::Mutex::new(
                    engine
                        .read()
                        .await
                        .world()
                        .get::<ContextWindow>(ce)
                        .cloned(),
                ));

            let tool_caller = tool_caller.clone();
            let exec_cw = shared_cw.clone();
            let mut exec =
                move |calls: Vec<leviath_providers::ToolCall>| -> ToolResultsFuture<'static> {
                    let tool_caller = tool_caller.clone();
                    let exec_cw = exec_cw.clone();
                    Box::pin(async move {
                        let mut out = Vec::new();
                        for c in calls {
                            let r = if c.name.starts_with("context_") {
                                super::worker::handle_context_tool(&c.name, &c.arguments, &exec_cw)
                                    .await
                            } else if is_interaction_tool(&c.name) {
                                // Fan-out workers run autonomously — they can't
                                // block on a user prompt. Direct the worker to
                                // proceed; questions surface to the merge/parent
                                // stage (which is interactive).
                                "[note] Interactive prompts are unavailable inside a fan-out \
                                 worker. Proceed autonomously and note any question in your final \
                                 output for the merge stage to resolve."
                                    .to_string()
                            } else {
                                tool_caller.call(&c.name, c.arguments.clone()).await
                            };
                            out.push((c.id, r));
                        }
                        out
                    })
                };

            // Alternating shared<->entity sync so context_* writes reach the
            // entity and the engine's appends reach the shared copy (same
            // protocol the root worker uses).
            let sync_cw = shared_cw.clone();
            let mut to_entity = true;
            let mut post_tool_sync =
                move |world: &mut bevy_ecs::prelude::World, ent: bevy_ecs::prelude::Entity| {
                    // The per-worker lock is only ever taken here and in this
                    // worker's tool executor, which never run concurrently, so
                    // try_lock always succeeds; the window is seeded before the
                    // loop and the worker entity always has a ContextWindow.
                    let mut guard = sync_cw
                        .try_lock()
                        .expect("per-worker sync lock is uncontended");
                    if to_entity {
                        let shared = guard
                            .as_ref()
                            .expect("shared window seeded before the loop");
                        let mut ecw = world
                            .get_mut::<ContextWindow>(ent)
                            .expect("worker entity always has a ContextWindow");
                        ecw.regions = shared.regions.clone();
                        ecw.current_tokens = shared.current_tokens;
                    } else {
                        let ecw = world
                            .get::<ContextWindow>(ent)
                            .expect("worker entity always has a ContextWindow");
                        *guard = Some(ecw.clone());
                    }
                    to_entity = !to_entity;
                };

            let res = run_inference_loop_shared(
                &engine,
                ce,
                &wp,
                &wm,
                worker_tools,
                worker_max_iter,
                None,
                None,
                &mut exec,
                None,
                Some(&mut post_tool_sync),
            )
            .await;
            (cid, ce, res)
        });
    }

    // ── 5. Join, write results back, collect outcomes ──
    let mut summaries: Vec<(String, String)> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    while let Some(joined) = set.join_next().await {
        // A worker task only fails to join if it panicked — that's a bug, so
        // surface it rather than silently swallowing it.
        let (cid, ce, res) = joined.expect("fan-out worker task panicked");
        let mut eng = engine.write().await;
        let mut state = eng
            .world_mut()
            .get_mut::<AgentState>(ce)
            .expect("spawned worker always has AgentState");
        match res {
            Ok(resp) => {
                state.status = AgentStatus::Complete;
                summaries.push((cid, resp.content));
            }
            Err(e) => {
                let msg = e.to_string();
                state.status = AgentStatus::Error {
                    message: msg.clone(),
                };
                failures.push((cid, msg));
            }
        }
    }

    // ── 6. Failure policy ──
    if !failures.is_empty() && config.on_worker_failure == WorkerFailurePolicy::FailAll {
        io.on_error(&format!(
            "fan_out: {} worker(s) failed (on_worker_failure = fail_all)",
            failures.len()
        ))
        .await;
        return Ok(FanOutOutcome::FailAll);
    }

    // ── 7. Consolidate results into the parent, then merge/transition ──
    let mut report = format!(
        "[fan_out results: {} succeeded, {} failed]\n",
        summaries.len(),
        failures.len()
    );
    for (cid, content) in &summaries {
        report.push_str(&format!("\n## worker {cid}\n{content}\n"));
    }
    for (cid, err) in &failures {
        report.push_str(&format!("\n## worker {cid} FAILED\n{err}\n"));
    }
    inject_results(engine, parent_entity, &report).await;

    Ok(match &config.merge_stage {
        Some(m) => FanOutOutcome::Merge(m.clone()),
        None => FanOutOutcome::Proceed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use leviath_core::RegionKind;
    use leviath_core::blueprint::{ModelConfig, Stage, WorkerFailurePolicy};
    use leviath_core::layout::{ContextLayout, RegionDefinition};

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
    fn resolve_worker_query_no_match_propagates_discover_error() {
        // A worker_query that matches nothing makes discover_worker return Err,
        // which resolve_worker propagates via `?` (covers that `?` arm).
        let current = bp("root");
        let mut registry = HashMap::new();
        registry.insert("coder".to_string(), bp("coder"));
        let mut c = cfg();
        c.worker_query = Some("zzz-no-such-agent".into());
        let err = resolve_worker(&c, &current, &registry).unwrap_err();
        assert!(err.to_string().contains("matched no"));
    }

    #[test]
    fn resolve_worker_no_source_errors() {
        // Defensive: validate_graph normally guarantees one source, but a caller
        // that bypasses validation gets a clear error.
        let current = bp("root");
        let err = resolve_worker(&cfg(), &current, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("no worker source"));
    }

    #[test]
    fn discover_worker_zero_and_ambiguous() {
        let mut registry = HashMap::new();
        registry.insert("alpha".to_string(), bp("alpha"));
        registry.insert("alto".to_string(), bp("alto"));
        // zero
        assert!(
            discover_worker("zzz", &registry)
                .unwrap_err()
                .to_string()
                .contains("matched no")
        );
        // ambiguous ("al" matches both), error lists sorted candidates
        let err = discover_worker("al", &registry).unwrap_err().to_string();
        assert!(err.contains("ambiguous"));
        assert!(err.contains("alpha, alto"));
    }

    #[test]
    fn parse_work_items_handles_prose_and_rejects_bad_json() {
        // No array at all.
        assert!(parse_work_items("just prose, no array").is_err());
        // Brackets present but invalid JSON inside.
        assert!(parse_work_items("[this is not json]").is_err());
        // Prose around a valid array is tolerated.
        let items =
            parse_work_items("Here you go:\n[{\"id\":\"x\",\"context\":{\"k\":1}}]\nThanks")
                .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "x");
    }

    #[test]
    fn discover_matches_capabilities_and_ignores_nonstring_metadata() {
        let mut agent = bp("cap-agent");
        agent
            .metadata
            .insert("capabilities".to_string(), serde_json::json!(["refactor"]));
        // Non-string/array metadata is ignored, not matched.
        agent
            .metadata
            .insert("tags".to_string(), serde_json::json!(42));
        let mut registry = HashMap::new();
        registry.insert("cap-agent".to_string(), agent);
        assert_eq!(
            discover_worker("refactor", &registry).unwrap().name,
            "cap-agent"
        );
        assert!(discover_worker("42", &registry).is_err());
    }

    #[test]
    fn load_agent_registry_skips_malformed_installed_agent() {
        let tmp = std::env::temp_dir().join(format!("lev-fanout-bad-{}", std::process::id()));
        let bad = tmp.join("broken");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("agent.leviath"), "= = = not valid toml {{{").unwrap();
        let installer = AgentInstaller::with_install_dir(tmp.clone());
        let registry = load_agent_registry_with(&bp("root"), &installer);
        assert!(registry.contains_key("root"));
        assert!(
            !registry.contains_key("broken"),
            "malformed agent is skipped"
        );
        let _ = std::fs::remove_dir_all(&tmp);
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

    // ─── run_fan_out_stage (end-to-end) ──────────────────────────────────────

    use leviath_core::Region;
    use leviath_runtime::{AgentEngine, ProviderRegistry, SubAgentChildren};

    type Scripted = (String, Vec<leviath_providers::ToolCall>);

    /// One scriptable provider for all fan-out tests: each `infer` pops the next
    /// scripted `(content, tool_calls)`; once the script is exhausted it returns
    /// a no-tool "done" (so workers finish) — or errors, if `error_when_empty`.
    /// The first `infer` in a fan-out run is the split; the rest are workers.
    struct ScriptedProvider {
        script: std::sync::Mutex<std::collections::VecDeque<Scripted>>,
        error_when_empty: bool,
    }
    impl ScriptedProvider {
        /// Split returns `json`, then workers get "done".
        fn split(json: &str) -> Self {
            Self {
                script: std::sync::Mutex::new(vec![(json.to_string(), vec![])].into()),
                error_when_empty: false,
            }
        }
        /// Split returns `json`, then every worker inference errors.
        fn split_then_error(json: &str) -> Self {
            Self {
                script: std::sync::Mutex::new(vec![(json.to_string(), vec![])].into()),
                error_when_empty: true,
            }
        }
        /// Fully scripted responses (split first, then per-worker turns).
        fn scripted(items: Vec<Scripted>) -> Self {
            Self {
                script: std::sync::Mutex::new(items.into()),
                error_when_empty: false,
            }
        }
    }
    #[async_trait::async_trait]
    impl leviath_providers::Provider for ScriptedProvider {
        async fn infer(
            &self,
            _req: leviath_providers::InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            let next = self.script.lock().unwrap().pop_front();
            let (content, tool_calls) = match next {
                Some(r) => r,
                None if self.error_when_empty => {
                    return Err(leviath_providers::ProviderError::Other(
                        "worker boom".into(),
                    ));
                }
                None => ("done".to_string(), vec![]),
            };
            Ok(leviath_providers::InferenceResponse {
                content,
                tool_calls,
                tokens_used: leviath_providers::TokenUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                finish_reason: leviath_providers::FinishReason::Complete,
            })
        }
        fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            4
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "mock"
        }
        fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    /// Build a `context_write` / `ask_user_text` etc. tool call for scripting.
    fn tool_call(name: &str, args: serde_json::Value) -> leviath_providers::ToolCall {
        leviath_providers::ToolCall {
            id: "c1".to_string(),
            name: name.to_string(),
            arguments: args,
        }
    }

    /// Blueprint with a fan_out stage (worker_stage=worker) + merge stage.
    fn fanout_blueprint(config: FanOutConfig) -> Blueprint {
        use leviath_core::RegionKind;
        use leviath_core::layout::{ContextLayout, RegionDefinition};
        let layout = ContextLayout::new(
            vec![RegionDefinition::new(
                "sys".into(),
                RegionKind::Pinned,
                2000,
            )],
            12000,
        );
        let mut fan = leviath_core::blueprint::Stage::new(
            "parallel".into(),
            ModelConfig::new("mock".into(), "m".into()),
        );
        fan.mode = leviath_core::blueprint::StageMode::FanOut { config };
        let mut worker = leviath_core::blueprint::Stage::new(
            "worker".into(),
            ModelConfig::new("mock".into(), "m".into()),
        );
        worker.allow_as_worker = true;
        let merge = leviath_core::blueprint::Stage::new(
            "merge".into(),
            ModelConfig::new("mock".into(), "m".into()),
        );
        Blueprint::new("t".into(), "d".into(), vec![fan, worker, merge], layout)
    }

    fn base_config() -> FanOutConfig {
        FanOutConfig {
            worker_agent: None,
            worker_stage: Some("worker".into()),
            worker_query: None,
            merge_stage: Some("merge".into()),
            max_workers: 2,
            on_worker_failure: WorkerFailurePolicy::Continue,
            split_prompt: "split the work".into(),
        }
    }

    /// Build an engine (with the scripted provider) + a parent entity that has a
    /// conversation region, returned as a shared handle.
    async fn setup(split: &str) -> (EngineHandle, Entity, Arc<ToolRegistry>) {
        setup_with(ScriptedProvider::split(split)).await
    }

    /// Like [`setup`] but with a caller-provided scripted provider.
    async fn setup_with(provider: ScriptedProvider) -> (EngineHandle, Entity, Arc<ToolRegistry>) {
        let mut registry = ProviderRegistry::new();
        registry.register("mock".into(), Arc::new(provider));
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(fanout_blueprint(base_config()));
        let pid = pool.spawn_agent(engine.world_mut());
        let parent = pool.get_agent(&pid).unwrap();
        {
            let mut w = engine.world_mut().get_mut::<ContextWindow>(parent).unwrap();
            w.add_region(Region::new(
                "conversation".into(),
                leviath_core::RegionKind::SlidingWindow {
                    max_items: 100,
                    eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                },
                10000,
            ));
        }
        let engine: EngineHandle = Arc::new(tokio::sync::RwLock::new(engine));
        let config = crate::config::Config::default();
        let workdir = std::env::current_dir().unwrap();
        let tools = Arc::new(ToolRegistry::build(workdir, &config).await);
        (engine, parent, tools)
    }

    #[tokio::test]
    async fn resolve_worker_model_uses_registered_else_fallback() {
        // Unregistered provider → fallback.
        let engine: EngineHandle = Arc::new(tokio::sync::RwLock::new(AgentEngine::with_providers(
            ProviderRegistry::new(),
        )));
        let model = ModelConfig::new("nonexistent".into(), "x".into());
        let (p, m) = resolve_worker_model(&engine, &model, ("fb-p", "fb-m")).await;
        assert_eq!((p.as_str(), m.as_str()), ("fb-p", "fb-m"));

        // Registered provider → used.
        let mut reg = ProviderRegistry::new();
        reg.register("mock".into(), Arc::new(ScriptedProvider::split("[]")));
        let engine2: EngineHandle =
            Arc::new(tokio::sync::RwLock::new(AgentEngine::with_providers(reg)));
        let model2 = ModelConfig::new("mock".into(), "the-model".into());
        let (p2, m2) = resolve_worker_model(&engine2, &model2, ("fb", "fb")).await;
        assert_eq!((p2.as_str(), m2.as_str()), ("mock", "the-model"));
    }

    #[tokio::test]
    async fn run_fan_out_spawns_workers_and_merges() {
        let split = r#"[{"id":"a","context":{"x":1}},{"id":"b","context":{"x":2}}]"#;
        let (engine, parent, tools) = setup(split).await;
        let bp = fanout_blueprint(base_config());
        let mut reg = HashMap::new();
        reg.insert(bp.name.clone(), bp.clone());
        let mut io = super::super::io::mock::MockIO::new();

        let outcome = run_fan_out_stage(
            &engine,
            parent,
            &bp,
            &base_config(),
            &reg,
            tools.as_ref(),
            "mock",
            "m",
            &mut io,
        )
        .await
        .unwrap();

        assert_eq!(outcome, FanOutOutcome::Merge("merge".to_string()));

        let eng = engine.read().await;
        let children = eng.world().get::<SubAgentChildren>(parent).unwrap();
        assert_eq!(children.children.len(), 2, "one worker per work item");
        for &c in &children.children {
            assert_eq!(
                eng.world().get::<AgentState>(c).unwrap().status,
                AgentStatus::Complete
            );
        }
        let conv = eng
            .world()
            .get::<ContextWindow>(parent)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .content
            .iter()
            .map(|e| e.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(conv.contains("fan_out results"), "results injected: {conv}");
    }

    #[tokio::test]
    async fn run_fan_out_no_merge_proceeds() {
        let split = r#"[{"id":"a","context":{}}]"#;
        let (engine, parent, tools) = setup(split).await;
        let mut config = base_config();
        config.merge_stage = None;
        let bp = fanout_blueprint(config.clone());
        let mut reg = HashMap::new();
        reg.insert(bp.name.clone(), bp.clone());
        let mut io = super::super::io::mock::MockIO::new();
        let outcome = run_fan_out_stage(
            &engine,
            parent,
            &bp,
            &config,
            &reg,
            tools.as_ref(),
            "mock",
            "m",
            &mut io,
        )
        .await
        .unwrap();
        assert_eq!(outcome, FanOutOutcome::Proceed);
    }

    #[tokio::test]
    async fn run_fan_out_empty_split_no_merge_proceeds() {
        // Empty split + no merge stage → Proceed (covers the None arm).
        let (engine, parent, tools) = setup("[]").await;
        let mut config = base_config();
        config.merge_stage = None;
        let bp = fanout_blueprint(config.clone());
        let mut reg = HashMap::new();
        reg.insert(bp.name.clone(), bp.clone());
        let mut io = super::super::io::mock::MockIO::new();
        let outcome = run_fan_out_stage(
            &engine,
            parent,
            &bp,
            &config,
            &reg,
            tools.as_ref(),
            "mock",
            "m",
            &mut io,
        )
        .await
        .unwrap();
        assert_eq!(outcome, FanOutOutcome::Proceed);
    }

    #[tokio::test]
    async fn run_fan_out_split_inference_error_propagates() {
        // A provider name that isn't registered makes the split inference fail,
        // propagating through `run_inference_loop_shared(...).await?`.
        let (engine, parent, tools) = setup("[]").await;
        let config = base_config();
        let bp = fanout_blueprint(config.clone());
        let mut reg = HashMap::new();
        reg.insert(bp.name.clone(), bp.clone());
        let mut io = super::super::io::mock::MockIO::new();
        let result = run_fan_out_stage(
            &engine,
            parent,
            &bp,
            &config,
            &reg,
            tools.as_ref(),
            "nonexistent-provider",
            "m",
            &mut io,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn run_fan_out_resolve_worker_error_propagates() {
        // The split succeeds, but the worker source (a query) matches nothing,
        // so resolve_worker returns Err — propagated via its `?`.
        let (engine, parent, tools) = setup(r#"[{"id":"a","context":{}}]"#).await;
        let mut config = base_config();
        config.worker_stage = None;
        config.worker_query = Some("no-such-worker-agent".into());
        let bp = fanout_blueprint(config.clone());
        // Empty registry → the query matches nothing.
        let reg = HashMap::new();
        let mut io = super::super::io::mock::MockIO::new();
        let result = run_fan_out_stage(
            &engine,
            parent,
            &bp,
            &config,
            &reg,
            tools.as_ref(),
            "mock",
            "m",
            &mut io,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn inject_results_no_context_window_is_noop() {
        // A parent entity with no ContextWindow exercises the None arm of
        // inject_results' `if let Some(mut w)` (nothing injected, no panic).
        let mut engine = AgentEngine::with_providers(ProviderRegistry::new());
        let entity = engine.world_mut().spawn(()).id();
        let engine: EngineHandle = Arc::new(tokio::sync::RwLock::new(engine));
        inject_results(&engine, entity, "orphan text").await;
    }

    #[tokio::test]
    async fn run_fan_out_worker_uses_builtin_tool() {
        // A worker that calls a normal builtin tool routes through ToolRegistry.
        let (engine, parent, tools) = setup_with(ScriptedProvider::scripted(vec![
            (r#"[{"id":"a","context":{}}]"#.to_string(), vec![]),
            (
                "listing".to_string(),
                vec![tool_call("list_dir", serde_json::json!({"path":"."}))],
            ),
        ]))
        .await;
        let mut fo = base_config();
        fo.max_workers = 1;
        let mut bp = fanout_blueprint(fo.clone());
        bp.stages
            .iter_mut()
            .find(|s| s.name == "worker")
            .expect("fanout_blueprint always defines a worker stage")
            .available_tools = vec!["list_dir".to_string()];
        let mut reg = HashMap::new();
        reg.insert(bp.name.clone(), bp.clone());
        let mut io = super::super::io::mock::MockIO::new();
        let outcome = run_fan_out_stage(
            &engine,
            parent,
            &bp,
            &fo,
            &reg,
            tools.as_ref(),
            "mock",
            "m",
            &mut io,
        )
        .await
        .unwrap();
        assert_eq!(outcome, FanOutOutcome::Merge("merge".to_string()));
        let eng = engine.read().await;
        let child = eng
            .world()
            .get::<SubAgentChildren>(parent)
            .unwrap()
            .children[0];
        assert_eq!(
            eng.world().get::<AgentState>(child).unwrap().status,
            AgentStatus::Complete
        );
    }

    #[tokio::test]
    async fn run_fan_out_continue_reports_worker_failures() {
        // Worker fails, but on_worker_failure = continue → merge runs and the
        // consolidated report includes the failure (covers the failures loop).
        let (engine, parent, tools) = setup_with(ScriptedProvider::split_then_error(
            r#"[{"id":"a","context":{}}]"#,
        ))
        .await;
        let mut fo = base_config();
        fo.max_workers = 1;
        fo.on_worker_failure = WorkerFailurePolicy::Continue;
        let bp = fanout_blueprint(fo.clone());
        let mut reg = HashMap::new();
        reg.insert(bp.name.clone(), bp.clone());
        let mut io = super::super::io::mock::MockIO::new();
        let outcome = run_fan_out_stage(
            &engine,
            parent,
            &bp,
            &fo,
            &reg,
            tools.as_ref(),
            "mock",
            "m",
            &mut io,
        )
        .await
        .unwrap();
        assert_eq!(outcome, FanOutOutcome::Merge("merge".to_string()));
        // The consolidated report (in the parent conversation) notes the failure.
        let eng = engine.read().await;
        let conv = eng
            .world()
            .get::<ContextWindow>(parent)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .content
            .iter()
            .map(|e| e.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            conv.contains("FAILED"),
            "report should note the failure: {conv}"
        );
    }

    #[tokio::test]
    async fn run_fan_out_invalid_split_fails() {
        let (engine, parent, tools) = setup("not json at all").await;
        let bp = fanout_blueprint(base_config());
        let reg = HashMap::new();
        let mut io = super::super::io::mock::MockIO::new();
        let outcome = run_fan_out_stage(
            &engine,
            parent,
            &bp,
            &base_config(),
            &reg,
            tools.as_ref(),
            "mock",
            "m",
            &mut io,
        )
        .await
        .unwrap();
        assert_eq!(outcome, FanOutOutcome::FailAll);
    }

    #[tokio::test]
    async fn run_fan_out_fail_all_routes_error() {
        // Build an engine whose workers error, with on_worker_failure = fail_all.
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".into(),
            Arc::new(ScriptedProvider::split_then_error(
                r#"[{"id":"a","context":{}}]"#,
            )),
        );
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(fanout_blueprint(base_config()));
        let pid = pool.spawn_agent(engine.world_mut());
        let parent = pool.get_agent(&pid).unwrap();
        {
            let mut w = engine.world_mut().get_mut::<ContextWindow>(parent).unwrap();
            w.add_region(Region::new(
                "conversation".into(),
                leviath_core::RegionKind::SlidingWindow {
                    max_items: 100,
                    eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                },
                10000,
            ));
        }
        let engine: EngineHandle = Arc::new(tokio::sync::RwLock::new(engine));
        let config = crate::config::Config::default();
        let tools = Arc::new(ToolRegistry::build(std::env::current_dir().unwrap(), &config).await);

        let mut fo = base_config();
        fo.on_worker_failure = WorkerFailurePolicy::FailAll;
        let bp = fanout_blueprint(fo.clone());
        let mut reg = HashMap::new();
        reg.insert(bp.name.clone(), bp.clone());
        let mut io = super::super::io::mock::MockIO::new();

        let outcome = run_fan_out_stage(
            &engine,
            parent,
            &bp,
            &fo,
            &reg,
            tools.as_ref(),
            "mock",
            "m",
            &mut io,
        )
        .await
        .unwrap();
        assert_eq!(outcome, FanOutOutcome::FailAll);
        // The worker was marked Error.
        let eng = engine.read().await;
        let children = eng.world().get::<SubAgentChildren>(parent).unwrap();
        let status = &eng
            .world()
            .get::<AgentState>(children.children[0])
            .unwrap()
            .status;
        assert_eq!(
            std::mem::discriminant(status),
            std::mem::discriminant(&AgentStatus::Error {
                message: String::new()
            }),
            "worker should be in the Error state, got {status:?}"
        );
    }

    #[tokio::test]
    async fn run_fan_out_worker_can_use_context_tools() {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".into(),
            Arc::new(ScriptedProvider::scripted(vec![
                (r#"[{"id":"a","context":{}}]"#.to_string(), vec![]),
                (
                    "writing".to_string(),
                    vec![tool_call(
                        "context_write",
                        serde_json::json!({"region":"sys","content":"WORKER WROTE THIS"}),
                    )],
                ),
            ])),
        );
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(fanout_blueprint(base_config()));
        let pid = pool.spawn_agent(engine.world_mut());
        let parent = pool.get_agent(&pid).unwrap();
        {
            let mut w = engine.world_mut().get_mut::<ContextWindow>(parent).unwrap();
            w.add_region(Region::new(
                "conversation".into(),
                leviath_core::RegionKind::SlidingWindow {
                    max_items: 100,
                    eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                },
                10000,
            ));
        }
        let engine: EngineHandle = Arc::new(tokio::sync::RwLock::new(engine));
        let config = crate::config::Config::default();
        let tools = Arc::new(ToolRegistry::build(std::env::current_dir().unwrap(), &config).await);

        // Worker stage allows context_write.
        let mut fo = base_config();
        fo.max_workers = 1;
        let mut bp = fanout_blueprint(fo.clone());
        bp.stages
            .iter_mut()
            .find(|s| s.name == "worker")
            .expect("fanout_blueprint always defines a worker stage")
            .available_tools = vec!["context_write".to_string()];
        let mut reg = HashMap::new();
        reg.insert(bp.name.clone(), bp.clone());
        let mut io = super::super::io::mock::MockIO::new();

        run_fan_out_stage(
            &engine,
            parent,
            &bp,
            &fo,
            &reg,
            tools.as_ref(),
            "mock",
            "m",
            &mut io,
        )
        .await
        .unwrap();

        // The worker's context_write landed on its own entity's "sys" region.
        let eng = engine.read().await;
        let child = eng
            .world()
            .get::<SubAgentChildren>(parent)
            .unwrap()
            .children[0];
        let sys = eng
            .world()
            .get::<ContextWindow>(child)
            .unwrap()
            .get_region("sys")
            .unwrap()
            .content
            .iter()
            .map(|e| e.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(sys.contains("WORKER WROTE THIS"), "sys region: {sys}");
    }

    #[tokio::test]
    async fn run_fan_out_worker_interaction_tool_gets_directive() {
        // A worker that calls ask_user_text receives the autonomous directive as
        // the tool result (rather than blocking), then proceeds to completion.
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".into(),
            Arc::new(ScriptedProvider::scripted(vec![
                (r#"[{"id":"a","context":{}}]"#.to_string(), vec![]),
                (
                    "asking".to_string(),
                    vec![tool_call(
                        "ask_user_text",
                        serde_json::json!({"prompt":"which approach?"}),
                    )],
                ),
            ])),
        );
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(fanout_blueprint(base_config()));
        let pid = pool.spawn_agent(engine.world_mut());
        let parent = pool.get_agent(&pid).unwrap();
        {
            let mut w = engine.world_mut().get_mut::<ContextWindow>(parent).unwrap();
            w.add_region(Region::new(
                "conversation".into(),
                leviath_core::RegionKind::SlidingWindow {
                    max_items: 100,
                    eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                },
                10000,
            ));
        }
        let engine: EngineHandle = Arc::new(tokio::sync::RwLock::new(engine));
        let config = crate::config::Config::default();
        let tools = Arc::new(ToolRegistry::build(std::env::current_dir().unwrap(), &config).await);

        let mut fo = base_config();
        fo.max_workers = 1;
        let mut bp = fanout_blueprint(fo.clone());
        bp.stages
            .iter_mut()
            .find(|s| s.name == "worker")
            .expect("fanout_blueprint always defines a worker stage")
            .available_tools = vec!["ask_user_text".to_string()];
        let mut reg = HashMap::new();
        reg.insert(bp.name.clone(), bp.clone());
        let mut io = super::super::io::mock::MockIO::new();

        let outcome = run_fan_out_stage(
            &engine,
            parent,
            &bp,
            &fo,
            &reg,
            tools.as_ref(),
            "mock",
            "m",
            &mut io,
        )
        .await
        .unwrap();
        // Worker didn't block; the stage completed to its merge outcome.
        assert_eq!(outcome, FanOutOutcome::Merge("merge".to_string()));
        let eng = engine.read().await;
        let child = eng
            .world()
            .get::<SubAgentChildren>(parent)
            .unwrap()
            .children[0];
        assert_eq!(
            eng.world().get::<AgentState>(child).unwrap().status,
            AgentStatus::Complete
        );
    }

    #[test]
    fn interaction_tools_are_recognized() {
        assert!(is_interaction_tool("ask_user_text"));
        assert!(is_interaction_tool("ask_user_choice"));
        assert!(is_interaction_tool("ask_user_confirm"));
        assert!(is_interaction_tool("present_for_review"));
        assert!(!is_interaction_tool("read_file"));
        assert!(!is_interaction_tool("context_write"));
    }

    #[tokio::test]
    async fn run_fan_out_empty_split_skips_to_merge() {
        let (engine, parent, tools) = setup("[]").await;
        let bp = fanout_blueprint(base_config());
        let reg = HashMap::new();
        let mut io = super::super::io::mock::MockIO::new();
        let outcome = run_fan_out_stage(
            &engine,
            parent,
            &bp,
            &base_config(),
            &reg,
            tools.as_ref(),
            "mock",
            "m",
            &mut io,
        )
        .await
        .unwrap();
        assert_eq!(outcome, FanOutOutcome::Merge("merge".to_string()));
        // No workers spawned.
        assert!(
            engine
                .read()
                .await
                .world()
                .get::<SubAgentChildren>(parent)
                .is_none()
        );
    }

    #[tokio::test]
    async fn run_fan_out_split_response_with_tool_call_runs_executor() {
        // The split inference returns a tool call alongside the JSON array,
        // exercising the split's (otherwise-unused) no-op tool executor.
        let (engine, parent, tools) = setup_with(ScriptedProvider::scripted(vec![(
            r#"[{"id":"a","context":{}}]"#.to_string(),
            vec![tool_call("noop", serde_json::json!({}))],
        )]))
        .await;
        let bp = fanout_blueprint(base_config());
        let mut reg = HashMap::new();
        reg.insert(bp.name.clone(), bp.clone());
        let mut io = super::super::io::mock::MockIO::new();
        let outcome = run_fan_out_stage(
            &engine,
            parent,
            &bp,
            &base_config(),
            &reg,
            tools.as_ref(),
            "mock",
            "m",
            &mut io,
        )
        .await
        .unwrap();
        assert_eq!(outcome, FanOutOutcome::Merge("merge".to_string()));
    }

    #[test]
    fn scripted_provider_trait_surface() {
        // Exercise the Provider trait methods the inference path doesn't call.
        use leviath_providers::Provider;
        let p = ScriptedProvider::split("[]");
        assert_eq!(p.count_tokens("abcd", "m"), 4);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        assert_eq!(p.name(), "mock");
        let _ = p.capabilities("m");
    }

    #[test]
    fn load_agent_registry_uses_default_installer() {
        // Exercise the default-installer wrapper by pointing LEVIATH_HOME at an
        // empty temp dir (no installed agents), so only the local blueprint
        // registers.
        let tmp = std::env::temp_dir().join(format!("lev-fanout-home-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let registry = temp_env::with_var("LEVIATH_HOME", Some(&tmp), || {
            load_agent_registry(&bp("solo-agent"))
        });
        assert!(registry.contains_key("solo-agent"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
