//! Fan-out stage handling as ECS systems.
//!
//! A `fan_out` stage (see [`leviath_core::blueprint::StageMode::FanOut`]) runs
//! its single inference as a **split** — its prompt (with the config's
//! `split_prompt` folded in by [`crate::pipeline`]) asks the model for a JSON
//! array of work items. [`fan_out_split`] intercepts that response (before the
//! normal `process_response` routing), parses the items, and parks the parent in
//! [`FanOutWaiting`]. [`fan_out_collect`] then starts one worker per item —
//! bounded by `max_workers` concurrent workers — via the daemon-installed
//! [`FanOutSpawner`], tracks them as the parent's `SubAgentChildren`, and once
//! every worker is terminal applies the failure policy, injects a consolidated
//! report into the parent's conversation, and transitions to the `merge_stage`
//! (or falls through to the stage's normal transition).
//!
//! The runtime only **starts and tracks** workers; resolving *which* blueprint a
//! worker runs (self-at-worker-stage, a named agent, or a capability query) is
//! the CLI's job, encapsulated behind the [`FanOutSpawner`] it installs.

use std::collections::VecDeque;
use std::sync::Arc;

use bevy_ecs::prelude::*;
use leviath_core::blueprint::{FanOutConfig, StageMode, WorkerFailurePolicy};

use crate::components::{
    AgentState, AgentStatus, ContextWindow, InferenceResult, ParentRef, SubAgentChildren,
};
use crate::pipeline::{AgentBlueprint, ProcessResponse, ResolveTransition, StageCursor};

/// Depth cap for fan-out workers when the parent's blueprint doesn't set one.
const DEFAULT_FANOUT_DEPTH: usize = 3;

/// One unit of work produced by a fan-out split.
#[derive(serde::Deserialize, Debug, Clone, Default)]
pub struct WorkItem {
    /// Stable id (used to label the worker in the consolidated report).
    #[serde(default)]
    pub id: String,
    /// Free-form context handed to the worker (seeded into its pinned context).
    #[serde(default)]
    pub context: serde_json::Value,
}

/// Parse a split response into work items. Tolerates markdown fences and prose by
/// extracting the outermost `[ … ]`. (Ported from the deleted imperative engine.)
pub fn parse_work_items(content: &str) -> Result<Vec<WorkItem>, String> {
    let trimmed = content.trim();
    let slice = match (trimmed.find('['), trimmed.rfind(']')) {
        (Some(s), Some(e)) if e > s => &trimmed[s..=e],
        _ => return Err("split output is not a JSON array".to_string()),
    };
    serde_json::from_str(slice)
        .map_err(|e| format!("split output is not a valid JSON array of work items: {e}"))
}

/// Starts one worker for a fan-out work item. The implementor resolves the
/// worker's blueprint (per `config`'s `worker_stage` / `worker_agent` /
/// `worker_query`), spawns it into `world` seeded with the work item, and returns
/// the child entity. Parent/child linking is done by [`fan_out_collect`], not the
/// spawner.
pub trait FanOutSpawner: Send + Sync {
    /// Spawn one worker under `parent` for the given work item, or `Err` with a
    /// human-readable reason (recorded as that item's failure).
    fn spawn_worker(
        &self,
        world: &mut World,
        parent: Entity,
        config: &FanOutConfig,
        item_id: &str,
        item_context: &serde_json::Value,
    ) -> Result<Entity, String>;
}

/// The installed [`FanOutSpawner`], as a world resource. Absent in a pure-runtime
/// world (then every fan-out item fails with "no fan-out spawner installed").
#[derive(Resource, Clone)]
pub struct FanOutSpawnerRes(pub Arc<dyn FanOutSpawner>);

/// A parent parked while its fan-out workers run. Holds the not-yet-started
/// `pending` items, the currently-`active` `(item_id, worker)` pairs, and the
/// accumulated worker results.
#[derive(Component)]
pub struct FanOutWaiting {
    config: FanOutConfig,
    max_workers: usize,
    pending: VecDeque<WorkItem>,
    active: Vec<(String, Entity)>,
    summaries: Vec<(String, String)>,
    failures: Vec<(String, String)>,
}

