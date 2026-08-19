//! Fan-out stage handling as ECS systems.
//!
//! A `fan_out` stage (see [`leviath_core::blueprint::StageMode::FanOut`]) runs
//! its single inference as a **split** - its prompt (with the config's
//! `split_prompt` folded in by [`crate::pipeline`]) asks the model for a JSON
//! array of work items. [`fan_out_split`] intercepts that response (before the
//! normal `process_response` routing), parses the items, and parks the parent in
//! [`FanOutWaiting`]. [`fan_out_collect`] then starts one worker per item -
//! bounded by `max_workers` concurrent workers - via the daemon-installed
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

/// How many times a malformed split is sent back to the model before the run
/// fails.
///
/// A split asks for one exact shape, and a model that answers with prose or an
/// apology has not failed at the work, only at the format. Failing the run on
/// the first such answer throws away everything the parent has done, which is
/// what a deep-researcher run reported: one non-conforming response ended it.
/// Two corrections is enough to clear a formatting slip without letting a model
/// that cannot produce the shape loop for ever.
const MAX_SPLIT_RETRIES: usize = 2;

/// How many corrective attempts a parent's split has already had.
///
/// Absent until the first malformed split, so a split that parses first time
/// costs nothing.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SplitAttempts(pub usize);

/// What the model is told after a split that could not be parsed.
///
/// It names the failure and restates the shape rather than repeating the
/// original instruction, because the original instruction is what just did not
/// work.
fn split_correction(reason: &str) -> String {
    format!(
        "Your previous response could not be used: {reason}. Reply with the JSON \
         array of work items and nothing else - no prose before or after it, no \
         markdown fences, no explanation. It must start with `[` and end with `]`."
    )
}

/// The first `MAX_SPLIT_SNIPPET` characters of what the model actually said,
/// for the failure message.
///
/// The old message named the rule that was broken but never what came back, so
/// an operator reading `split output is not a JSON array` could not tell a
/// refusal from an empty response from prose. Bounded because a split response
/// can be long, and truncated on a character boundary because model output is
/// arbitrary UTF-8.
fn response_snippet(response: &str) -> String {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        return "the response was empty".to_string();
    }
    // Taken as characters rather than bytes: a byte ceiling can land inside a
    // character, and model output is arbitrary UTF-8.
    let kept: String = trimmed.chars().take(MAX_SPLIT_SNIPPET).collect();
    match kept.len() < trimmed.len() {
        true => format!("the response began: {kept}…"),
        false => format!("the response was: {kept}"),
    }
}

/// How much of a failed split response the error message carries, in characters.
const MAX_SPLIT_SNIPPET: usize = 200;

/// One unit of work produced by a fan-out split.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
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
    // Every rejection folds into one error: this parses model output, so
    // "malformed input yields `Err`" has to hold for every shape of malformed.
    let slice = match (trimmed.find('['), trimmed.rfind(']')) {
        (Some(s), Some(e)) if e > s => trimmed.get(s..=e),
        _ => None,
    }
    .ok_or_else(|| "split output is not a JSON array".to_string())?;
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

/// A currently-running fan-out worker: its work-item id, its live entity, and
/// its run-id (kept so the waiting state can be persisted/restored without a
/// cross-entity lookup - see [`FanOutState`]).
struct ActiveWorker {
    item_id: String,
    entity: Entity,
    run_id: String,
}

/// A parent parked while its fan-out workers run. Holds the not-yet-started
/// `pending` items, the currently-`active` workers, and the accumulated results.
#[derive(Component)]
pub struct FanOutWaiting {
    config: FanOutConfig,
    max_workers: usize,
    pending: VecDeque<WorkItem>,
    active: Vec<ActiveWorker>,
    summaries: Vec<(String, String)>,
    failures: Vec<(String, String)>,
}

/// The serializable form of [`FanOutWaiting`], written to `<run_dir>/fanout.json`
/// so a parent interrupted mid-split resumes its merge after a restart. `active`
/// carries worker **run-ids** (not entities); recovery maps them back to the
/// reloaded worker entities.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FanOutState {
    /// The fan-out configuration.
    pub config: FanOutConfig,
    /// The concurrency cap; `usize::MAX` for a stage with `max_workers = 0`.
    pub max_workers: usize,
    /// Work items not yet started.
    pub pending: Vec<WorkItem>,
    /// In-flight workers as `(item_id, run_id)`.
    pub active: Vec<(String, String)>,
    /// Completed worker results as `(item_id, summary)`.
    pub summaries: Vec<(String, String)>,
    /// Failed worker results as `(item_id, message)`.
    pub failures: Vec<(String, String)>,
}

impl FanOutWaiting {
    /// Workers this parent is still parked on: in-flight plus not-yet-started.
    ///
    /// Surfaced by `lev ps` so "waiting" on a fan-out parent reads as progress
    /// against a known denominator rather than an unexplained stall.
    pub fn outstanding(&self) -> usize {
        self.active.len() + self.pending.len()
    }

    /// Project to the serializable [`FanOutState`] (workers by run-id).
    pub(crate) fn to_state(&self) -> FanOutState {
        FanOutState {
            config: self.config.clone(),
            max_workers: self.max_workers,
            pending: self.pending.iter().cloned().collect(),
            active: self
                .active
                .iter()
                .map(|w| (w.item_id.clone(), w.run_id.clone()))
                .collect(),
            summaries: self.summaries.clone(),
            failures: self.failures.clone(),
        }
    }
}

/// Rebuild a parent's [`FanOutWaiting`] from a persisted [`FanOutState`] and
/// insert it, mapping each active worker's run-id back to its reloaded entity
/// via `resolve`. Workers whose entity didn't reload are treated as failures so
/// the merge still completes rather than waiting forever. Used by restart
/// recovery to resume an interrupted fan-out.
pub fn restore_fan_out_waiting(
    world: &mut World,
    parent: Entity,
    state: FanOutState,
    resolve: &dyn Fn(&str) -> Option<Entity>,
) {
    let mut active = Vec::new();
    let mut failures = state.failures;
    for (item_id, run_id) in state.active {
        match resolve(&run_id) {
            Some(entity) => active.push(ActiveWorker {
                item_id,
                entity,
                run_id,
            }),
            None => failures.push((item_id, "worker did not reload after restart".to_string())),
        }
    }
    world.entity_mut(parent).insert(FanOutWaiting {
        config: state.config,
        max_workers: state.max_workers,
        pending: state.pending.into_iter().collect(),
        active,
        summaries: state.summaries,
        failures,
    });
}