/// Fan-out split system (exclusive): for each `ProcessResponse` agent whose
/// current stage is a fan-out stage, consume its response as the split output —
/// parse the work items and park the agent in [`FanOutWaiting`] (or mark it
/// `Error` if the split output isn't a JSON array). Removing `ProcessResponse`
/// here keeps the normal `process_response` routing from touching these agents.
pub fn fan_out_split(world: &mut World) {
    let mut candidates: Vec<(Entity, String, FanOutConfig)> = Vec::new();
    {
        let mut q = world.query_filtered::<(
            Entity,
            &AgentState,
            &AgentBlueprint,
            &StageCursor,
            &InferenceResult,
        ), With<ProcessResponse>>();
        for (entity, state, bp, cursor, infer) in q.iter(world) {
            if state.status != AgentStatus::Active {
                continue;
            }
            if let StageMode::FanOut { config } = &bp.0.stages[cursor.index].mode {
                candidates.push((entity, infer.response.clone(), config.clone()));
            }
        }
    }

    for (parent, response, config) in candidates {
        world
            .entity_mut(parent)
            .remove::<ProcessResponse>()
            .remove::<InferenceResult>();
        match parse_work_items(&response) {
            Ok(items) => {
                let max_workers = config.max_workers.max(1);
                world.entity_mut(parent).insert(FanOutWaiting {
                    config,
                    max_workers,
                    pending: items.into_iter().collect(),
                    active: Vec::new(),
                    summaries: Vec::new(),
                    failures: Vec::new(),
                });
                set_status(world, parent, AgentStatus::Waiting);
            }
            Err(message) => {
                set_status(
                    world,
                    parent,
                    AgentStatus::Error {
                        message: format!("fan_out split failed: {message}"),
                    },
                );
            }
        }
    }
}

/// Fan-out collect system (exclusive): drive each [`FanOutWaiting`] parent — reap
/// finished workers, start pending ones up to `max_workers`, and once none remain
/// running apply the failure policy, inject the consolidated report, and
/// transition to the merge stage (or resolve the stage's own transition).
pub fn fan_out_collect(world: &mut World) {
    let parents: Vec<Entity> = {
        let mut q = world.query_filtered::<Entity, With<FanOutWaiting>>();
        q.iter(world).collect()
    };

    for parent in parents {
        // A cancelled/errored parent abandons the fan-out; its workers are reaped
        // by the host's cascade cancel (which walks SubAgentChildren).
        if !matches!(agent_status(world, parent), Some(AgentStatus::Waiting)) {
            world.entity_mut(parent).remove::<FanOutWaiting>();
            continue;
        }
        // A `Waiting` parent from the query above still holds its `FanOutWaiting`
        // (only this system removes it, and each entity appears once per pass).
        let mut w = world
            .entity_mut(parent)
            .take::<FanOutWaiting>()
            .expect("a Waiting fan-out parent still holds FanOutWaiting");

        // 1. Reap workers that have reached a terminal state.
        let mut still_active = Vec::with_capacity(w.active.len());
        for (id, worker) in std::mem::take(&mut w.active) {
            match worker_terminal_result(world, worker) {
                Some(Ok(content)) => w.summaries.push((id, content)),
                Some(Err(message)) => w.failures.push((id, message)),
                None => still_active.push((id, worker)),
            }
        }
        w.active = still_active;

        // 2. Start pending workers up to the concurrency cap.
        while w.active.len() < w.max_workers {
            let Some(item) = w.pending.pop_front() else {
                break;
            };
            match start_worker(world, parent, &w.config, &item) {
                Ok(child) => w.active.push((item.id, child)),
                Err(message) => w.failures.push((item.id, message)),
            }
        }

        // 3. Finished when nothing is running or queued.
        if w.active.is_empty() && w.pending.is_empty() {
            finish_fan_out(world, parent, w);
        } else {
            world.entity_mut(parent).insert(w);
        }
    }
}

/// Apply the failure policy, inject the consolidated report, and transition.
fn finish_fan_out(world: &mut World, parent: Entity, w: FanOutWaiting) {
    if !w.failures.is_empty() && w.config.on_worker_failure == WorkerFailurePolicy::FailAll {
        set_status(
            world,
            parent,
            AgentStatus::Error {
                message: format!(
                    "fan_out: {} worker(s) failed (on_worker_failure = fail_all)",
                    w.failures.len()
                ),
            },
        );
        return;
    }

    let report = build_report(&w.summaries, &w.failures);
    inject_conversation(world, parent, &report);

    // Ready the parent to run again, then jump to the merge stage (if any) or let
    // the fan-out stage's own transition resolve.
    set_status(world, parent, AgentStatus::Active);
    match w.config.merge_stage.as_deref().and_then(|name| {
        world
            .get::<AgentBlueprint>(parent)
            .and_then(|bp| bp.0.stages.iter().position(|s| s.name == name))
    }) {
        Some(idx) => crate::pipeline::force_transition(world, parent, idx),
        None => {
            world.entity_mut(parent).insert(ResolveTransition);
        }
    }
}

/// Start one worker and link it to `parent` (`ParentRef` + `SubAgentChildren`),
/// enforcing the parent blueprint's child-depth cap. Returns the child entity.
fn start_worker(
    world: &mut World,
    parent: Entity,
    config: &FanOutConfig,
    item: &WorkItem,
) -> Result<Entity, String> {
    let max_depth = world
        .get::<SubAgentChildren>(parent)
        .map(|k| k.max_child_depth)
        .or_else(|| {
            world
                .get::<AgentBlueprint>(parent)
                .and_then(|bp| bp.0.max_child_depth)
        })
        .unwrap_or(DEFAULT_FANOUT_DEPTH);
    let parent_depth = world.get::<ParentRef>(parent).map_or(0, |p| p.depth);
    let child_depth = parent_depth + 1;
    if child_depth > max_depth {
        return Err(format!(
            "fan-out worker depth limit ({max_depth}) reached; not spawning"
        ));
    }

    let spawner = world
        .get_resource::<FanOutSpawnerRes>()
        .map(|r| r.0.clone())
        .ok_or_else(|| "no fan-out spawner installed".to_string())?;
    let child = spawner.spawn_worker(world, parent, config, &item.id, &item.context)?;

    let parent_agent_id = world
        .get::<AgentState>(parent)
        .map(|s| s.agent_id.clone())
        .unwrap_or_default();
    world.entity_mut(child).insert(ParentRef {
        parent_entity: parent,
        parent_agent_id,
        depth: child_depth,
    });
    match world.get_mut::<SubAgentChildren>(parent) {
        Some(mut kids) => kids.children.push(child),
        None => {
            world.entity_mut(parent).insert(SubAgentChildren {
                children: vec![child],
                max_child_depth: max_depth,
            });
        }
    }
    Ok(child)
}

/// A worker's terminal result: `Some(Ok(final_text))` if complete,
/// `Some(Err(reason))` if it errored/was cancelled/vanished, `None` if still
/// running.
fn worker_terminal_result(world: &World, worker: Entity) -> Option<Result<String, String>> {
    match agent_status(world, worker) {
        None => Some(Err("worker vanished".to_string())),
        Some(AgentStatus::Complete) => {
            let content = world
                .get::<InferenceResult>(worker)
                .map(|r| r.response.clone())
                .unwrap_or_default();
            Some(Ok(content))
        }
        Some(AgentStatus::Error { message }) => Some(Err(message)),
        Some(AgentStatus::Cancelled) => Some(Err("worker cancelled".to_string())),
        Some(_) => None,
    }
}

/// Build the consolidated `[fan_out results: …]` report from worker outcomes.
fn build_report(summaries: &[(String, String)], failures: &[(String, String)]) -> String {
    let mut report = format!(
        "[fan_out results: {} succeeded, {} failed]\n",
        summaries.len(),
        failures.len()
    );
    for (id, content) in summaries {
        report.push_str(&format!("\n## worker {id}\n{content}\n"));
    }
    for (id, err) in failures {
        report.push_str(&format!("\n## worker {id} FAILED\n{err}\n"));
    }
    report
}

/// Add `text` to the parent's `conversation` region (best-effort).
fn inject_conversation(world: &mut World, parent: Entity, text: &str) {
    if let Some(mut window) = world.get_mut::<ContextWindow>(parent) {
        let tokens = text.len() / 4 + 1;
        let _ = window.add_typed_entry(
            "conversation",
            leviath_core::EntryKind::UserMessage,
            text.to_string(),
            tokens,
        );
    }
}

/// An agent's status, if it still exists.
fn agent_status(world: &World, entity: Entity) -> Option<AgentStatus> {
    world.get::<AgentState>(entity).map(|s| s.status.clone())
}