/// Fan-out split system (exclusive): for each `ProcessResponse` agent whose
/// current stage is a fan-out stage, consume its response as the split output -
/// parse the work items and park the agent in [`FanOutWaiting`] (or mark it
/// `Error` if the split output isn't a JSON array). Removing `ProcessResponse`
/// here keeps the normal `process_response` routing from touching these agents.
pub fn fan_out_split(world: &mut World) {
    crate::tick_scope::clear();
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
        crate::tick_scope::enter(parent);
        world
            .entity_mut(parent)
            .remove::<ProcessResponse>()
            .remove::<InferenceResult>();
        match parse_work_items(&response) {
            Ok(items) => {
                // Unlimited (`max_workers = 0`) is the largest cap there is,
                // rather than a separate flag: the start loop below compares
                // against it and nothing else, and the persisted state keeps
                // the one number it always kept.
                let max_workers = config.worker_cap().unwrap_or(usize::MAX);
                // A split decides its own item count, so without a cap a model
                // that returns five hundred items spawns five hundred runs. The
                // cap also fixes each worker's share of the results region: past
                // some number of ways to divide it, every section is too small
                // to say anything.
                let items = match config.max_items {
                    Some(cap) if items.len() > cap => {
                        tracing::warn!(
                            produced = items.len(),
                            cap,
                            "fan_out split produced more items than max_items; keeping the first"
                        );
                        items.into_iter().take(cap).collect::<Vec<_>>()
                    }
                    _ => items,
                };
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
                // A split that did not parse is a formatting miss, not a dead
                // run: send the model its own answer plus a correction and ask
                // again, the way every other stage handles a response it cannot
                // use. Only once the corrections are spent does the run fail.
                let attempts = world.get::<SplitAttempts>(parent).map_or(0, |a| a.0);
                // Correcting needs somewhere to put the correction, so an agent
                // with no context window falls through to the failure below
                // rather than looping without ever being told anything.
                let corrected = match attempts < MAX_SPLIT_RETRIES {
                    false => false,
                    true => {
                        let mut entity = world.entity_mut(parent);
                        match entity.get_mut::<ContextWindow>() {
                            None => false,
                            Some(mut window) => {
                                // The model sees what it said before the
                                // correction, so "reply with only the array"
                                // has something to correct.
                                let tokens = leviath_core::estimate_tokens(&response);
                                let _ = window.add_typed_entry(
                                    "conversation",
                                    leviath_core::EntryKind::AssistantTurn {
                                        tool_calls: Vec::new(),
                                    },
                                    response.clone(),
                                    tokens,
                                );
                                crate::pipeline::inject_system_nudge(
                                    &mut window,
                                    &split_correction(&message),
                                );
                                true
                            }
                        }
                    }
                };
                if corrected {
                    tracing::warn!(
                        attempt = attempts + 1,
                        max = MAX_SPLIT_RETRIES,
                        error = %message,
                        "fan_out split did not parse; asking the model again"
                    );
                    world
                        .entity_mut(parent)
                        .insert(SplitAttempts(attempts + 1))
                        .insert(crate::pipeline::ReadyToInfer);
                } else {
                    // Name what came back as well as the rule it broke: the
                    // rule alone cannot tell a refusal from an empty response
                    // from prose.
                    set_status(
                        world,
                        parent,
                        AgentStatus::Error {
                            message: format!(
                                "fan_out split failed after {attempts} correction(s): \
                                 {message} ({})",
                                response_snippet(&response)
                            ),
                        },
                    );
                }
            }
        }
    }
}