/// Set an agent's status (no-op if it despawned).
fn set_status(world: &mut World, entity: Entity, status: AgentStatus) {
    if let Some(mut state) = world.get_mut::<AgentState>(entity) {
        state.status = status;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{InferenceConfig, ToolResultRoutingComponent};
    use crate::pipeline::{
        ReadyToInfer, StageInference, StageInferences, StageProgress, StageSetup, StageSetups,
        VisitCounts,
    };
    use leviath_core::blueprint::{ModelConfig, Stage};
    use leviath_core::layout::{ContextLayout, RegionDefinition};
    use leviath_core::{Blueprint, Region, RegionKind};
    use std::collections::HashSet;

    /// A spawner that spawns a trivial `Active` worker per item, refusing the ids
    /// in `fail`.
    struct TestSpawner {
        fail: HashSet<String>,
    }

    impl TestSpawner {
        fn ok() -> Arc<dyn FanOutSpawner> {
            Arc::new(TestSpawner {
                fail: HashSet::new(),
            })
        }
        fn refusing(ids: &[&str]) -> Arc<dyn FanOutSpawner> {
            Arc::new(TestSpawner {
                fail: ids.iter().map(|s| s.to_string()).collect(),
            })
        }
    }

    impl FanOutSpawner for TestSpawner {
        fn spawn_worker(
            &self,
            world: &mut World,
            _parent: Entity,
            _config: &FanOutConfig,
            item_id: &str,
            _item_context: &serde_json::Value,
        ) -> Result<Entity, String> {
            if self.fail.contains(item_id) {
                return Err(format!("spawn refused for '{item_id}'"));
            }
            Ok(world
                .spawn(AgentState {
                    agent_id: format!("worker-{item_id}"),
                    current_stage: "w".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: vec![],
                    pending_wait: None,
                    accepts_messages: true,
                })
                .id())
        }
    }

    fn cfg(merge: Option<&str>, max_workers: usize, policy: WorkerFailurePolicy) -> FanOutConfig {
        FanOutConfig {
            worker_agent: None,
            worker_stage: Some("w".to_string()),
            worker_query: None,
            merge_stage: merge.map(String::from),
            max_workers,
            on_worker_failure: policy,
            split_prompt: "split".to_string(),
        }
    }

    fn window() -> ContextWindow {
        let mut w = ContextWindow::new(12_000);
        w.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        ));
        w
    }

    fn stage_inf() -> StageInference {
        StageInference {
            provider_name: "script".to_string(),
            model: "m".to_string(),
            tools: vec![],
            tool_filter: None,
        }
    }

    fn setup() -> StageSetup {
        StageSetup {
            inference_config: InferenceConfig {
                temperature: None,
                max_output_tokens: None,
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            system_prompt: None,
        }
    }

    /// A blueprint whose stage 0 is a fan-out stage and stage 1 is `merge`.
    fn fanout_blueprint(config: FanOutConfig) -> Blueprint {
        let layout = ContextLayout::new(
            vec![RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::Clearable,
                10_000,
            )],
            12_000,
        );
        let mut s0 = Stage::new(
            "fan".to_string(),
            ModelConfig::new("script".to_string(), "m".to_string()),
        );
        s0.mode = StageMode::FanOut { config };
        let s1 = Stage::new(
            "merge".to_string(),
            ModelConfig::new("script".to_string(), "m".to_string()),
        );
        Blueprint::new("t".to_string(), "d".to_string(), vec![s0, s1], layout)
    }

    fn parent_state() -> AgentState {
        AgentState {
            agent_id: "parent".to_string(),
            current_stage: "fan".to_string(),
            iteration: 0,
            status: AgentStatus::Active,
            spawned_children_ids: vec![],
            pending_wait: None,
            accepts_messages: true,
        }
    }

    /// Spawn a parent sitting on `ProcessResponse` with `response` as its
    /// (split) inference output.
    fn spawn_parent(world: &mut World, bp: Blueprint, response: &str) -> Entity {
        world
            .spawn((
                AgentBlueprint(bp),
                StageCursor { index: 0 },
                parent_state(),
                StageProgress::default(),
                StageInferences(vec![stage_inf(), stage_inf()]),
                StageSetups(vec![setup(), setup()]),
                VisitCounts::default(),
                window(),
                InferenceResult {
                    response: response.to_string(),
                    tool_calls: vec![],
                    tokens_used: 0,
                    timestamp: 0,
                },
                ProcessResponse,
            ))
            .id()
    }

    fn install(world: &mut World, spawner: Arc<dyn FanOutSpawner>) {
        world.insert_resource(FanOutSpawnerRes(spawner));
    }

    fn status_of(world: &World, e: Entity) -> AgentStatus {
        world.get::<AgentState>(e).unwrap().status.clone()
    }

    /// Assert an agent is in an `Error` state (by discriminant, so no unmatched
    /// `matches!` arm is left uncovered).
    fn assert_errored(world: &World, e: Entity) {
        assert_eq!(
            std::mem::discriminant(&status_of(world, e)),
            std::mem::discriminant(&AgentStatus::Error {
                message: String::new()
            })
        );
    }

    fn complete_worker(world: &mut World, worker: Entity, content: &str) {
        set_status(world, worker, AgentStatus::Complete);
        world.entity_mut(worker).insert(InferenceResult {
            response: content.to_string(),
            tool_calls: vec![],
            tokens_used: 0,
            timestamp: 0,
        });
    }

    // ── parse_work_items ──────────────────────────────────────────────────────

    #[test]
    fn parse_work_items_handles_array_prose_and_errors() {
        let ok = parse_work_items(r#"[{"id":"a"},{"id":"b","context":{"k":1}}]"#).unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[0].id, "a");
        assert_eq!(ok[1].context["k"], 1);
        // Missing fields default.
        assert_eq!(parse_work_items("[{}]").unwrap()[0].id, "");
        // Prose around the array is tolerated.
        assert_eq!(
            parse_work_items("Here you go:\n```json\n[{\"id\":\"x\"}]\n```")
                .unwrap()
                .len(),
            1
        );
        // No brackets at all.
        assert!(parse_work_items("no array here").is_err());
        // Closing before opening (e <= s).
        assert!(parse_work_items("]nope[").is_err());
        // Brackets but not valid JSON.
        assert!(parse_work_items("[not json]").is_err());
    }

    // ── fan_out_split ─────────────────────────────────────────────────────────

    #[test]
    fn split_parks_a_fanout_stage_and_consumes_the_response() {
        let mut world = World::new();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailurePolicy::Continue)),
            r#"[{"id":"a"},{"id":"b"}]"#,
        );
        fan_out_split(&mut world);
        assert!(world.get::<FanOutWaiting>(e).is_some());
        assert_eq!(status_of(&world, e), AgentStatus::Waiting);
        // ProcessResponse + InferenceResult were consumed.
        assert!(world.get::<ProcessResponse>(e).is_none());
        assert!(world.get::<InferenceResult>(e).is_none());
        let w = world.get::<FanOutWaiting>(e).unwrap();
        assert_eq!(w.pending.len(), 2);
    }

    #[test]
    fn split_errors_on_non_array_output() {
        let mut world = World::new();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailurePolicy::Continue)),
            "definitely not a json array",
        );
        fan_out_split(&mut world);
        assert!(world.get::<FanOutWaiting>(e).is_none());
        assert_errored(&world, e);
    }

    #[test]
    fn split_skips_non_active_and_non_fanout_agents() {
        // Non-Active fan-out agent: left untouched.
        let mut world = World::new();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailurePolicy::Continue)),
            "[]",
        );
        set_status(&mut world, e, AgentStatus::Idle);
        fan_out_split(&mut world);
        assert!(world.get::<ProcessResponse>(e).is_some());
        assert!(world.get::<FanOutWaiting>(e).is_none());

        // Non-fan-out stage: not a candidate at all.
        let layout = ContextLayout::new(
            vec![RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::Clearable,
                10_000,
            )],
            12_000,
        );
        let s = Stage::new(
            "plain".to_string(),
            ModelConfig::new("script".to_string(), "m".to_string()),
        );
        let bp = Blueprint::new("t".to_string(), "d".to_string(), vec![s], layout);
        let e2 = spawn_parent(&mut world, bp, "[]");
        fan_out_split(&mut world);
        assert!(world.get::<ProcessResponse>(e2).is_some());
    }

    // ── fan_out_collect: worker lifecycle + merge ─────────────────────────────

    #[test]
    fn collect_starts_workers_then_merges_on_completion() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailurePolicy::Continue)),
            r#"[{"id":"a"},{"id":"b"}]"#,
        );
        fan_out_split(&mut world);
        fan_out_collect(&mut world);
        // Two workers started and tracked.
        let kids = world.get::<SubAgentChildren>(e).unwrap().children.clone();
        assert_eq!(kids.len(), 2);
        assert!(world.get::<FanOutWaiting>(e).is_some());
        // Each worker got a ParentRef at depth 1.
        for k in &kids {
            assert_eq!(world.get::<ParentRef>(*k).unwrap().depth, 1);
        }

        // Complete both workers, then collect merges to the merge stage.
        for k in &kids {
            complete_worker(&mut world, *k, "fixed it");
        }
        fan_out_collect(&mut world);
        assert!(world.get::<FanOutWaiting>(e).is_none());
        assert_eq!(status_of(&world, e), AgentStatus::Active);
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
        assert!(world.get::<ReadyToInfer>(e).is_some());
        // The consolidated report landed in the parent's conversation.
        assert!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens
                > 0
        );
    }

    #[test]
    fn collect_respects_max_workers_and_stages_pending() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 1, WorkerFailurePolicy::Continue)),
            r#"[{"id":"a"},{"id":"b"}]"#,
        );
        fan_out_split(&mut world);
        fan_out_collect(&mut world);
        // Only one worker at a time.
        assert_eq!(world.get::<SubAgentChildren>(e).unwrap().children.len(), 1);
        let first = world.get::<SubAgentChildren>(e).unwrap().children[0];
        // A collect pass while the worker is still running keeps it active and
        // starts nothing new (worker still counts against max_workers).
        fan_out_collect(&mut world);
        assert_eq!(world.get::<SubAgentChildren>(e).unwrap().children.len(), 1);
        assert!(world.get::<FanOutWaiting>(e).is_some());
        complete_worker(&mut world, first, "one");
        fan_out_collect(&mut world);
        // Second worker started after the first finished.
        assert_eq!(world.get::<SubAgentChildren>(e).unwrap().children.len(), 2);
        let second = world.get::<SubAgentChildren>(e).unwrap().children[1];
        complete_worker(&mut world, second, "two");
        fan_out_collect(&mut world);
        assert!(world.get::<FanOutWaiting>(e).is_none());
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    }

    #[test]
    fn collect_fail_all_marks_parent_error() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailurePolicy::FailAll)),
            r#"[{"id":"a"}]"#,
        );
        fan_out_split(&mut world);
        fan_out_collect(&mut world);
        let worker = world.get::<SubAgentChildren>(e).unwrap().children[0];
        set_status(
            &mut world,
            worker,
            AgentStatus::Error {
                message: "boom".to_string(),
            },
        );
        fan_out_collect(&mut world);
        assert_errored(&world, e);
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 0); // no merge
    }

    #[test]
    fn collect_continue_reports_failures_and_proceeds_without_merge() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        // No merge stage ⇒ ResolveTransition (proceed) rather than force_transition.
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailurePolicy::Continue)),
            r#"[{"id":"a"},{"id":"b"}]"#,
        );
        fan_out_split(&mut world);
        fan_out_collect(&mut world);
        let kids = world.get::<SubAgentChildren>(e).unwrap().children.clone();
        set_status(
            &mut world,
            kids[0],
            AgentStatus::Error {
                message: "worker a died".to_string(),
            },
        );
        complete_worker(&mut world, kids[1], "b ok");
        fan_out_collect(&mut world);
        assert!(world.get::<FanOutWaiting>(e).is_none());
        assert!(world.get::<crate::pipeline::ResolveTransition>(e).is_some());
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 0);
    }

    #[test]
    fn collect_finishes_immediately_when_there_are_no_work_items() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailurePolicy::Continue)),
            "[]",
        );
        fan_out_split(&mut world);
        fan_out_collect(&mut world);
        // No workers; straight to merge.
        assert!(world.get::<SubAgentChildren>(e).is_none());
        assert!(world.get::<FanOutWaiting>(e).is_none());
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    }

    #[test]
    fn collect_merge_stage_not_found_falls_through_to_transition() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("ghost"), 2, WorkerFailurePolicy::Continue)),
            "[]",
        );
        fan_out_split(&mut world);
        fan_out_collect(&mut world);
        // Unknown merge stage ⇒ ResolveTransition, no stage jump.
        assert!(world.get::<crate::pipeline::ResolveTransition>(e).is_some());
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 0);
    }

    #[test]
    fn collect_abandons_a_cancelled_parent() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailurePolicy::Continue)),
            r#"[{"id":"a"}]"#,
        );
        fan_out_split(&mut world);
        set_status(&mut world, e, AgentStatus::Cancelled);
        fan_out_collect(&mut world);
        assert!(world.get::<FanOutWaiting>(e).is_none());
        assert_eq!(status_of(&world, e), AgentStatus::Cancelled);
    }

    #[test]
    fn collect_without_a_spawner_records_failures() {
        // No FanOutSpawnerRes installed ⇒ every item fails to start.
        let mut world = World::new();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailurePolicy::Continue)),
            r#"[{"id":"a"}]"#,
        );
        fan_out_split(&mut world);
        fan_out_collect(&mut world);
        // Item failed to start, Continue policy ⇒ still transitions to merge.
        assert!(world.get::<FanOutWaiting>(e).is_none());
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    }

    #[test]
    fn collect_spawner_error_becomes_a_failure() {
        let mut world = World::new();
        install(&mut world, TestSpawner::refusing(&["a"]));
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailurePolicy::FailAll)),
            r#"[{"id":"a"}]"#,
        );
        fan_out_split(&mut world);
        fan_out_collect(&mut world);
        // Spawn refused + FailAll ⇒ parent errors.
        assert_errored(&world, e);
    }

    // ── start_worker: depth cap + existing SubAgentChildren ───────────────────

    #[test]
    fn start_worker_enforces_depth_cap() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let mut bp = fanout_blueprint(cfg(None, 2, WorkerFailurePolicy::Continue));
        bp.max_child_depth = Some(3);
        let e = spawn_parent(&mut world, bp, r#"[{"id":"deep"}]"#);
        // Parent is itself a depth-3 sub-agent ⇒ child would be depth 4 > 3.
        world.entity_mut(e).insert(ParentRef {
            parent_entity: Entity::from_raw(999),
            parent_agent_id: "root".to_string(),
            depth: 3,
        });
        fan_out_split(&mut world);
        fan_out_collect(&mut world);
        // No worker spawned (depth cap hit before any container is created).
        assert!(world.get::<SubAgentChildren>(e).is_none());
        assert!(world.get::<FanOutWaiting>(e).is_none());
    }

    #[test]
    fn start_worker_uses_existing_subagentchildren_cap_and_appends() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailurePolicy::Continue)),
            r#"[{"id":"a"}]"#,
        );
        // Pre-existing children container with a generous cap.
        world.entity_mut(e).insert(SubAgentChildren {
            children: vec![Entity::from_raw(1000)],
            max_child_depth: 9,
        });
        fan_out_split(&mut world);
        fan_out_collect(&mut world);
        let kids = world.get::<SubAgentChildren>(e).unwrap();
        assert_eq!(kids.max_child_depth, 9);
        assert_eq!(kids.children.len(), 2); // appended to the existing one
    }

    // ── worker_terminal_result / build_report / inject_conversation ───────────

    #[test]
    fn worker_terminal_result_covers_every_status() {
        let mut world = World::new();
        let complete = world
            .spawn((
                parent_state(),
                InferenceResult {
                    response: "done text".to_string(),
                    tool_calls: vec![],
                    tokens_used: 0,
                    timestamp: 0,
                },
            ))
            .id();
        set_status(&mut world, complete, AgentStatus::Complete);
        assert_eq!(
            worker_terminal_result(&world, complete),
            Some(Ok("done text".to_string()))
        );

        let complete_no_infer = world.spawn(parent_state()).id();
        set_status(&mut world, complete_no_infer, AgentStatus::Complete);
        assert_eq!(
            worker_terminal_result(&world, complete_no_infer),
            Some(Ok(String::new()))
        );

        let errored = world.spawn(parent_state()).id();
        set_status(
            &mut world,
            errored,
            AgentStatus::Error {
                message: "x".to_string(),
            },
        );
        assert_eq!(
            worker_terminal_result(&world, errored),
            Some(Err("x".to_string()))
        );

        let cancelled = world.spawn(parent_state()).id();
        set_status(&mut world, cancelled, AgentStatus::Cancelled);
        assert!(worker_terminal_result(&world, cancelled).is_some_and(|r| r.is_err()));

        let running = world.spawn(parent_state()).id(); // Active
        assert_eq!(worker_terminal_result(&world, running), None);

        assert!(worker_terminal_result(&world, Entity::from_raw(4242)).is_some_and(|r| r.is_err()));
    }

    #[test]
    fn build_report_lists_successes_and_failures() {
        let report = build_report(
            &[("a".to_string(), "ok-a".to_string())],
            &[("b".to_string(), "boom".to_string())],
        );
        assert!(report.contains("1 succeeded, 1 failed"));
        assert!(report.contains("## worker a\nok-a"));
        assert!(report.contains("## worker b FAILED\nboom"));
    }

    #[test]
    fn inject_conversation_is_a_noop_without_a_window() {
        let mut world = World::new();
        let has_window = world.spawn(window()).id();
        inject_conversation(&mut world, has_window, "hello");
        assert!(
            world
                .get::<ContextWindow>(has_window)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens
                > 0
        );
        // Entity without a ContextWindow: silently ignored.
        let no_window = world.spawn(parent_state()).id();
        inject_conversation(&mut world, no_window, "hello");
    }

    #[test]
    fn set_status_is_a_noop_for_a_missing_agent() {
        let mut world = World::new();
        set_status(&mut world, Entity::from_raw(77), AgentStatus::Complete);
        assert_eq!(agent_status(&world, Entity::from_raw(77)), None);
    }

    // ── force_transition (pipeline helper) edge cases via fan-out ─────────────

    #[test]
    fn force_transition_applies_routing_and_handles_despawn_and_overflow() {
        use crate::pipeline::force_transition;
        // Routing present on the target stage ⇒ ToolResultRoutingComponent added.
        let mut world = World::new();
        let mut setups = vec![setup(), setup()];
        setups[1].routing = Some(leviath_core::ToolResultRouting::default());
        let e = world
            .spawn((
                AgentBlueprint(fanout_blueprint(cfg(
                    Some("merge"),
                    2,
                    WorkerFailurePolicy::Continue,
                ))),
                StageCursor { index: 0 },
                parent_state(),
                StageProgress::default(),
                StageInferences(vec![stage_inf(), stage_inf()]),
                StageSetups(setups),
                VisitCounts::default(),
                window(),
            ))
            .id();
        force_transition(&mut world, e, 1);
        assert!(world.get::<ToolResultRoutingComponent>(e).is_some());
        assert!(world.get::<ReadyToInfer>(e).is_some());

        // Despawned entity: no panic, no effect.
        force_transition(&mut world, Entity::from_raw(9191), 1);
    }

    #[test]
    fn force_transition_marks_error_on_prompt_overflow() {
        use crate::pipeline::force_transition;
        // A tiny pinned region + a huge stage system prompt ⇒ overflow on entry.
        let layout = ContextLayout::new(
            vec![RegionDefinition::new(
                "task".to_string(),
                RegionKind::Pinned,
                20,
            )],
            1000,
        );
        let mut s0 = Stage::new(
            "fan".to_string(),
            ModelConfig::new("script".to_string(), "m".to_string()),
        );
        s0.mode = StageMode::FanOut {
            config: cfg(Some("merge"), 2, WorkerFailurePolicy::Continue),
        };
        let mut s1 = Stage::new(
            "merge".to_string(),
            ModelConfig::new("script".to_string(), "m".to_string()),
        );
        s1.config.insert(
            "system_prompt".to_string(),
            serde_json::Value::String("x".repeat(10_000)),
        );
        let bp = Blueprint::new("t".to_string(), "d".to_string(), vec![s0, s1], layout);

        let mut setups = vec![setup(), setup()];
        setups[1].system_prompt = Some("x".repeat(10_000));
        let mut w = ContextWindow::new(1000);
        w.add_region(Region::new("task".to_string(), RegionKind::Pinned, 20));
        let (mut world, e) = world_with(bp, setups, w);
        force_transition(&mut world, e, 1);
        assert_errored(&world, e);
    }

    /// Build a world with one agent carrying the given blueprint/setups/window.
    fn world_with(bp: Blueprint, setups: Vec<StageSetup>, w: ContextWindow) -> (World, Entity) {
        let mut world = World::new();
        let e = world
            .spawn((
                AgentBlueprint(bp),
                StageCursor { index: 0 },
                parent_state(),
                StageProgress::default(),
                StageInferences(vec![stage_inf(), stage_inf()]),
                StageSetups(setups),
                VisitCounts::default(),
                w,
            ))
            .id();
        (world, e)
    }
}