/// Fan-out collect system (exclusive): drive each [`FanOutWaiting`] parent - reap
/// finished workers, start pending ones up to `max_workers`, and once none remain
/// running apply the failure policy, inject the consolidated report, and
/// transition to the merge stage (or resolve the stage's own transition).
pub fn fan_out_collect(world: &mut World) {
    crate::tick_scope::clear();
    let parents: Vec<Entity> = {
        let mut q = world.query_filtered::<Entity, With<FanOutWaiting>>();
        q.iter(world).collect()
    };

    for parent in parents {
        crate::tick_scope::enter(parent);
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

        // 1. Reap workers that have reached a terminal state. A consumed
        // worker's result now lives in `w.summaries`/`w.failures`, so its heavy
        // components are dead weight - mark it for `slim_merged_workers`, which
        // drops them once the terminal snapshot has reached the persistence
        // lane. The entity itself stays (the host only despawns it when the
        // parent goes terminal), but without its context window: previously
        // every finished fan-out worker kept a full window resident for the
        // whole remainder of the parent's run.
        let mut still_active = Vec::with_capacity(w.active.len());
        for aw in std::mem::take(&mut w.active) {
            match worker_terminal_result(world, aw.entity) {
                Some(result) => {
                    match result {
                        Ok(content) => w.summaries.push((aw.item_id, content)),
                        Err(message) => w.failures.push((aw.item_id, message)),
                    }
                    world.entity_mut(aw.entity).insert(MergedWorker);
                }
                None => still_active.push(aw),
            }
        }
        w.active = still_active;

        // 2. Start pending workers up to the concurrency cap.
        while w.active.len() < w.max_workers {
            let Some(item) = w.pending.pop_front() else {
                break;
            };
            match start_worker(world, parent, &w.config, &item) {
                Ok(child) => {
                    // Capture the worker's run-id so the waiting state persists.
                    let run_id = world
                        .get::<crate::persistence::RunMetadata>(child)
                        .map(|m| m.run_id.clone())
                        .unwrap_or_default();
                    w.active.push(ActiveWorker {
                        item_id: item.id,
                        entity: child,
                        run_id,
                    });
                }
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

/// A fan-out worker whose terminal result the parent has already consumed.
/// Set by [`fan_out_collect`]; consumed by [`slim_merged_workers`].
#[derive(Component)]
pub struct MergedWorker;

/// Drop a merged worker's heavy components once its terminal snapshot has
/// reached the persistence lane.
///
/// Ordering makes this safe on both sides: the marker is only set after the
/// parent consumed the worker's result (so the merge no longer reads the
/// worker), and the watermark gate (`PersistWatermark::persisted_status`)
/// holds the slim back until the terminal state is on its way to disk (so
/// nothing readable is lost - the entity's remaining metadata still identifies
/// the run, and its full final state is in the run dir).
pub fn slim_merged_workers(
    workers: Query<(Entity, &crate::pipeline::PersistWatermark), With<MergedWorker>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, watermark) in workers.iter() {
        crate::tick_scope::enter(entity);
        let terminal_persisted = matches!(
            watermark.persisted_status(),
            Some(
                leviath_core::run_meta::RunStatus::Complete
                    | leviath_core::run_meta::RunStatus::Error
                    | leviath_core::run_meta::RunStatus::Cancelled
            )
        );
        if !terminal_persisted {
            continue; // the terminal snapshot has not been dispatched yet
        }
        commands.entity(entity).remove::<(
            ContextWindow,
            InferenceResult,
            crate::pipeline::StageInferences,
            crate::pipeline::StageSetups,
            AgentBlueprint,
            MergedWorker,
        )>();
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

    // Where the results land, and how much room they have there. A blueprint
    // that names a region of its own gets that region's budget to divide; the
    // default is the conversation region, which is also carrying the message
    // history.
    let region = w
        .config
        .results_region
        .clone()
        .unwrap_or_else(|| "conversation".to_string());
    let budget = world
        .get::<ContextWindow>(parent)
        .and_then(|window| window.get_region(&region).map(|r| r.max_tokens));
    let report = build_report(&w.summaries, &w.failures, budget);
    inject_results(world, parent, &region, &report);

    // Ready the parent to run again, then jump to the merge stage (if any) or let
    // the fan-out stage's own transition resolve.
    set_status(world, parent, AgentStatus::Active);
    match w.config.merge_stage.as_deref().and_then(|name| {
        world
            .get::<AgentBlueprint>(parent)
            .and_then(|bp| bp.0.stages.iter().position(|s| s.name == name))
    }) {
        Some(idx) => crate::pipeline::force_transition(
            world,
            crate::world::AgentId::in_world(world, parent),
            idx,
        ),
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
    // Record the worker's run-id on the parent's serializable state so the tree
    // (fan-out workers included) is persisted for a deterministic restart rebuild.
    // A freshly spawned worker always has run metadata; its parent always has state.
    let worker_id = world
        .get::<crate::persistence::RunMetadata>(child)
        .expect("a fan-out worker always has run metadata")
        .run_id
        .clone();
    world
        .get_mut::<AgentState>(parent)
        .expect("a fan-out parent always has AgentState")
        .spawned_children_ids
        .push(worker_id);
    // Seed the worker's context from the parent per any declared blueprint
    // context transform (when a fan-out worker runs a different blueprint).
    crate::context_transform::apply_context_transforms(
        world,
        crate::world::AgentId::in_world(world, parent),
        crate::world::AgentId::in_world(world, child),
    );
    Ok(child)
}

/// A worker's terminal result: `Some(Ok(deliverable))` if complete,
/// `Some(Err(reason))` if it errored/was cancelled/vanished, `None` if still
/// running.
///
/// A worker that called `submit_output` contributes exactly what it submitted.
/// Otherwise this falls back to the text of its last assistant message, which is
/// what every worker used to contribute and is usually wrong: a worker whose
/// final turn was a tool call has no trailing text, so the merge stage received
/// an empty string, which is silently indistinguishable from a worker that had
/// nothing to say.
///
/// The fallback stays because it costs nothing and an existing blueprint that
/// happens to end on a text turn keeps working. A blueprint that wants the
/// guarantee sets `require_output` on its worker stage.
///
/// A worker whose stage set `require_output` and that finished without one is
/// reported as a **failure**, not as a success with empty content. It reached
/// `Complete` either way - the enforcement loop proceeds rather than stranding
/// the run, and a worker that burns its iterations against a validator it cannot
/// satisfy ends the same way. Counting that as success is how a fan-out reports
/// "10 succeeded, 0 failed" over ten empty sections, which is worse than an
/// error: the merge stage cannot tell an empty answer from a missing one, so it
/// writes a confident merge of nothing.
fn worker_terminal_result(world: &World, worker: Entity) -> Option<Result<String, String>> {
    match agent_status(world, worker) {
        None => Some(Err("worker vanished".to_string())),
        Some(AgentStatus::Complete) => {
            match world
                .get::<crate::persistence::FinalOutput>(worker)
                .map(|o| o.0.content.clone())
            {
                Some(content) => Some(Ok(content)),
                None if worker_requires_output(world, worker) => Some(Err(
                    "worker finished without the final output its stage requires".to_string(),
                )),
                None => Some(Ok(world
                    .get::<InferenceResult>(worker)
                    .map(|r| r.response.clone())
                    .unwrap_or_default())),
            }
        }
        Some(AgentStatus::Error { message }) => Some(Err(message)),
        Some(AgentStatus::Cancelled) => Some(Err("worker cancelled".to_string())),
        Some(_) => None,
    }
}

/// Whether the stage this worker is sitting in demands a final output.
fn worker_requires_output(world: &World, worker: Entity) -> bool {
    let Some(bp) = world.get::<AgentBlueprint>(worker) else {
        return false;
    };
    let Some(cursor) = world.get::<StageCursor>(worker) else {
        return false;
    };
    bp.0.stages
        .get(cursor.index)
        .is_some_and(|s| s.require_output)
}

/// Smallest per-worker share worth writing, in bytes.
///
/// Below this a section says nothing useful, and the honest move is to tell the
/// merge stage that the results are too many to carry rather than hand it a
/// hundred fragments. That is what `max_items` on the fan-out config is for.
const MIN_REPORT_BYTES_PER_WORKER: usize = 200;

/// Per-worker share when the results region's budget cannot be read.
const DEFAULT_REPORT_BYTES_PER_WORKER: usize = 4_000;

/// Marker appended to a worker's section that was cut to fit the report.
const REPORT_TRUNCATION_MARKER: &str =
    "\n[...truncated; read this worker's own run for the full answer]";

/// How many bytes each worker's section may use, given the region's token
/// budget and how many workers there are.
///
/// An equal share, so every worker appears. The first cut at this capped each
/// worker at a fixed size and then trimmed the finished report to fit, which
/// meant the early workers got their full allowance and the late ones were cut
/// off entirely - a hundred-way fan-out where only the first twenty were
/// readable, with nothing saying so.
fn bytes_per_worker(region_budget_tokens: Option<usize>, workers: usize) -> usize {
    let Some(tokens) = region_budget_tokens.filter(|t| *t > 0) else {
        return DEFAULT_REPORT_BYTES_PER_WORKER;
    };
    // The workspace's bytes-over-four estimate, minus a margin for the header
    // and the per-worker `## worker <id>` lines.
    let usable = tokens.saturating_mul(4).saturating_mul(9) / 10;
    (usable / workers.max(1)).max(MIN_REPORT_BYTES_PER_WORKER)
}

/// One worker's contribution, trimmed to `budget` bytes.
fn fit_worker_section(content: &str, budget: usize) -> String {
    if content.len() <= budget {
        return content.to_string();
    }
    let room = budget.saturating_sub(REPORT_TRUNCATION_MARKER.len());
    format!(
        "{}{REPORT_TRUNCATION_MARKER}",
        leviath_core::truncate_at_boundary(content, room)
    )
}

/// Build the consolidated `[fan_out results: …]` report from worker outcomes.
///
/// `region_budget_tokens` is the results region's budget, which the workers'
/// sections divide equally between them.
fn build_report(
    summaries: &[(String, String)],
    failures: &[(String, String)],
    region_budget_tokens: Option<usize>,
) -> String {
    let sections = summaries.len().max(1);
    let budget = bytes_per_worker(region_budget_tokens, sections);
    let mut report = format!(
        "[fan_out results: {} succeeded, {} failed]\n",
        summaries.len(),
        failures.len()
    );
    // Say the share out loud when it is tight, so the merge stage knows it is
    // reading extracts and can go to a worker's own run for the rest.
    if summaries.iter().any(|(_, c)| c.len() > budget) {
        report.push_str(&format!(
            "[each worker's answer is shown up to {budget} characters; \
             read a worker's own run for the whole thing]\n"
        ));
    }
    for (id, content) in summaries {
        report.push_str(&format!(
            "\n## worker {id}\n{}\n",
            fit_worker_section(content, budget)
        ));
    }
    for (id, err) in failures {
        report.push_str(&format!("\n## worker {id} FAILED\n{err}\n"));
    }
    report
}

/// Add `text` to the parent's results region, trimming it to fit.
///
/// The write used to be best-effort in the worst sense: `add_entry` rejects an
/// over-budget entry outright, and the error was discarded, so a report too big
/// for the region left the merge stage with nothing and said nothing about it.
/// Trimming first means the merge always receives *something*, and a report that
/// had to be cut says so where the model will read it.
fn inject_results(world: &mut World, parent: Entity, region: &str, text: &str) {
    let Some(mut window) = world.get_mut::<ContextWindow>(parent) else {
        return;
    };
    // A named region the layout does not declare would silently swallow the
    // whole report, so fall back to the one every agent has. `lev validate`
    // catches the typo before a run gets here.
    let region = match window.get_region(region).is_some() {
        true => region,
        false => {
            tracing::warn!(
                region = %region,
                "fan-out results region is not in this agent's layout; using conversation"
            );
            "conversation"
        }
    };
    let budget = window
        .get_region(region)
        .map(|r| r.max_tokens.saturating_sub(r.current_tokens))
        .unwrap_or(0);
    let allowed = budget.saturating_mul(4);
    let fitted = match text.len() <= allowed {
        true => text.to_string(),
        false => {
            let room = allowed.saturating_sub(REPORT_TRUNCATION_MARKER.len());
            format!(
                "{}{REPORT_TRUNCATION_MARKER}",
                leviath_core::truncate_at_boundary(text, room)
            )
        }
    };
    let tokens = leviath_core::estimate_tokens(&fitted);
    let _ = window.add_typed_entry(region, leviath_core::EntryKind::UserMessage, fitted, tokens);
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
                .spawn((
                    AgentState {
                        agent_id: format!("worker-{item_id}"),
                        current_stage: "w".to_string(),
                        iteration: 0,
                        status: AgentStatus::Active,
                        spawned_children_ids: vec![],
                        pending_wait: None,
                        accepts_messages: true,
                    },
                    // A real worker carries run metadata (attached by build_agent);
                    // mirror that so the parent can record the worker's run-id.
                    crate::persistence::RunMetadata {
                        run_id: format!("run-{item_id}"),
                        agent_name: "worker".to_string(),
                        agent_path: String::new(),
                        task: String::new(),
                        model: None,
                        workdir: String::new(),
                        num_stages: 1,
                        started_at: 0,
                        parent_run_id: None,
                        metadata: std::collections::HashMap::new(),
                        callback_url: None,
                        callback_secret: None,
                        title: None,
                        unattended: false,
                        read_paths: None,
                        output_request: None,
                        model_override: None,
                    },
                ))
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
            results_region: None,
            max_items: None,
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
            fallbacks: Vec::new(),
            output: None,
        }
    }

    fn setup() -> StageSetup {
        StageSetup {
            inference_config: InferenceConfig {
                temperature: None,
                max_output_tokens: None,
                extra_params: Default::default(),
                batch_tool_hint: false,
                shell_hint: false,
                request_timeout_secs: None,
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            system_prompt: None,
            output: None,
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

    /// `max_items` is a ceiling on slices, not just on concurrency. A split that
    /// returns five hundred items would otherwise spawn five hundred runs, and
    /// each worker's share of the results region is the region's budget divided
    /// by how many there are: past some count every section is too small to say
    /// anything.
    #[test]
    fn split_keeps_only_the_first_max_items() {
        let mut world = World::new();
        let mut config = cfg(Some("merge"), 2, WorkerFailurePolicy::Continue);
        config.max_items = Some(3);
        let items: Vec<String> = (0..10).map(|i| format!(r#"{{"id":"w{i}"}}"#)).collect();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(config),
            &format!("[{}]", items.join(",")),
        );

        fan_out_split(&mut world);

        let w = world.get::<FanOutWaiting>(e).expect("parked");
        assert_eq!(w.pending.len(), 3, "kept the cap, not the ten produced");
        let kept: Vec<&str> = w.pending.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(kept, ["w0", "w1", "w2"], "and kept the first of them");
    }

    /// Under the cap nothing is dropped, so a fan-out that sets one does not pay
    /// for it on every ordinary split.
    #[test]
    fn split_keeps_everything_under_the_cap() {
        let mut world = World::new();
        let mut config = cfg(Some("merge"), 2, WorkerFailurePolicy::Continue);
        config.max_items = Some(9);
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(config),
            r#"[{"id":"a"},{"id":"b"}]"#,
        );

        fan_out_split(&mut world);

        assert_eq!(
            world.get::<FanOutWaiting>(e).expect("parked").pending.len(),
            2
        );
    }

    #[test]
    fn split_errors_on_non_array_output() {
        // The corrections have to be spent before the run dies, so this drives
        // the split until they are. Failing on the first answer is the bug the
        // retry exists to fix.
        let mut world = World::new();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailurePolicy::Continue)),
            "definitely not a json array",
        );
        for _ in 0..=MAX_SPLIT_RETRIES {
            fan_out_split(&mut world);
            redrive_split(&mut world, e, "definitely not a json array");
        }
        assert!(world.get::<FanOutWaiting>(e).is_none());
        assert_errored(&world, e);
    }

    /// Put the parent back where a fresh inference would leave it, so the split
    /// can be driven a second and third time without a real provider.
    fn redrive_split(world: &mut World, e: Entity, response: &str) {
        world
            .entity_mut(e)
            .remove::<crate::pipeline::ReadyToInfer>()
            .insert(InferenceResult {
                response: response.to_string(),
                tool_calls: vec![],
                tokens_used: 0,
                timestamp: 0,
            })
            .insert(ProcessResponse);
    }

    fn conversation_text(world: &World, e: Entity) -> String {
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .content
            .iter()
            .map(|entry| entry.content.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The fix for the deep-researcher report: a split that comes back as prose
    /// is a formatting miss, and the run gets to correct it instead of dying.
    #[test]
    fn a_split_that_is_not_an_array_is_corrected_rather_than_fatal() {
        let mut world = World::new();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailurePolicy::Continue)),
            "Sure! I will research these topics for you.",
        );

        fan_out_split(&mut world);

        assert_eq!(status_of(&world, e), AgentStatus::Active, "still running");
        assert_eq!(world.get::<SplitAttempts>(e), Some(&SplitAttempts(1)));
        assert!(
            world.get::<crate::pipeline::ReadyToInfer>(e).is_some(),
            "the parent is queued for another attempt"
        );
        assert!(world.get::<FanOutWaiting>(e).is_none());
        let convo = conversation_text(&world, e);
        assert!(
            convo.contains("Sure! I will research"),
            "the model sees its own answer: {convo}"
        );
        assert!(
            convo.contains("[System]") && convo.contains("start with `[`"),
            "and the correction: {convo}"
        );
    }

    /// A correction that works is the whole point: the second answer parses and
    /// the run carries on into its fan-out.
    #[test]
    fn a_corrected_split_proceeds_to_the_fan_out() {
        let mut world = World::new();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailurePolicy::Continue)),
            "no array here",
        );
        fan_out_split(&mut world);
        redrive_split(&mut world, e, r#"[{"id":"a","context":{}}]"#);

        fan_out_split(&mut world);

        assert!(world.get::<FanOutWaiting>(e).is_some(), "the split took");
        assert_eq!(status_of(&world, e), AgentStatus::Waiting);
    }

    /// Once the corrections are spent the run does fail, and the message names
    /// what came back rather than only the rule it broke.
    #[test]
    fn the_failure_message_quotes_what_the_model_actually_said() {
        let mut world = World::new();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailurePolicy::Continue)),
            "I cannot help with that request.",
        );
        for _ in 0..=MAX_SPLIT_RETRIES {
            fan_out_split(&mut world);
            redrive_split(&mut world, e, "I cannot help with that request.");
        }
        // Read through Debug rather than a pattern: the arm a passing run does
        // not take reads to llvm-cov as an uncovered region.
        let status = format!("{:?}", status_of(&world, e));
        assert!(status.contains("Error"), "{status}");
        assert!(status.contains("I cannot help with that"), "{status}");
        assert!(status.contains("correction(s)"), "{status}");
    }

    /// No context window means nowhere to put a correction, so the run fails on
    /// the first malformed split rather than looping while being told nothing.
    #[test]
    fn a_split_with_nowhere_to_put_a_correction_fails_at_once() {
        let mut world = World::new();
        let e = world
            .spawn((
                AgentBlueprint(fanout_blueprint(cfg(
                    None,
                    2,
                    WorkerFailurePolicy::Continue,
                ))),
                StageCursor { index: 0 },
                parent_state(),
                StageProgress::default(),
                StageInferences(vec![stage_inf(), stage_inf()]),
                StageSetups(vec![setup(), setup()]),
                VisitCounts::default(),
                InferenceResult {
                    response: "not an array".to_string(),
                    tool_calls: vec![],
                    tokens_used: 0,
                    timestamp: 0,
                },
                ProcessResponse,
            ))
            .id();

        fan_out_split(&mut world);

        assert_errored(&world, e);
        assert!(world.get::<SplitAttempts>(e).is_none());
    }

    #[test]
    fn the_snippet_reports_an_empty_response_as_empty() {
        assert_eq!(response_snippet("   \n "), "the response was empty");
    }

    #[test]
    fn the_snippet_quotes_a_short_response_whole() {
        assert_eq!(response_snippet("  nope  "), "the response was: nope");
    }

    #[test]
    fn the_snippet_truncates_a_long_response_on_a_character_boundary() {
        // Three bytes per character on purpose: cutting model output at a byte
        // offset is how a panic gets shipped, so the ceiling counts characters
        // and this proves no character was split.
        let long = "€".repeat(MAX_SPLIT_SNIPPET + 10);
        let snippet = response_snippet(&long);
        assert!(snippet.starts_with("the response began: "), "{snippet}");
        assert!(snippet.ends_with('…'), "{snippet}");
        let kept = snippet
            .trim_start_matches("the response began: ")
            .trim_end_matches('…');
        assert_eq!(kept.chars().count(), MAX_SPLIT_SNIPPET);
        assert!(kept.chars().all(|c| c == '€'), "no character was split");
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

    /// Run the slim system once over `world`.
    fn run_slim(world: &mut World) {
        let mut schedule = bevy_ecs::schedule::Schedule::default();
        schedule.add_systems(slim_merged_workers);
        schedule.run(world);
    }

    /// A merged worker keeps its heavy components until its terminal snapshot
    /// has been dispatched, then sheds them - previously every finished
    /// fan-out worker kept a full context window resident until the parent
    /// went terminal.
    #[test]
    fn merged_workers_are_slimmed_once_their_terminal_state_is_persisted() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailurePolicy::Continue)),
            r#"[{"id":"a"}]"#,
        );
        fan_out_split(&mut world);
        fan_out_collect(&mut world);
        let worker = world.get::<SubAgentChildren>(e).unwrap().children[0];
        // Give the worker a context window so there is something to shed.
        world
            .entity_mut(worker)
            .insert((window(), crate::pipeline::PersistWatermark::default()));
        complete_worker(&mut world, worker, "done");
        fan_out_collect(&mut world);

        // Consumed by the merge and marked - but its terminal snapshot has not
        // been dispatched, so it keeps its state.
        assert!(world.get::<MergedWorker>(worker).is_some());
        run_slim(&mut world);
        assert!(
            world.get::<ContextWindow>(worker).is_some(),
            "unpersisted terminal state stays resident"
        );

        // Stamp the watermark terminal, and the worker sheds its heavy parts.
        let mut wm = crate::pipeline::PersistWatermark::default();
        wm.stamp_status(leviath_core::run_meta::RunStatus::Complete);
        world.entity_mut(worker).insert(wm);
        run_slim(&mut world);
        assert!(world.get::<ContextWindow>(worker).is_none());
        assert!(world.get::<MergedWorker>(worker).is_none());
        // The entity itself survives for the host's bookkeeping.
        assert!(world.get::<AgentState>(worker).is_some());
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

    /// `max_workers = 0` is unlimited: every item starts on the first collect
    /// pass, and the persisted state carries the cap as the largest number
    /// there is, which round-trips through JSON like any other.
    #[test]
    fn collect_with_max_workers_zero_starts_every_item_at_once() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 0, WorkerFailurePolicy::Continue)),
            r#"[{"id":"a"},{"id":"b"},{"id":"c"},{"id":"d"},{"id":"e"}]"#,
        );
        fan_out_split(&mut world);
        fan_out_collect(&mut world);
        assert_eq!(world.get::<SubAgentChildren>(e).unwrap().children.len(), 5);
        let state = world.get::<FanOutWaiting>(e).unwrap().to_state();
        assert_eq!(state.max_workers, usize::MAX);
        assert!(state.pending.is_empty());
        let json = serde_json::to_string(&state).unwrap();
        let back: FanOutState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_workers, usize::MAX);
    }

    #[test]
    fn fan_out_state_roundtrips_and_unresolved_workers_become_failures() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailurePolicy::Continue)),
            r#"[{"id":"a"},{"id":"b"}]"#,
        );
        fan_out_split(&mut world);
        fan_out_collect(&mut world); // starts both workers → active

        // Projecting to the serializable state captures each worker's run-id.
        let state = world.get::<FanOutWaiting>(e).unwrap().to_state();
        assert_eq!(state.active.len(), 2);
        assert!(state.active.iter().all(|(_id, run_id)| !run_id.is_empty()));

        // Restore onto a fresh parent, resolving run-ids back to entities.
        let by_run: std::collections::HashMap<String, Entity> = world
            .get::<SubAgentChildren>(e)
            .unwrap()
            .children
            .iter()
            .filter_map(|&c| {
                world
                    .get::<crate::persistence::RunMetadata>(c)
                    .map(|m| (m.run_id.clone(), c))
            })
            .collect();
        let fresh = world.spawn_empty().id();
        restore_fan_out_waiting(&mut world, fresh, state.clone(), &|rid| {
            by_run.get(rid).copied()
        });
        assert_eq!(
            world
                .get::<FanOutWaiting>(fresh)
                .unwrap()
                .to_state()
                .active
                .len(),
            2
        );

        // A resolver that can't map the workers → they become failures, so the
        // merge still completes rather than waiting forever.
        let orphaned = world.spawn_empty().id();
        restore_fan_out_waiting(&mut world, orphaned, state, &|_| None);
        let s = world.get::<FanOutWaiting>(orphaned).unwrap().to_state();
        assert!(s.active.is_empty());
        assert_eq!(s.failures.len(), 2);
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
            parent_entity: Entity::from_raw_u32(999)
                .expect("a small literal index is always a valid entity id"),
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
            children: vec![
                Entity::from_raw_u32(1000)
                    .expect("a small literal index is always a valid entity id"),
            ],
            max_child_depth: 9,
        });
        fan_out_split(&mut world);
        fan_out_collect(&mut world);
        let kids = world.get::<SubAgentChildren>(e).unwrap();
        assert_eq!(kids.max_child_depth, 9);
        assert_eq!(kids.children.len(), 2); // appended to the existing one
    }

    // ── worker_terminal_result / build_report / inject_conversation ───────────

    /// The bug this feature exists to fix. A worker's contribution used to be
    /// the text of its last assistant message, so a worker whose final turn was
    /// a tool call contributed an empty string - and the shipped
    /// a worker told to report what it did writes that report into exactly that
    /// channel.
    #[test]
    fn a_submitted_answer_beats_the_last_assistant_text() {
        let mut world = World::new();
        let worker = world
            .spawn((
                parent_state(),
                InferenceResult {
                    // What the old code would have handed the merge stage: the
                    // trailing aside, not the deliverable.
                    response: "Let me run the tests one more time.".to_string(),
                    tool_calls: vec![],
                    tokens_used: 0,
                    timestamp: 0,
                },
                crate::persistence::FinalOutput(leviath_core::output::FinalOutput::new(
                    "changed src/lib.rs; the failing test now passes",
                    None,
                    "fix_worker".to_string(),
                    0,
                )),
            ))
            .id();
        set_status(&mut world, worker, AgentStatus::Complete);
        assert_eq!(
            worker_terminal_result(&world, worker),
            Some(Ok(
                "changed src/lib.rs; the failing test now passes".to_string()
            ))
        );
    }

    /// The fallback stays, so a blueprint that happens to end on a text turn
    /// keeps working without declaring anything.
    #[test]
    fn a_worker_that_submitted_nothing_still_falls_back_to_its_text() {
        let mut world = World::new();
        let worker = world
            .spawn((
                parent_state(),
                InferenceResult {
                    response: "the old behaviour".to_string(),
                    tool_calls: vec![],
                    tokens_used: 0,
                    timestamp: 0,
                },
            ))
            .id();
        set_status(&mut world, worker, AgentStatus::Complete);
        assert_eq!(
            worker_terminal_result(&world, worker),
            Some(Ok("the old behaviour".to_string()))
        );
    }

    /// Spawn a worker sitting in a stage that demands a final output.
    fn spawn_required_output_worker(world: &mut World) -> Entity {
        let mut stage = Stage::new(
            "w".to_string(),
            ModelConfig::new("script".to_string(), "m".to_string()),
        );
        stage.require_output = true;
        let layout = ContextLayout::new(
            vec![RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::Clearable,
                10_000,
            )],
            12_000,
        );
        let bp = Blueprint::new("w".to_string(), "d".to_string(), vec![stage], layout);
        let worker = world
            .spawn((parent_state(), AgentBlueprint(bp), StageCursor { index: 0 }))
            .id();
        set_status(world, worker, AgentStatus::Complete);
        worker
    }

    /// The fan-out reported "10 succeeded, 0 failed" over ten empty sections,
    /// because a worker that reached `Complete` without its required output was
    /// read as a success with nothing to say. The merge stage cannot tell those
    /// apart, so it writes a confident merge of nothing.
    ///
    /// This is the ordinary way it happens, not an edge case: a worker that
    /// cannot satisfy its validator retries until its iterations run out and
    /// leaves on the max-iterations path, which ends at `Complete`.
    #[test]
    fn a_worker_that_owes_an_output_and_has_none_is_a_failure() {
        let mut world = World::new();
        let worker = spawn_required_output_worker(&mut world);

        assert_eq!(
            worker_terminal_result(&world, worker),
            Some(Err(
                "worker finished without the final output its stage requires".to_string()
            )),
            "the merge has to be told a worker failed, and why"
        );
    }

    /// The same worker, having actually submitted: its answer is what it
    /// contributes, and the requirement is discharged.
    #[test]
    fn a_worker_that_owes_an_output_and_has_one_contributes_it() {
        let mut world = World::new();
        let worker = spawn_required_output_worker(&mut world);
        world
            .entity_mut(worker)
            .insert(crate::persistence::FinalOutput(
                leviath_core::output::FinalOutput {
                    content: "the rows".to_string(),
                    format: Some("csv".to_string()),
                    stage: "w".to_string(),
                    submitted_at: 0,
                    truncated: false,
                    artifacts: vec![],
                },
            ));

        assert_eq!(
            worker_terminal_result(&world, worker),
            Some(Ok("the rows".to_string()))
        );
    }

    /// A worker with a blueprint but no cursor cannot be placed in a stage, so
    /// there is no stage to read a requirement off. It keeps the fallback rather
    /// than being called a failure for a question that was never asked.
    #[test]
    fn a_worker_with_no_stage_to_read_owes_nothing() {
        let mut world = World::new();
        let bp = fanout_blueprint(cfg(None, 1, WorkerFailurePolicy::Continue));

        // No blueprint at all.
        let bare = world.spawn(parent_state()).id();
        assert!(!worker_requires_output(&world, bare));

        // A blueprint, but no cursor saying which stage it is in.
        let no_cursor = world.spawn((parent_state(), AgentBlueprint(bp))).id();
        assert!(!worker_requires_output(&world, no_cursor));

        // A cursor pointing past the end of the stage list.
        let past_end = world
            .spawn((
                parent_state(),
                AgentBlueprint(fanout_blueprint(cfg(
                    None,
                    1,
                    WorkerFailurePolicy::Continue,
                ))),
                StageCursor { index: 99 },
            ))
            .id();
        assert!(!worker_requires_output(&world, past_end));
    }

    /// A blueprint that never opted in keeps the old fallback, empty text and
    /// all. Turning that into a failure would break every fan-out written before
    /// `require_output` existed.
    #[test]
    fn a_worker_that_owes_nothing_keeps_the_last_turn_fallback() {
        let mut world = World::new();
        let worker = world.spawn(parent_state()).id();
        set_status(&mut world, worker, AgentStatus::Complete);

        assert_eq!(
            worker_terminal_result(&world, worker),
            Some(Ok(String::new()))
        );
    }

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

        assert!(
            worker_terminal_result(
                &world,
                Entity::from_raw_u32(4242)
                    .expect("a small literal index is always a valid entity id")
            )
            .is_some_and(|r| r.is_err())
        );
    }

    /// The failure this bound exists for. A hundred workers answering at the
    /// size limit build a 25 MB report; `add_entry` rejects an over-budget entry
    /// rather than truncating, and the error was discarded - so the merge stage
    /// received nothing at all, silently, in exactly the case fan-out is for.
    #[test]
    fn a_huge_fan_out_still_reaches_the_merge_stage() {
        let mut world = World::new();
        let mut window = ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::Clearable,
            10_000,
        ));
        let parent = world.spawn((parent_state(), window)).id();

        // A hundred workers, each answering at the per-submission cap.
        let huge = "x".repeat(leviath_core::output::MAX_FINAL_OUTPUT_BYTES);
        let summaries: Vec<(String, String)> =
            (0..100).map(|i| (format!("w{i}"), huge.clone())).collect();
        let report = build_report(&summaries, &[], Some(10_000));
        inject_results(&mut world, parent, "conversation", &report);

        let region = world
            .get::<ContextWindow>(parent)
            .expect("window")
            .get_region("conversation")
            .expect("region");
        assert!(
            !region.content.is_empty(),
            "the merge stage must receive something rather than nothing"
        );
        let landed = &region.content[0].content;
        // Every worker is still accounted for in the header, and the text says
        // it was cut rather than pretending to be whole.
        assert!(landed.contains("100 succeeded"), "header survives");
        assert!(landed.contains("truncated"), "and says it was cut");
        assert!(region.current_tokens <= region.max_tokens, "within budget");
    }

    /// Dividing the region between the workers makes the report fit an *empty*
    /// region, which is the easy case. A region already carrying something has
    /// less room than that, and the report-level trim is what keeps the write
    /// from being rejected outright: `add_entry` refuses an over-budget entry
    /// rather than shortening it, so without this the merge stage receives
    /// nothing at all.
    #[test]
    fn a_report_larger_than_what_is_left_of_the_region_is_trimmed_not_dropped() {
        const REGION_TOKENS: usize = 2_000;
        let mut world = World::new();
        let mut window = ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            "worker_results".to_string(),
            leviath_core::RegionKind::Clearable,
            REGION_TOKENS,
        ));
        // Most of the region is already spoken for.
        let filler = "f".repeat(REGION_TOKENS * 4 * 8 / 10);
        let filler_tokens = leviath_core::estimate_tokens(&filler);
        window
            .add_typed_entry(
                "worker_results",
                leviath_core::EntryKind::UserMessage,
                filler,
                filler_tokens,
            )
            .expect("the filler fits");
        let parent = world.spawn((parent_state(), window)).id();

        // A report sized for the whole region, landing in what is left of it.
        let long = "x".repeat(5_000);
        let summaries: Vec<(String, String)> =
            (0..8).map(|i| (format!("w{i}"), long.clone())).collect();
        let report = build_report(&summaries, &[], Some(REGION_TOKENS));
        assert!(report.len() > REGION_TOKENS * 4 / 5, "the report is big");
        inject_results(&mut world, parent, "worker_results", &report);

        let region = world
            .get::<ContextWindow>(parent)
            .expect("window")
            .get_region("worker_results")
            .expect("region")
            .clone();
        assert_eq!(
            region.content.len(),
            2,
            "the report landed beside the filler"
        );
        let landed = &region.content[1].content;
        assert!(
            landed.contains("8 succeeded"),
            "the header survives the cut"
        );
        assert!(
            landed.contains(REPORT_TRUNCATION_MARKER.trim()),
            "and it says it was cut"
        );
        assert!(region.current_tokens <= region.max_tokens, "within budget");
    }

    /// The share is equal, so every worker appears. The first cut capped each
    /// worker at a fixed size and trimmed the finished report to fit, which gave
    /// the early workers their full allowance and cut the late ones off
    /// entirely - a hundred-way fan-out where only the first twenty were
    /// readable, with nothing saying so.
    #[test]
    fn every_worker_appears_in_a_large_fan_out() {
        // End to end: building the report and landing it in the region. The
        // unfairness was in the second half - a fixed per-worker size makes a
        // report far too big, and trimming *that* keeps the front and drops the
        // back.
        const REGION_TOKENS: usize = 40_000;
        let mut world = World::new();
        let mut window = ContextWindow::new(400_000);
        window.add_region(leviath_core::Region::new(
            "worker_results".to_string(),
            leviath_core::RegionKind::Clearable,
            REGION_TOKENS,
        ));
        let parent = world.spawn((parent_state(), window)).id();

        let long = "x".repeat(50_000);
        let summaries: Vec<(String, String)> =
            (0..100).map(|i| (format!("w{i}"), long.clone())).collect();
        let report = build_report(&summaries, &[], Some(REGION_TOKENS));
        inject_results(&mut world, parent, "worker_results", &report);

        let landed = world
            .get::<ContextWindow>(parent)
            .expect("window")
            .get_region("worker_results")
            .expect("region")
            .content[0]
            .content
            .clone();
        for i in 0..100 {
            assert!(
                landed.contains(&format!("## worker w{i}\n")),
                "worker w{i} never reached the merge stage"
            );
        }
        // And it says the sections are extracts, so the merge stage knows to go
        // to a worker's own run for the rest.
        assert!(landed.contains("read a worker's own run"));
    }

    /// Each worker gets the same room, whatever the count.
    #[test]
    fn the_share_shrinks_as_the_worker_count_grows() {
        assert!(bytes_per_worker(Some(40_000), 4) > bytes_per_worker(Some(40_000), 100));
        // A bigger region means a bigger share for the same workers.
        assert!(bytes_per_worker(Some(80_000), 10) > bytes_per_worker(Some(40_000), 10));
        // Never so small a section says nothing at all.
        assert_eq!(
            bytes_per_worker(Some(10), 10_000),
            MIN_REPORT_BYTES_PER_WORKER
        );
        // No readable budget falls back rather than dividing by nothing.
        assert_eq!(bytes_per_worker(None, 4), DEFAULT_REPORT_BYTES_PER_WORKER);
    }

    /// A blueprint can send the results somewhere other than the conversation,
    /// which is otherwise carrying the message history alongside them.
    #[test]
    fn results_go_to_the_named_region() {
        let mut world = World::new();
        let mut window = ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::Clearable,
            10_000,
        ));
        window.add_region(leviath_core::Region::new(
            "worker_results".to_string(),
            leviath_core::RegionKind::Clearable,
            20_000,
        ));
        let parent = world.spawn((parent_state(), window)).id();
        inject_results(&mut world, parent, "worker_results", "the report");

        let w = world.get::<ContextWindow>(parent).expect("window");
        assert_eq!(
            w.get_region("worker_results")
                .expect("region")
                .content
                .len(),
            1
        );
        assert!(
            w.get_region("conversation")
                .expect("region")
                .content
                .is_empty(),
            "the default region is left alone"
        );
    }

    /// A named region the layout does not declare falls back rather than
    /// swallowing the whole report.
    #[test]
    fn an_unknown_results_region_falls_back_to_the_conversation() {
        let mut world = World::new();
        let mut window = ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::Clearable,
            10_000,
        ));
        let parent = world.spawn((parent_state(), window)).id();
        inject_results(&mut world, parent, "typo_region", "the report");

        assert_eq!(
            world
                .get::<ContextWindow>(parent)
                .expect("window")
                .get_region("conversation")
                .expect("region")
                .content
                .len(),
            1
        );
    }

    /// A report that fits is passed through untouched, so the common case reads
    /// exactly as it did.
    #[test]
    fn a_small_fan_out_report_is_not_trimmed() {
        let mut world = World::new();
        let mut window = ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::Clearable,
            10_000,
        ));
        let parent = world.spawn((parent_state(), window)).id();
        let report = build_report(
            &[("a".to_string(), "did the thing".to_string())],
            &[],
            Some(10_000),
        );
        inject_results(&mut world, parent, "conversation", &report);
        let landed = world
            .get::<ContextWindow>(parent)
            .expect("window")
            .get_region("conversation")
            .expect("region")
            .content[0]
            .content
            .clone();
        assert_eq!(landed, report);
    }

    #[test]
    fn build_report_lists_successes_and_failures() {
        let report = build_report(
            &[("a".to_string(), "ok-a".to_string())],
            &[("b".to_string(), "boom".to_string())],
            None,
        );
        assert!(report.contains("1 succeeded, 1 failed"));
        assert!(report.contains("## worker a\nok-a"));
        assert!(report.contains("## worker b FAILED\nboom"));
    }

    #[test]
    fn inject_conversation_is_a_noop_without_a_window() {
        let mut world = World::new();
        let has_window = world.spawn(window()).id();
        inject_results(&mut world, has_window, "conversation", "hello");
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
        inject_results(&mut world, no_window, "conversation", "hello");
    }

    #[test]
    fn set_status_is_a_noop_for_a_missing_agent() {
        let mut world = World::new();
        set_status(
            &mut world,
            Entity::from_raw_u32(77).expect("a small literal index is always a valid entity id"),
            AgentStatus::Complete,
        );
        assert_eq!(
            agent_status(
                &world,
                Entity::from_raw_u32(77)
                    .expect("a small literal index is always a valid entity id")
            ),
            None
        );
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
        let agent = crate::world::AgentId::in_world(&world, e);
        force_transition(&mut world, agent, 1);
        assert!(world.get::<ToolResultRoutingComponent>(e).is_some());
        assert!(world.get::<ReadyToInfer>(e).is_some());

        // Despawned entity: no panic, no effect.
        let gone = crate::world::AgentId::in_world(
            &world,
            Entity::from_raw_u32(9191).expect("a small literal index is always a valid entity id"),
        );
        force_transition(&mut world, gone, 1);
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
        let agent = crate::world::AgentId::in_world(&world, e);
        force_transition(&mut world, agent, 1);
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
