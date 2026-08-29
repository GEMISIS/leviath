//! Restart recovery: reload persisted non-terminal agents into a fresh world when
//! the daemon starts, so runs interrupted by a stop/crash resume where they left
//! off - critically, any agent that was mid-inference re-issues that inference
//! (the reloaded agent is `ReadyToInfer`), rather than being lost.
//!
//! For each `<runs_dir>/<run_id>/meta.json` whose status is non-terminal, this
//! loads the blueprint (via [`build_agent_for_reload`], reusing the spawn path),
//! which skips the required-at-spawn region gate since the window is restored
//! from a snapshot; restores the
//! persisted context / stage / iteration / token totals via
//! [`leviath_runtime::restore::restore_agent`], puts the run's per-stage ledger
//! back from `stages.json` via
//! [`leviath_runtime::restore::restore_stage_ledger`], and preserves the
//! original run metadata. Anything unreadable or un-reloadable is skipped (logged), never fatal.
//!
//! One exception to the "re-issue inference" resume: a run that was parked at a
//! stage-boundary interaction point (e.g. `plan_approval`) wrote an
//! `interactions.json` sidecar while blocked. For those, `reload_one` calls
//! [`leviath_runtime::interaction_points::restore_interaction_point`] to bring the
//! agent back in the *waiting* state with the same prompt re-opened, rather than
//! re-inferring and dropping it. Model-initiated dynamic tools
//! (`ask_user_*`, `present_for_review`, `edit_document`) and taint-gate prompts are
//! not persisted - they block inside the transient tool-worker turn, so on restart
//! they take the ordinary re-inference path and the model simply re-asks.
//!
//! ## Tool-call delivery contract (issue #96)
//!
//! A tool batch in flight at the crash is **replayed, not re-executed**. Dispatch
//! journals the batch (a `ToolBatch` record) before its side effects can start,
//! and every call's result the moment it finishes (`ToolCallDone`); when the fold
//! surfaces such a pending batch, `reload_one` calls
//! [`leviath_runtime::restore::restore_pending_batch`] to land the assistant turn
//! with each completed call's real journaled result - so completed side effects
//! are exactly-once across a restart. Calls whose completion never reached the
//! journal (still executing, or the crash landed in the instant between the
//! external effect and its journal append - a window no journal can close,
//! since an external side effect can't be observed atomically) come back as
//! verify-first `[error] interrupted` results rather than being silently re-run;
//! the re-issued inference decides what still needs doing.

use std::path::Path;

use bevy_ecs::entity::Entity;
use leviath_core::run_archive;
use leviath_core::run_meta::{ContextSnapshot, RunMeta, RunStatus};
use leviath_runtime::host::SpawnArgs;
use leviath_runtime::interaction_points::InteractionPointState;
use leviath_runtime::persistence::{RunMetadata, TokenTotals};
use leviath_runtime::restore::restore_agent;
use leviath_runtime::world::PipelineWorld;

// Seven of recovery's former imports came in only to spell the seven parameters
// that are now one `SpawnDeps`.
use crate::daemon::spawn::{SpawnDeps, build_agent_for_reload};

/// Reload every non-terminal persisted run under `runs_dir`, returning the
/// `(run_id, entity)` pairs for the host to map. Runs that fail to reload are
/// skipped.
pub(crate) fn reload_persisted_agents(
    world: &mut PipelineWorld,
    deps: SpawnDeps<'_>,
    runs_dir: &Path,
) -> Vec<(String, leviath_runtime::world::AgentId)> {
    let mut reloaded: Vec<(RunMeta, Entity)> = Vec::new();
    let Ok(dir_entries) = std::fs::read_dir(runs_dir) else {
        return Vec::new(); // no runs dir yet - nothing to recover
    };
    // Scan phase: collect every persisted run's metadata + whether it's parked mid
    // fan-out (has a fanout.json), so the triage can rank them.
    let candidates: Vec<(RunMeta, bool)> = dir_entries
        .flatten()
        .filter_map(|dir_entry| {
            let run_dir = dir_entry.path();
            let meta = read_meta(&run_dir)?; // no meta.json, or unreadable/unparseable
            let parked_on_fanout = run_dir.join(leviath_core::files::FANOUT_FILE).exists();
            Some((meta, parked_on_fanout))
        })
        .collect();
    // Order phase: drop terminal runs and rank the rest actionable-first (in-flight
    // inference / pending tool results before blocked-on-input), so interrupted work
    // that can make progress resumes ahead of runs that can't.
    let ordered = leviath_runtime::restore::triage_restores(candidates);
    for meta in ordered {
        let run_dir = runs_dir.join(&meta.run_id);
        match reload_one(world, deps.clone(), &meta, &run_dir) {
            Ok(entity) => reloaded.push((meta, entity)),
            Err(e) => {
                tracing::warn!(run_id = %meta.run_id, error = %e, "skipping un-reloadable run");
                mark_crashed(&run_dir, meta, &e.to_string(), deps.now_secs);
            }
        }
    }
    // Second pass: every run is now an entity, so rebuild the parent→children
    // tree deterministically from the persisted links (no heuristics), then
    // resume any parent that was parked mid fan-out.
    relink_tree(world, &reloaded);
    restore_fan_outs(world, &reloaded, runs_dir);
    // Scoped on the way out: the host stores these for the life of the daemon,
    // which is exactly where a bare entity would lose track of its world.
    reloaded
        .into_iter()
        .map(|(meta, entity)| (meta.run_id, world.own_agent(entity)))
        .collect()
}

/// Page a single unloaded run back into the world from disk, on demand. Reads
/// its persisted metadata; if the run exists and is non-terminal, reloads it
/// (blueprint + tool state + context/stage) and returns the new entity. `None`
/// if there's no such resumable run. This is the host's reload-on-demand seam
/// (an op targeting an unloaded run pages it in first).
pub(crate) fn reload_run(
    world: &mut PipelineWorld,
    deps: SpawnDeps<'_>,
    run_id: &str,
    runs_dir: &std::path::Path,
) -> Option<leviath_runtime::world::AgentId> {
    let run_dir = runs_dir.join(run_id);
    let meta = read_meta(&run_dir)?;
    if is_finished(&meta.status) {
        return None; // a finished run isn't paged back in
    }
    let entity = reload_one(world, deps, &meta, &run_dir).ok()?;
    Some(world.own_agent(entity))
}

/// Rebuild `FanOutWaiting` for any reloaded parent that was parked mid fan-out
/// (a `<run_dir>/fanout.json` is present), so its split/merge resumes rather than
/// hanging. Active workers are re-linked by run-id via the reloaded run→entity
/// map; a worker that didn't reload is recorded as a failure so the merge still
/// completes. A malformed/absent file is skipped.
fn restore_fan_outs(world: &mut PipelineWorld, reloaded: &[(RunMeta, Entity)], runs_dir: &Path) {
    let by_run_id: std::collections::HashMap<&str, Entity> = reloaded
        .iter()
        .map(|(m, e)| (m.run_id.as_str(), *e))
        .collect();
    for (meta, entity) in reloaded {
        let path = runs_dir
            .join(&meta.run_id)
            .join(leviath_core::files::FANOUT_FILE);
        let Some(state) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<leviath_runtime::fanout::FanOutState>(&s).ok())
        else {
            continue;
        };
        leviath_runtime::fanout::restore_fan_out_waiting(
            world.world_mut(),
            *entity,
            state,
            &|rid| by_run_id.get(rid).copied(),
        );
    }
}

/// Rebuild `ParentRef` / `SubAgentChildren` on the freshly reloaded entities from
/// their persisted `parent_run_id` / `children` links, so a restarted daemon
/// resumes the exact sub-agent tree (a waiting parent holds for its children;
/// children aren't orphaned). Links whose counterpart didn't reload are logged
/// and skipped. Idempotent: existing components are overwritten, not duplicated.
fn relink_tree(world: &mut PipelineWorld, reloaded: &[(RunMeta, Entity)]) {
    use leviath_runtime::components::{AgentState, ParentRef, SubAgentChildren};

    let by_run_id: std::collections::HashMap<&str, Entity> = reloaded
        .iter()
        .map(|(m, e)| (m.run_id.as_str(), *e))
        .collect();
    let w = world.world_mut();
    for (meta, entity) in reloaded {
        // Child → parent edge.
        if let Some(parent_id) = &meta.parent_run_id {
            match by_run_id.get(parent_id.as_str()) {
                Some(&parent_entity) => {
                    w.entity_mut(*entity).insert(ParentRef {
                        parent_entity,
                        parent_agent_id: parent_id.clone(),
                        depth: meta.depth,
                    });
                }
                None => tracing::warn!(
                    run_id = %meta.run_id, parent = %parent_id,
                    "parent run did not reload; leaving child unlinked"
                ),
            }
        }
        // Parent → children edge (skip any child that didn't reload).
        if !meta.children.is_empty() {
            let children: Vec<Entity> = meta
                .children
                .iter()
                .filter_map(|cid| by_run_id.get(cid.as_str()).copied())
                .collect();
            if !children.is_empty() {
                w.entity_mut(*entity).insert(SubAgentChildren {
                    children,
                    max_child_depth: meta.max_child_depth,
                });
            }
            // Keep the serializable child list consistent with the rebuilt
            // component so the next snapshot re-persists the same tree. A reloaded
            // agent always carries `AgentState`.
            w.get_mut::<AgentState>(*entity)
                .expect("a reloaded agent always has AgentState")
                .spawned_children_ids = meta.children.clone();
        }
    }
}

/// Read + parse `<run_dir>/meta.json`, returning `None` if it is missing or
/// invalid. Recovery treats both the same way: a run it cannot read is a run
/// it leaves alone.
fn read_meta(run_dir: &Path) -> Option<RunMeta> {
    crate::runstate::read_meta_from(run_dir).ok()
}

/// The cumulative token totals recorded in a run's metadata.
fn totals_from(meta: &RunMeta) -> TokenTotals {
    TokenTotals {
        prompt_tokens: meta.prompt_tokens,
        completion_tokens: meta.completion_tokens,
        cached_tokens: meta.cached_tokens,
        cache_write_tokens: meta.cache_write_tokens,
        tool_calls: meta.tool_calls,
        // Restored so a resumed run keeps counting from what it already spent
        // rather than restarting at zero. The unpriced count comes back too:
        // a run that could not be priced before resuming still cannot be.
        cost: leviath_providers::CostTotals {
            priced_usd: meta.cost_priced_usd,
            // The per-call split is not persisted, only whether the total was
            // exact. Restoring both counts as zero would let a resumed run
            // whose cost had been RECONSTRUCTED come back claiming to be the
            // invoice, so an inexact total carries one computed call forward -
            // enough to keep `is_exact()` false, which is the only thing these
            // two counts are read for after a resume.
            reported_calls: 0,
            computed_calls: usize::from(!meta.cost_is_exact),
            unpriced_calls: meta.unpriced_calls,
        },
    }
}

/// Record a run that could not be reloaded as terminally errored.
///
/// The daemon is the sole owner of these runs, so anything still marked
/// `running` at startup is by definition not running. Runs that *can* be
/// reloaded are resumed (that is the whole point of this module); this is only
/// for the ones that can't. Logging the failure without this write would leave
/// them claiming `"status": "running"` on disk forever, so `lev ps` and the
/// dashboard would show a live run that no longer exists.
///
/// Best-effort: a write failure here is logged, never fatal - the daemon is
/// mid-startup and the rest of the recovery pass must still run.
fn mark_crashed(run_dir: &Path, meta: RunMeta, reason: &str, now_secs: i64) {
    let crashed = RunMeta {
        status: RunStatus::Error,
        error: Some(format!(
            "the daemon exited while this run was active and it could not be recovered: {reason}"
        )),
        updated_at: now_secs,
        ..meta
    };
    if let Err(e) = crate::runstate::write_meta_to(run_dir, &crashed) {
        tracing::warn!(
            run_id = %crashed.run_id,
            error = %e,
            "could not record an un-reloadable run as crashed"
        );
    }
}

/// Whether a run's status means it should not be resumed.
/// Whether this run is over, as opposed to merely stopped.
///
/// `Cancelled` is deliberately not here. A cancelled run keeps its journal, its
/// context regions, its stage and iteration, and its parked fan-out state, so
/// there is something to carry on from and `lev resume` should be able to reach
/// it (#576). A `Complete` run has nothing left to do, and an `Error` one should
/// be read before it is continued.
///
/// This governs paging a run back in on demand. Startup recovery has its own
/// filter in `triage_restores`, which still drops cancelled runs: somebody
/// stopped that run on purpose, and restarting the daemon is not them changing
/// their mind.
fn is_finished(status: &RunStatus) -> bool {
    matches!(status, RunStatus::Complete | RunStatus::Error)
}

/// Reload one run: spawn it fresh from its blueprint, then overlay the persisted
/// context / stage / totals / per-stage ledger and preserve the original run
/// metadata.
fn reload_one(
    world: &mut PipelineWorld,
    deps: SpawnDeps<'_>,
    meta: &RunMeta,
    run_dir: &Path,
) -> Result<Entity, String> {
    let args = SpawnArgs {
        run_id: meta.run_id.clone(),
        blueprint_path: meta.agent_path.clone(),
        task: meta.task.clone(),
        // Region seed content isn't replayed on reload: the window is restored
        // from the persisted context snapshot after build_agent, so re-seeding
        // would be redundant (and could double up content).
        regions: Default::default(),
        // The override the run was *launched* with, not the label its entry
        // stage resolved to. `meta.model` is always set once a run has started,
        // and handing it back here as `--model` pinned every stage of a
        // reloaded run to the first stage's provider and model - a run that
        // named nothing at launch lost its whole failover list on restart.
        model: meta.model_override.clone(),
        workdir: meta.workdir.clone(),
        metadata: meta.metadata.clone(),
        callback_url: meta.callback_url.clone(),
        callback_secret: meta.callback_secret.clone(),
        // `--yolo` is the one launch override that survives a reload, because
        // it is the one whose loss strands the run. Dropping it looked like the
        // safe choice - forgetting an override can only prompt more, never less
        // - but "more prompting" for an unattended run means stopping forever on
        // a prompt nobody is watching for. The operator gave this consent at
        // launch and never withdrew it; a daemon restart is not a withdrawal.
        // Runs written before `yolo` was persisted default to `false`.
        //
        // `--allow` and `--max-depth` stay unpersisted: losing them narrows what
        // the run may do, which is the harmless direction.
        yolo: meta.yolo,
        // Belt and braces: seeds aren't replayed on reload at all (see above),
        // so a resumed run can never re-execute a command seed.
        no_seed_commands: true,
        allow: Vec::new(),
        max_depth: None,
        parent_run_id: meta.parent_run_id.clone(),
        // Restored for the same reason `yolo` is: a reload that dropped the
        // caller's requested shape would silently revert the run to the
        // blueprint's partway through, and the caller would never see why.
        output: meta.output_request.clone(),
    };
    let entity = build_agent_for_reload(world.world_mut(), deps, &args)?;

    // Restore the persisted context, stage, iteration, and token totals.
    //
    // Prefer the run's atomic journal (`run.lvr`): it records meta + context
    // together, so a crash between the separate `meta.json` and `context.json`
    // writes can't leave us with a mismatched pair (new stage/iteration + stale
    // context). The archive is appended *before* either JSON file, so in that exact
    // crash window it already holds the newer generation and folds to a consistent
    // `{meta, context}`. Fall back to the separate JSON files only for runs written
    // before the archive existed, or an archive that couldn't be read at all - that
    // pair may be one tick out of sync, but it's the pre-existing behavior.
    let folded = std::fs::read(run_dir.join(leviath_core::files::ARCHIVE_FILE))
        .ok()
        .and_then(|bytes| run_archive::read_archive_lenient(&mut bytes.as_slice()).ok())
        .and_then(|(_version, records)| run_archive::fold(&records));
    let (snapshot, stage_index, iteration, totals, pending_batch) = match folded {
        Some(folded) => {
            let totals = totals_from(&folded.meta);
            (
                folded.context,
                folded.meta.stage_index,
                folded.meta.iteration,
                totals,
                folded.pending_batch,
            )
        }
        None => {
            let snapshot = std::fs::read_to_string(run_dir.join(leviath_core::files::CONTEXT_FILE))
                .ok()
                .and_then(|s| serde_json::from_str::<ContextSnapshot>(&s).ok())
                .unwrap_or_else(|| ContextSnapshot {
                    stage_name: meta.current_stage.clone(),
                    total_tokens: 0,
                    max_tokens: 0,
                    regions: Vec::new(),
                });
            (
                snapshot,
                meta.stage_index,
                meta.iteration,
                totals_from(meta),
                // No journal ⇒ no batch record ⇒ the pre-journal behavior
                // (plain re-inference).
                None,
            )
        }
    };
    restore_agent(
        world.world_mut(),
        entity,
        &snapshot,
        stage_index,
        iteration,
        totals,
    );

    // Put the per-stage ledger back too. `build_agent_for_reload` seeds one
    // all-zero record per blueprint stage, and the persist tick rewrites
    // `stages.json` whole, so skipping this did not merely fail to restore the
    // run's stage history - the next tick wrote zeros over the file that held
    // it, while `meta.json`'s run totals went on looking correct (issue #415).
    // No file (a run from before the ledger, or one that stopped before its
    // first persist) leaves the seeded records as they are.
    let mut stage_records = crate::runstate::read_stages_index_from(run_dir);
    for rec in stage_records.iter_mut() {
        if let Some(clock) = rec.active.as_mut() {
            clock.settle(meta.updated_at);
        }
        // The visit in progress has a clock of its own, and it was left running
        // by a daemon that has since stopped. Settled at the same moment for the
        // same reason: the span ended when the process holding it did, not now.
        // The visit itself stays open - the run really is still in that stage,
        // and re-entering is not what a resume does.
        if let Some(clock) = rec
            .visits
            .last_mut()
            .filter(|v| v.left_at.is_none())
            .and_then(|v| v.active.as_mut())
        {
            clock.settle(meta.updated_at);
        }
    }
    leviath_runtime::restore::restore_stage_ledger(world.world_mut(), entity, &stage_records);

    // Put the run's working clock back, so a resumed run reports the time it has
    // actually spent rather than starting over. Settled at `updated_at` first:
    // a span left open by a daemon that died ended when the daemon did, and
    // carrying it forward would bill the run for the outage.
    {
        let mut clock = world
            .world_mut()
            .get_mut::<leviath_runtime::persistence::RunClock>(entity)
            .expect("build_agent attached a run clock");
        clock.0 = meta.active.unwrap_or_default();
        clock.0.settle(meta.updated_at);
    }

    // A run parked until the machine is fixed keeps its reason across a
    // restart. Without this the marker is a live component nothing rebuilds,
    // so the run returns as a bare `Paused` and the next persist tick computes
    // `waiting_on` from markers that are no longer there and writes `null`
    // over the recorded reason - the same shape as the stage ledger above,
    // where not restoring did not merely lose the value but erased the file
    // that held it. The run is not re-dispatched while paused, so nothing
    // would recompute it either.
    if let Some(leviath_core::run_meta::WaitReason::NeedsSetup { blocker, remedy }) =
        &meta.waiting_on
    {
        world
            .world_mut()
            .entity_mut(entity)
            .insert(leviath_runtime::pipeline::PausedForSetup {
                blocker: *blocker,
                remedy: remedy.clone(),
            });
    }

    // A tool batch was in flight when the daemon died and its results never
    // reached the window: replay what the journal recorded - real results for
    // completed calls, verify-first errors for interrupted ones - so the
    // re-issued inference sees what already ran instead of re-executing the
    // batch's side effects (issue #96). fold() only surfaces a batch that is
    // genuinely unapplied (same iteration, turn absent from the window).
    if let Some(batch) = pending_batch {
        leviath_runtime::restore::restore_pending_batch(
            world.world_mut(),
            entity,
            &batch,
            &meta.children,
        );
    }

    // `build_agent` stamps fresh run metadata; preserve the original identity.
    {
        let mut md = world
            .world_mut()
            .get_mut::<RunMetadata>(entity)
            .expect("build_agent attached run metadata");
        md.started_at = meta.started_at;
        md.title = meta.title.clone();
        // Carried across the restart with the title itself. A reloaded run does
        // not get another titling attempt (see `wants_title` at spawn), so
        // dropping this would turn "titling ran and could not name this run"
        // back into the silence the field exists to end.
        md.title_error = meta.title_error.clone();
        md.callback_url = meta.callback_url.clone();
        md.callback_secret = meta.callback_secret.clone();
        // `parent_run_id` was already restored via `args` into build_agent's metadata.
    }

    // Carry the run's productivity flags across the restart, so a resumed run
    // doesn't report itself as having modified nothing (issue #107).
    {
        let mut flags = world
            .world_mut()
            .get_mut::<leviath_runtime::persistence::RunOutcomeFlags>(entity)
            .expect("build_agent attached run outcome flags");
        flags.0 = meta.flags.clone();
    }

    // Put back an answer the run had already submitted, content and all: the
    // descriptor is in `meta.json`, the bytes in the sidecar beside it. Without
    // this the component is absent after a reload, and the very next persist
    // tick writes a `meta.json` with no `final_output` - so a restart would not
    // merely fail to restore the answer, it would erase the one on disk. It
    // also re-arms the required-output gate correctly: a stage that submitted
    // before the restart is not asked to do it again.
    if let Some(output) = crate::runstate::read_final_output_in(run_dir, meta) {
        world
            .world_mut()
            .entity_mut(entity)
            .insert(leviath_runtime::persistence::FinalOutput(output));
    }

    // If this run was parked at a stage-boundary interaction point (e.g.
    // plan_approval), re-present it in the *waiting* state rather than the default
    // `Active` + `ReadyToInfer` restore - so the open prompt survives the restart
    // instead of being dropped and re-inferred (issue #38). A missing/malformed
    // sidecar, or a blueprint that no longer matches, leaves the default restore.
    if let Some(state) =
        std::fs::read_to_string(run_dir.join(leviath_core::files::INTERACTIONS_FILE))
            .ok()
            .and_then(|s| serde_json::from_str::<InteractionPointState>(&s).ok())
    {
        // Built against this world's ECS a few lines above, so it is ours.
        let agent = world.own_agent(entity);
        leviath_runtime::interaction_points::restore_interaction_point(
            world.world_mut(),
            agent,
            state,
        );
    }

    // A run the user paused stays paused across the restart: the default
    // restore presents it `Active`, which would silently resume it.
    if meta.status == RunStatus::Paused {
        // Built against this world's ECS, so it is ours.
        world.pause(world.own_agent(entity));
    }

    Ok(entity)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Named here rather than inherited from the parent: production now spells
    // these seven as one `SpawnDeps`, so importing them above would mean seven
    // imports the module itself does not use.
    use std::sync::Arc;

    use leviath_mcp::ToolExecutor;
    use leviath_runtime::host::SubAgentOp;
    use leviath_runtime::interaction_hub::InteractionHub;
    use tokio::sync::Mutex;
    use tokio::sync::mpsc::UnboundedSender;

    use crate::config::Config;
    use crate::daemon::tool_service::CliToolService;

    use leviath_runtime::ProviderRegistry;
    use leviath_runtime::components::AgentStatus;
    use leviath_runtime::inference_pool::InferencePoolConfig;
    use tokio::runtime::Handle;

    fn sub_tx() -> UnboundedSender<SubAgentOp> {
        tokio::sync::mpsc::unbounded_channel().0
    }

    struct FakeProvider;
    #[async_trait::async_trait]
    impl leviath_providers::Provider for FakeProvider {
        async fn infer(
            &self,
            _r: &leviath_providers::InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            Err(leviath_providers::ProviderError::Other("t".to_string()))
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            1000
        }
        fn name(&self) -> &str {
            "fake"
        }
        fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    fn test_world() -> (PipelineWorld, Arc<CliToolService>) {
        let cli = Arc::new(CliToolService::new());
        let mut registry = ProviderRegistry::new();
        for p in ["anthropic", "openai", "ollama"] {
            registry.register(p.to_string(), Arc::new(FakeProvider));
        }
        let world = PipelineWorld::new(
            registry,
            cli.clone(),
            InferencePoolConfig::new(),
            1,
            None,
            Handle::current(),
        );
        (world, cli)
    }

    fn coder_manifest() -> String {
        // Self-contained fixture - not the shipped blueprint (see test_support).
        crate::test_support::inline_coder_manifest()
    }

    /// Write a `<runs_dir>/<run_id>/meta.json` (+ optional context.json) for a run
    /// whose blueprint lives at `agent_path`.
    fn write_run(
        runs_dir: &Path,
        run_id: &str,
        agent_path: &str,
        status: RunStatus,
        context: Option<&ContextSnapshot>,
    ) {
        write_run_tree(RunFixture {
            runs_dir,
            run_id,
            agent_path,
            status,
            context,
            parent_run_id: None,
            children: &[],
            depth: 0,
            max_child_depth: 0,
        });
    }

    /// Like [`write_run`], but with explicit tree links so recovery's re-linking
    /// pass can be exercised.
    /// One persisted run, as a fixture writes it.
    ///
    /// A struct because every field is a column of the record being written, and
    /// a nine-argument call in which three are `&str` and two are `usize` is a
    /// transposition waiting to happen in a file whose whole job is asserting on
    /// what got written.
    struct RunFixture<'a> {
        runs_dir: &'a Path,
        run_id: &'a str,
        agent_path: &'a str,
        status: RunStatus,
        context: Option<&'a ContextSnapshot>,
        parent_run_id: Option<&'a str>,
        children: &'a [&'a str],
        depth: usize,
        max_child_depth: usize,
    }

    fn write_run_tree(f: RunFixture<'_>) {
        let RunFixture {
            runs_dir,
            run_id,
            agent_path,
            status,
            context,
            parent_run_id,
            children,
            depth,
            max_child_depth,
        } = f;
        let dir = runs_dir.join(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = RunMeta {
            // A span still open when the daemon died at `updated_at`, so a
            // reload has something to settle rather than carry forward.
            active: Some(leviath_core::run_meta::ActiveClock {
                banked_secs: 5,
                since: Some(200),
            }),
            run_id: run_id.to_string(),
            agent_name: "coder".to_string(),
            agent_path: agent_path.to_string(),
            task: "resume me".to_string(),
            model: None,
            pid: 0,
            status,
            current_stage: "implement".to_string(),
            stage_index: 0,
            num_stages: 1,
            iteration: 5,
            prompt_tokens: 42,
            completion_tokens: 7,
            cached_tokens: 0,
            cache_write_tokens: 0,
            tool_calls: 3,
            cost_usd: Some(0.0),
            unpriced_calls: 0,
            cost_is_exact: true,
            cost_priced_usd: 0.0,
            workdir: std::env::temp_dir().to_string_lossy().to_string(),
            started_at: 111,
            updated_at: 222,
            last_progress_at: None,
            error: None,
            title: Some("Resume Me".to_string()),
            title_error: None,
            metadata: std::collections::HashMap::new(),
            callback_url: Some("http://cb".to_string()),
            callback_secret: None,
            parent_run_id: parent_run_id.map(str::to_string),
            children: children.iter().map(|s| s.to_string()).collect(),
            depth,
            max_child_depth,
            // Non-default on purpose: proves reload restores the run's
            // productivity flags rather than starting them over (issue #107).
            flags: leviath_core::run_meta::RunFlags {
                modified_files: vec!["src/a.rs".to_string()],
                modified_file_count: 1,
                // Contradicts what this manifest would compute on a fresh
                // spawn (it advertises `write_file`), which is the point: the
                // flags describe how the run actually executed, so the
                // persisted answer wins over a re-derived one (issue #192).
                no_output_tools: true,
                ..Default::default()
            },
            yolo: false,
            read_paths: None,
            // Non-default on purpose, like `flags` above: proves a reload puts
            // the run's answer back rather than dropping it (and then erasing
            // the copy already on disk at the next persist tick).
            final_output: Some(
                leviath_core::output::FinalOutput::new(
                    "already answered",
                    Some("markdown".to_string()),
                    "implement".to_string(),
                    777,
                )
                .descriptor(),
            ),
            output_request: Some(leviath_core::output::OutputSpec {
                format: Some("a2ui".to_string()),
                ..Default::default()
            }),
            model_override: None,
            waiting_on: None,
        };
        std::fs::write(dir.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();
        // The answer's bytes live beside the descriptor, so a reload has
        // something to restore.
        std::fs::write(
            dir.join(leviath_core::FINAL_OUTPUT_FILE),
            "already answered",
        )
        .unwrap();
        if let Some(ctx) = context {
            std::fs::write(
                dir.join("context.json"),
                serde_json::to_string(ctx).unwrap(),
            )
            .unwrap();
        }
    }

    fn agent_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent.leviath"), coder_manifest()).unwrap();
        dir
    }

    /// Write a `<runs_dir>/<run_id>/run.lvr` that folds to the given `stage_index`,
    /// `iteration`, `prompt_tokens`, and `context` - the run's atomic journal. Used
    /// to prove recovery prefers this consistent pair over a stale `context.json`.
    fn write_run_archive(
        runs_dir: &Path,
        run_id: &str,
        agent_path: &str,
        stage_index: usize,
        iteration: usize,
        prompt_tokens: usize,
        context: &ContextSnapshot,
    ) {
        use leviath_core::run_archive::{self, RunIdentity, RunRecord};
        let dir = runs_dir.join(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let mut meta = RunMeta::new(
            run_id.to_string(),
            "coder".to_string(),
            agent_path.to_string(),
            "resume me".to_string(),
            None,
            std::env::temp_dir().to_string_lossy().to_string(),
            1,
        );
        meta.status = RunStatus::Running;
        meta.current_stage = "implement".to_string();
        meta.stage_index = stage_index;
        meta.iteration = iteration;
        meta.prompt_tokens = prompt_tokens;
        let mut buf = Vec::new();
        run_archive::write_archive_start(&mut buf, run_archive::RUN_ARCHIVE_VERSION).unwrap();
        run_archive::write_record(
            &mut buf,
            &RunRecord::Header {
                identity: RunIdentity {
                    run_id: run_id.to_string(),
                    machine_id: "m".to_string(),
                    world_id: "w".to_string(),
                    created_at: 1,
                },
                meta: Box::new(meta),
            },
        )
        .unwrap();
        run_archive::write_record(
            &mut buf,
            &RunRecord::ContextCheckpoint {
                snapshot: context.clone(),
                at: 2,
            },
        )
        .unwrap();
        std::fs::write(dir.join("run.lvr"), &buf).unwrap();
    }

    /// A run the user paused before the restart comes back paused, not the
    /// default `Active` restore - a daemon restart must not silently resume it.
    #[tokio::test]
    async fn reload_keeps_a_paused_run_paused() {
        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let runs = tempfile::tempdir().unwrap();
        write_run(
            runs.path(),
            "run-paused",
            manifest.to_str().unwrap(),
            RunStatus::Paused,
            None,
        );

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 999,
                subagent_tx: sub_tx().clone(),
            },
            runs.path(),
        );

        assert_eq!(restored.len(), 1);
        let (run_id, entity) = &restored[0];
        assert_eq!(run_id, "run-paused");
        assert_eq!(world.agent_status(*entity), Some(AgentStatus::Paused));
    }

    /// Reload one run from `runs_dir` and hand back the world plus its entity.
    /// A descriptor with no sidecar beside it is no answer. That is a run
    /// written before the answer moved out of `meta.json`, or one whose
    /// directory was pruned, and restoring half of it would be worse than
    /// restoring none: the next persist tick would write the half back.
    #[test]
    fn a_descriptor_without_its_sidecar_restores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut meta = RunMeta::new(
            "run-1".to_string(),
            "a".to_string(),
            "/p".to_string(),
            "t".to_string(),
            None,
            "/w".to_string(),
            1,
        );

        // No descriptor at all.
        assert!(crate::runstate::read_final_output_in(dir.path(), &meta).is_none());

        // A descriptor, but nothing on disk to go with it.
        let answer = leviath_core::output::FinalOutput::new(
            "already answered",
            Some("markdown".to_string()),
            "implement".to_string(),
            777,
        );
        meta.final_output = Some(answer.descriptor());
        assert!(crate::runstate::read_final_output_in(dir.path(), &meta).is_none());

        // And with both, the answer comes back whole.
        std::fs::write(
            dir.path().join(leviath_core::FINAL_OUTPUT_FILE),
            &answer.content,
        )
        .unwrap();
        let restored =
            crate::runstate::read_final_output_in(dir.path(), &meta).expect("both halves");
        assert_eq!(restored.content, "already answered");
        assert_eq!(restored.stage, "implement");
    }

    async fn reload_single(runs: &Path, run_id: &str) -> (PipelineWorld, Entity) {
        let (mut world, cli) = test_world();
        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: Arc::new(Mutex::new(ToolExecutor::new())),
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &InteractionHub::new(),
                now_secs: 999,
                subagent_tx: sub_tx().clone(),
            },
            runs,
        );
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].0, run_id);
        let entity = restored[0].1;
        (world, entity.entity())
    }

    /// An unattended run comes back unattended. Dropping `--yolo` on reload was
    /// meant as the safe side, but it converted a running unattended job into
    /// one parked on a prompt nobody was watching for (issue #184).
    #[tokio::test]
    async fn reload_keeps_an_unattended_run_unattended() {
        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let runs = tempfile::tempdir().unwrap();
        write_run(
            runs.path(),
            "run-yolo",
            manifest.to_str().unwrap(),
            RunStatus::Running,
            None,
        );
        // Flip the persisted flag the way a `--yolo` launch would have.
        let meta_path = runs.path().join("run-yolo").join("meta.json");
        let mut meta: RunMeta =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta.yolo = true;
        std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

        let (world, entity) = reload_single(runs.path(), "run-yolo").await;
        assert!(
            world
                .world()
                .get::<RunMetadata>(entity)
                .expect("reloaded run has metadata")
                .unattended
        );
        assert!(
            world
                .world()
                .get::<leviath_runtime::components::InteractionAutoApprove>(entity)
                .is_some(),
            "an unattended reload still auto-approves its checkpoints"
        );
    }

    /// A run launched without `--yolo` must not acquire it on reload, and a
    /// `meta.json` written before the field existed reads as attended.
    #[tokio::test]
    async fn reload_does_not_invent_unattended() {
        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let runs = tempfile::tempdir().unwrap();
        write_run(
            runs.path(),
            "run-plain",
            manifest.to_str().unwrap(),
            RunStatus::Running,
            None,
        );
        // Strip the field entirely: exactly what an older binary wrote.
        let meta_path = runs.path().join("run-plain").join("meta.json");
        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        raw.as_object_mut().unwrap().remove("yolo");
        std::fs::write(&meta_path, serde_json::to_string(&raw).unwrap()).unwrap();

        let (world, entity) = reload_single(runs.path(), "run-plain").await;
        assert!(
            !world
                .world()
                .get::<RunMetadata>(entity)
                .expect("reloaded run has metadata")
                .unattended
        );
    }

    /// A reload replays the `--model` the run was launched with, and only that.
    ///
    /// `meta.model` is the label the entry stage resolved to, and it is set on
    /// every run that has started. It used to be handed back as the override,
    /// so a run launched with no `--model` came back with every stage pinned
    /// to its first stage's pair. Here the label names a provider that is not
    /// registered any more, the way a run resolved on a since-removed key
    /// would: as an override that refused the reload outright; as a label it
    /// is history, and the stages resolve afresh from the blueprint.
    #[tokio::test]
    async fn reload_replays_the_launch_override_not_the_resolved_label() {
        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let runs = tempfile::tempdir().unwrap();

        // No override at launch: an older `meta.json` with a label and no
        // `model_override` field at all.
        write_run(
            runs.path(),
            "run-label",
            manifest.to_str().unwrap(),
            RunStatus::Running,
            None,
        );
        let meta_path = runs.path().join("run-label").join("meta.json");
        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        let obj = raw.as_object_mut().unwrap();
        obj.insert("model".into(), serde_json::json!("ghost/pinned"));
        obj.remove("model_override");
        std::fs::write(&meta_path, serde_json::to_string(&raw).unwrap()).unwrap();

        let (world, entity) = reload_single(runs.path(), "run-label").await;
        let md = world
            .world()
            .get::<RunMetadata>(entity)
            .expect("reloaded run has metadata");
        assert_eq!(md.model_override, None);
        assert_eq!(
            md.model.as_deref(),
            Some("anthropic/m"),
            "the entry stage resolves from the blueprint, not the stale label"
        );

        // A bare `--model` at launch: the reload asks for the same thing, so
        // the stage keeps its provider and takes the named model.
        write_run(
            runs.path(),
            "run-override",
            manifest.to_str().unwrap(),
            RunStatus::Running,
            None,
        );
        let meta_path = runs.path().join("run-override").join("meta.json");
        let mut meta: RunMeta =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta.model = Some("anthropic/m-x".to_string());
        meta.model_override = Some("m-x".to_string());
        std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();
        // The first run is terminal now, so only the second reloads.
        std::fs::remove_dir_all(runs.path().join("run-label")).unwrap();

        let (world, entity) = reload_single(runs.path(), "run-override").await;
        let md = world
            .world()
            .get::<RunMetadata>(entity)
            .expect("reloaded run has metadata");
        assert_eq!(md.model_override.as_deref(), Some("m-x"));
        assert_eq!(md.model.as_deref(), Some("anthropic/m-x"));
    }

    #[tokio::test]
    async fn reloads_nonterminal_runs_and_restores_state() {
        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let runs = tempfile::tempdir().unwrap();

        // A running snapshot with real context.
        let ctx = ContextSnapshot {
            stage_name: "implement".to_string(),
            total_tokens: 4,
            max_tokens: 100_000,
            regions: vec![leviath_core::run_meta::RegionSnapshot {
                name: "conversation".to_string(),
                kind: "clearable".to_string(),
                current_tokens: 4,
                max_tokens: 100_000,
                entries: vec![leviath_core::run_meta::RegionEntrySnapshot {
                    content: "earlier turn".to_string(),
                    tokens: 4,
                    kind: leviath_core::region::EntryKind::UserMessage,
                    metadata: None,
                    key: None,
                    taint: Default::default(),
                }],
                description: None,
            }],
        };
        write_run(
            runs.path(),
            "run-live",
            manifest.to_str().unwrap(),
            RunStatus::Running,
            Some(&ctx),
        );
        // A completed run - must be skipped.
        write_run(
            runs.path(),
            "run-done",
            manifest.to_str().unwrap(),
            RunStatus::Complete,
            None,
        );

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 999,
                subagent_tx: sub_tx().clone(),
            },
            runs.path(),
        );

        assert_eq!(restored.len(), 1);
        let (run_id, entity) = &restored[0];
        assert_eq!(run_id, "run-live");
        assert_eq!(world.agent_status(*entity), Some(AgentStatus::Active));
        // Iteration + preserved metadata restored.
        let md = world.world().get::<RunMetadata>(entity.entity()).unwrap();
        assert_eq!(md.started_at, 111);
        assert_eq!(md.title.as_deref(), Some("Resume Me"));
        assert_eq!(md.callback_url.as_deref(), Some("http://cb"));
        let totals = world.world().get::<TokenTotals>(entity.entity()).unwrap();
        assert_eq!(totals.prompt_tokens, 42);
        assert_eq!(totals.tool_calls, 3);
        // The working clock comes back so a resumed run keeps its time, with the
        // span that was open when the daemon died closed at the last moment the
        // run was known to be up (222) rather than run on to now (999).
        let clock = world
            .world()
            .get::<leviath_runtime::persistence::RunClock>(entity.entity())
            .unwrap();
        assert_eq!(clock.0.banked_secs, 27);
        assert_eq!(clock.0.since, None);
        // ...as are the run's productivity flags, so a resumed run doesn't report
        // itself as having modified nothing.
        let flags = world
            .world()
            .get::<leviath_runtime::persistence::RunOutcomeFlags>(entity.entity())
            .unwrap();
        assert_eq!(flags.0.modified_files, vec!["src/a.rs".to_string()]);
        assert_eq!(flags.0.modified_file_count, 1);
        // Including the capability answer, which the blueprint on disk would
        // now compute differently - the run is judged as it ran (issue #192).
        assert!(flags.0.no_output_tools);
        // The answer the run had already given is put back on the entity. Were
        // it not, the next persist tick would write a meta.json without it and
        // erase the copy already on disk.
        let output = world
            .world()
            .get::<leviath_runtime::persistence::FinalOutput>(entity.entity())
            .expect("a submitted answer survives the restart");
        assert_eq!(output.0.content, "already answered");
        assert_eq!(output.0.stage, "implement");
        // As does the shape the caller asked for at launch, so the resumed run
        // does not silently revert to the blueprint's partway through.
        assert_eq!(
            md.output_request.as_ref().and_then(|s| s.format.as_deref()),
            Some("a2ui")
        );
    }

    /// Fresh + stale differ on every observable field, so the assertions below
    /// pin down exactly which source recovery restored from.
    fn assert_restored_from_archive(world: &PipelineWorld, entity: Entity) {
        use leviath_runtime::components::AgentState;
        let state = world.world().get::<AgentState>(entity).unwrap();
        // stage_name comes from the archive's context (not the stale context.json),
        // iteration from the archive's meta (not meta.json's 5).
        assert_eq!(state.current_stage, "fresh-stage");
        assert_eq!(state.iteration, 9);
        // token totals come from the archive's meta (not meta.json's 42).
        let totals = world.world().get::<TokenTotals>(entity).unwrap();
        assert_eq!(totals.prompt_tokens, 99);
    }

    /// Torn-snapshot pairing: when the atomic journal (`run.lvr`) and the separate
    /// `context.json` disagree - the crash-window state where a new `meta.json`
    /// sits next to a stale `context.json` - resume restores the journal's
    /// consistent `{meta, context}` pair, not the stale JSON.
    #[tokio::test]
    async fn reload_prefers_the_atomic_journal_over_a_stale_context_json() {
        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let mpath = manifest.to_str().unwrap();
        let runs = tempfile::tempdir().unwrap();

        // A STALE context.json (older generation) alongside a meta.json whose
        // iteration/totals are also older than the journal - write_run stamps
        // iteration 5 / prompt_tokens 42.
        let stale = ContextSnapshot {
            stage_name: "stale-stage".to_string(),
            total_tokens: 1,
            max_tokens: 100,
            regions: vec![],
        };
        write_run(
            runs.path(),
            "run-torn",
            mpath,
            RunStatus::Running,
            Some(&stale),
        );
        // The journal at the newer generation: iteration 9, prompt_tokens 99,
        // context stage "fresh-stage".
        let fresh = ContextSnapshot {
            stage_name: "fresh-stage".to_string(),
            total_tokens: 4,
            max_tokens: 100_000,
            regions: vec![],
        };
        write_run_archive(runs.path(), "run-torn", mpath, 0, 9, 99, &fresh);

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 999,
                subagent_tx: sub_tx().clone(),
            },
            runs.path(),
        );

        assert_eq!(restored.len(), 1);
        assert_restored_from_archive(&world, restored[0].1.entity());
    }

    /// A crash *during* the journal append can leave a torn trailing frame. Recovery
    /// reads the journal leniently, so the valid prefix still resolves the resume
    /// state (rather than silently falling back to the possibly-mismatched JSON).
    #[tokio::test]
    async fn reload_tolerates_a_torn_journal_tail() {
        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let mpath = manifest.to_str().unwrap();
        let runs = tempfile::tempdir().unwrap();

        let stale = ContextSnapshot {
            stage_name: "stale-stage".to_string(),
            total_tokens: 1,
            max_tokens: 100,
            regions: vec![],
        };
        write_run(
            runs.path(),
            "run-torn2",
            mpath,
            RunStatus::Running,
            Some(&stale),
        );
        let fresh = ContextSnapshot {
            stage_name: "fresh-stage".to_string(),
            total_tokens: 4,
            max_tokens: 100_000,
            regions: vec![],
        };
        write_run_archive(runs.path(), "run-torn2", mpath, 0, 9, 99, &fresh);
        // Append a torn frame (length prefix promising bytes that aren't there).
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(runs.path().join("run-torn2/run.lvr"))
                .unwrap();
            f.write_all(&[0, 0, 0, 0, 0, 0, 0, 10, 1, 2]).unwrap();
        }

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 999,
                subagent_tx: sub_tx().clone(),
            },
            runs.path(),
        );

        assert_eq!(restored.len(), 1);
        // The valid prefix folds → resume still uses the journal's fresh state.
        assert_restored_from_archive(&world, restored[0].1.entity());
    }

    /// Append raw journal records to an existing `run.lvr`, the way the live
    /// lane journals a batch dispatch and its per-call completions.
    fn append_archive_records(
        runs_dir: &Path,
        run_id: &str,
        records: &[leviath_core::run_archive::RunRecord],
    ) {
        use std::io::Write;
        let mut buf = Vec::new();
        for r in records {
            leviath_core::run_archive::write_record(&mut buf, r).unwrap();
        }
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(runs_dir.join(run_id).join("run.lvr"))
            .unwrap();
        f.write_all(&buf).unwrap();
    }

    fn batch_call(
        id: &str,
        name: &str,
        result: Option<&str>,
    ) -> leviath_core::run_archive::ToolCallRecord {
        leviath_core::run_archive::ToolCallRecord {
            id: id.to_string(),
            name: name.to_string(),
            arguments: "{}".to_string(),
            result: result.map(str::to_string),
            thought_signature: None,
        }
    }

    /// The conversation entries of a reloaded agent's window.
    fn conversation_of(world: &PipelineWorld, entity: Entity) -> Vec<leviath_core::RegionEntry> {
        world
            .world()
            .get::<leviath_runtime::components::ContextWindow>(entity)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .content
            .clone()
    }

    /// The #96 crash-resume path end to end: a batch was dispatched (journaled),
    /// one call completed (journaled), one didn't, and the daemon died before
    /// the batch applied. Reload replays the recorded result and synthesizes a
    /// verify-first error for the lost one - and the agent re-infers from there
    /// instead of re-executing the batch.
    #[tokio::test]
    async fn reload_replays_a_pending_tool_batch_instead_of_reexecuting() {
        use leviath_core::run_archive::RunRecord;
        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let mpath = manifest.to_str().unwrap();
        let runs = tempfile::tempdir().unwrap();

        write_run(runs.path(), "run-batch", mpath, RunStatus::Running, None);
        let ctx = ContextSnapshot {
            stage_name: "implement".to_string(),
            total_tokens: 0,
            max_tokens: 100_000,
            regions: vec![],
        };
        write_run_archive(runs.path(), "run-batch", mpath, 0, 9, 99, &ctx);
        append_archive_records(
            runs.path(),
            "run-batch",
            &[
                RunRecord::ToolBatch {
                    calls: vec![
                        batch_call("c_done", "write_file", None),
                        batch_call("c_lost", "shell", None),
                    ],
                    at: 3,
                    stage_index: 0,
                    iteration: 9,
                    response: "writing then running".to_string(),
                },
                RunRecord::ToolCallDone {
                    iteration: 9,
                    call_id: "c_done".to_string(),
                    result: "Wrote 42 bytes to x.txt".to_string(),
                    at: 4,
                },
            ],
        );

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 999,
                subagent_tx: sub_tx().clone(),
            },
            runs.path(),
        );

        assert_eq!(restored.len(), 1);
        let entity = restored[0].1;
        let entries = conversation_of(&world, entity.entity());
        // The assistant turn landed with both calls...
        assert!(entries.iter().any(|e| matches!(
            &e.kind,
            leviath_core::region::EntryKind::AssistantTurn { tool_calls } if tool_calls.len() == 2
        )));
        // ...the completed call keeps its real journaled result...
        assert!(
            entries
                .iter()
                .any(|e| e.content == "Wrote 42 bytes to x.txt")
        );
        // ...and the lost call gets the verify-first synthesis.
        assert!(entries.iter().any(|e| e.content.contains("interrupted")
            && e.content.contains("Verify whether it took effect")));
        // The agent re-infers from the reconstructed window.
        assert!(
            world
                .world()
                .get::<leviath_runtime::pipeline::ReadyToInfer>(entity.entity())
                .is_some()
        );
    }

    /// The batch's assistant turn already reached the persisted window before
    /// the crash (apply_tool_results ran; the Progress record landed): fold
    /// clears the pending batch, so reload appends nothing a second time.
    #[tokio::test]
    async fn reload_does_not_replay_a_batch_already_in_the_window() {
        use leviath_core::region::EntryKind;
        use leviath_core::run_archive::RunRecord;
        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let mpath = manifest.to_str().unwrap();
        let runs = tempfile::tempdir().unwrap();

        write_run(runs.path(), "run-applied", mpath, RunStatus::Running, None);
        // The archived window already holds the batch's turn + paired result.
        let ctx = ContextSnapshot {
            stage_name: "implement".to_string(),
            total_tokens: 2,
            max_tokens: 100_000,
            regions: vec![leviath_core::run_meta::RegionSnapshot {
                name: "conversation".to_string(),
                kind: "clearable".to_string(),
                current_tokens: 2,
                max_tokens: 100_000,
                entries: vec![
                    leviath_core::run_meta::RegionEntrySnapshot {
                        content: "done".to_string(),
                        tokens: 1,
                        kind: EntryKind::AssistantTurn {
                            tool_calls: vec![leviath_core::region::SerializedToolCall {
                                id: "c1".to_string(),
                                name: "write_file".to_string(),
                                arguments: serde_json::Value::Null,
                                thought_signature: None,
                            }],
                        },
                        metadata: None,
                        key: None,
                        taint: Default::default(),
                    },
                    leviath_core::run_meta::RegionEntrySnapshot {
                        content: "Wrote it".to_string(),
                        tokens: 1,
                        kind: EntryKind::ToolResult {
                            tool_call_id: "c1".to_string(),
                            tool_name: "write_file".to_string(),
                            is_error: false,
                        },
                        metadata: None,
                        key: None,
                        taint: Default::default(),
                    },
                ],
                description: None,
            }],
        };
        write_run_archive(runs.path(), "run-applied", mpath, 0, 9, 99, &ctx);
        append_archive_records(
            runs.path(),
            "run-applied",
            &[RunRecord::ToolBatch {
                calls: vec![batch_call("c1", "write_file", None)],
                at: 3,
                stage_index: 0,
                iteration: 9,
                response: "done".to_string(),
            }],
        );

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 999,
                subagent_tx: sub_tx().clone(),
            },
            runs.path(),
        );

        assert_eq!(restored.len(), 1);
        let entries = conversation_of(&world, restored[0].1.entity());
        // Exactly the persisted turn - no second copy, no interrupted synthesis.
        assert_eq!(
            entries
                .iter()
                .filter(|e| matches!(&e.kind, EntryKind::AssistantTurn { tool_calls } if !tool_calls.is_empty()))
                .count(),
            1
        );
        assert!(!entries.iter().any(|e| e.content.contains("interrupted")));
    }

    /// A temp agent dir holding the interactive planning manifest, whose stage 0
    /// (`plan`) is an `interactive_points` stage with a `plan_approval` point.
    fn interactive_agent_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("agent.leviath"),
            crate::test_support::inline_interactive_manifest(),
        )
        .unwrap();
        dir
    }

    #[tokio::test]
    async fn reload_resumes_a_blocked_interaction_point_in_the_waiting_state() {
        let agent = interactive_agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let runs = tempfile::tempdir().unwrap();

        // A run parked at the plan_approval interaction point (stage 0 = plan)...
        write_run(
            runs.path(),
            "run-await",
            manifest.to_str().unwrap(),
            RunStatus::WaitingInput,
            None,
        );
        // ...plus the interaction sidecar the daemon wrote while it was blocked.
        std::fs::write(
            runs.path().join("run-await/interactions.json"),
            serde_json::to_string(&InteractionPointState {
                cursor: 0,
                round: 0,
                body: "## Plan\n1. do it".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        world.insert_interaction_hub(hub.clone()); // restore reads the hub resource
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 999,
                subagent_tx: sub_tx().clone(),
            },
            runs.path(),
        );

        assert_eq!(restored.len(), 1);
        let (run_id, entity) = &restored[0];
        assert_eq!(run_id, "run-await");
        // Re-armed in the *waiting* state (not the default Active), so no inference
        // re-issues and the open prompt isn't dropped - the issue #38 fix.
        assert_eq!(world.agent_status(*entity), Some(AgentStatus::Waiting));
        assert!(
            world
                .world()
                .get::<leviath_runtime::interaction_points::AwaitingInteractionPoint>(
                    entity.entity()
                )
                .is_some()
        );
        assert!(
            world
                .world()
                .get::<leviath_runtime::pipeline::ReadyToInfer>(entity.entity())
                .is_none(),
            "the spawn-set ReadyToInfer is cleared so the inference lane won't fire"
        );

        // The prompt was re-opened in the hub, carrying the reviewed plan.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let pending = hub.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, "run-await");
        assert_eq!(pending[0].1.body.as_deref(), Some("## Plan\n1. do it"));
    }

    #[tokio::test]
    async fn reload_restores_actionable_runs_before_blocked_and_skips_terminal() {
        let agent = agent_dir();
        let mpath = agent.path().join("agent.leviath");
        let mpath = mpath.to_str().unwrap();
        let runs = tempfile::tempdir().unwrap();
        // Directory iteration order is unspecified; name the blocked run so it would
        // sort ahead alphabetically, proving the triage (not the filesystem) decides.
        write_run(
            runs.path(),
            "aaa-blocked",
            mpath,
            RunStatus::WaitingInput,
            None,
        );
        write_run(runs.path(), "zzz-active", mpath, RunStatus::Running, None);
        write_run(runs.path(), "mmm-done", mpath, RunStatus::Complete, None);

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 999,
                subagent_tx: sub_tx().clone(),
            },
            runs.path(),
        );

        // Terminal run skipped; the actionable (Running) run is restored first.
        let order: Vec<&str> = restored.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(order, vec!["zzz-active", "aaa-blocked"]);
    }

    /// #576: cancelling stops a run, it does not end it. Everything needed to
    /// carry on is on disk, so `lev resume` has to be able to reach it, which
    /// means it has to page back in.
    #[tokio::test]
    async fn reload_run_pages_in_a_cancelled_run_but_not_a_finished_one() {
        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let mpath = manifest.to_str().unwrap();
        let runs = tempfile::tempdir().unwrap();
        write_run(runs.path(), "stopped", mpath, RunStatus::Cancelled, None);
        write_run(runs.path(), "died", mpath, RunStatus::Error, None);

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let config = Config::default();
        let owners = Default::default();
        let deps = |mcp: Arc<Mutex<ToolExecutor>>| crate::daemon::spawn::SpawnDeps {
            tool_service: cli.as_ref(),
            config: &config,
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &owners,
            hub: &hub,
            now_secs: 1,
            subagent_tx: sub_tx().clone(),
        };

        assert!(
            reload_run(&mut world, deps(mcp.clone()), "stopped", runs.path()).is_some(),
            "a cancelled run keeps its journal, context and stage, so it pages in"
        );
        assert!(
            reload_run(&mut world, deps(mcp.clone()), "died", runs.path()).is_none(),
            "a run that errored is read before it is continued, not resumed blind"
        );
    }

    #[tokio::test]
    async fn reload_run_pages_in_nonterminal_only() {
        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let mpath = manifest.to_str().unwrap();
        let runs = tempfile::tempdir().unwrap();
        write_run(runs.path(), "live", mpath, RunStatus::Running, None);
        write_run(runs.path(), "done", mpath, RunStatus::Complete, None);

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));

        // A non-terminal run is paged in.
        assert!(
            reload_run(
                &mut world,
                crate::daemon::spawn::SpawnDeps {
                    tool_service: cli.as_ref(),
                    config: &Config::default(),
                    shared_mcp: mcp.clone(),
                    mcp_tool_defs: &[],
                    mcp_tool_owners: &Default::default(),
                    hub: &hub,
                    now_secs: 1,
                    subagent_tx: sub_tx().clone(),
                },
                "live",
                runs.path(),
            )
            .is_some()
        );
        // A terminal run is not.
        assert!(
            reload_run(
                &mut world,
                crate::daemon::spawn::SpawnDeps {
                    tool_service: cli.as_ref(),
                    config: &Config::default(),
                    shared_mcp: mcp.clone(),
                    mcp_tool_defs: &[],
                    mcp_tool_owners: &Default::default(),
                    hub: &hub,
                    now_secs: 1,
                    subagent_tx: sub_tx().clone(),
                },
                "done",
                runs.path(),
            )
            .is_none()
        );
        // A run with no meta on disk is not.
        assert!(
            reload_run(
                &mut world,
                crate::daemon::spawn::SpawnDeps {
                    tool_service: cli.as_ref(),
                    config: &Config::default(),
                    shared_mcp: mcp,
                    mcp_tool_defs: &[],
                    mcp_tool_owners: &Default::default(),
                    hub: &hub,
                    now_secs: 1,
                    subagent_tx: sub_tx().clone(),
                },
                "no-such-run",
                runs.path(),
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn resumes_a_parent_parked_mid_fan_out() {
        use leviath_core::blueprint::{FanOutConfig, WorkerFailurePolicy};
        use leviath_runtime::fanout::{FanOutState, FanOutWaiting};

        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let mpath = manifest.to_str().unwrap();
        let runs = tempfile::tempdir().unwrap();

        // A parent parked mid fan-out: a valid fanout.json alongside its meta.
        write_run(
            runs.path(),
            "parent-fo",
            mpath,
            RunStatus::WaitingInput,
            None,
        );
        let state = FanOutState {
            origin: leviath_runtime::fanout::FanOutOrigin::Stage,
            config: FanOutConfig {
                worker_agent: None,
                worker_stage: Some("w".to_string()),
                worker_query: None,
                merge_stage: None,
                max_workers: 1,
                on_worker_failure: WorkerFailurePolicy::Continue,
                split_prompt: "s".to_string(),
                results_region: None,
                max_items: None,
                max_attempts: None,
            },
            max_workers: 1,
            pending: vec![],
            // One in-flight worker, referenced by the run-id of another reloaded
            // run so the resolver maps it back to an entity on restore.
            active: vec![("item-1".to_string(), "worker-fo".to_string())],
            summaries: vec![],
            failures: vec![],
            paused: false,
        };
        std::fs::write(
            runs.path().join("parent-fo").join("fanout.json"),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();
        // The referenced worker run, so the active worker re-links to a real entity.
        write_run(runs.path(), "worker-fo", mpath, RunStatus::Running, None);

        // A run with a malformed fanout.json → skipped (no FanOutWaiting).
        write_run(runs.path(), "bad-fo", mpath, RunStatus::WaitingInput, None);
        std::fs::write(runs.path().join("bad-fo").join("fanout.json"), b"garbage").unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 999,
                subagent_tx: sub_tx().clone(),
            },
            runs.path(),
        );
        let by_id: std::collections::HashMap<_, _> =
            restored.iter().map(|(r, e)| (r.clone(), *e)).collect();

        // The parent's fan-out waiting state was rebuilt; the malformed one wasn't.
        assert!(
            world
                .world()
                .get::<FanOutWaiting>(by_id["parent-fo"].entity())
                .is_some()
        );
        assert!(
            world
                .world()
                .get::<FanOutWaiting>(by_id["bad-fo"].entity())
                .is_none()
        );
    }

    #[tokio::test]
    async fn rebuilds_parent_child_tree_on_reload() {
        use leviath_runtime::components::{ParentRef, SubAgentChildren};

        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let mpath = manifest.to_str().unwrap();
        let runs = tempfile::tempdir().unwrap();

        // A parent with two children + a child that records its parent + depth.
        write_run_tree(RunFixture {
            runs_dir: runs.path(),
            run_id: "parent",
            agent_path: mpath,
            status: RunStatus::WaitingInput,
            context: None,
            parent_run_id: None,
            children: &["child-a", "child-b"],
            depth: 0,
            max_child_depth: 4,
        });
        write_run_tree(RunFixture {
            runs_dir: runs.path(),
            run_id: "child-a",
            agent_path: mpath,
            status: RunStatus::Running,
            context: None,
            parent_run_id: Some("parent"),
            children: &[],
            depth: 1,
            max_child_depth: 0,
        });
        write_run_tree(RunFixture {
            runs_dir: runs.path(),
            run_id: "child-b",
            agent_path: mpath,
            status: RunStatus::Running,
            context: None,
            parent_run_id: Some("parent"),
            children: &[],
            depth: 1,
            max_child_depth: 0,
        });

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 999,
                subagent_tx: sub_tx().clone(),
            },
            runs.path(),
        );
        assert_eq!(restored.len(), 3);
        let by_id: std::collections::HashMap<_, _> =
            restored.iter().map(|(r, e)| (r.clone(), *e)).collect();
        let parent = by_id["parent"];
        let child_a = by_id["child-a"];
        let child_b = by_id["child-b"];

        // Parent's SubAgentChildren rebuilt with both children + the depth cap.
        let kids = world
            .world()
            .get::<SubAgentChildren>(parent.entity())
            .unwrap();
        assert_eq!(kids.max_child_depth, 4);
        assert_eq!(kids.children.len(), 2);
        assert!(
            kids.children.contains(&child_a.entity()) && kids.children.contains(&child_b.entity())
        );
        // Each child's ParentRef points back at the parent, at its stored depth.
        let pr = world.world().get::<ParentRef>(child_a.entity()).unwrap();
        assert_eq!(pr.parent_entity, parent.entity());
        assert_eq!(pr.parent_agent_id, "parent");
        assert_eq!(pr.depth, 1);
        // The serializable child list is kept in sync for the next snapshot.
        let state = world
            .world()
            .get::<leviath_runtime::components::AgentState>(parent.entity())
            .unwrap();
        assert_eq!(state.spawned_children_ids, vec!["child-a", "child-b"]);
    }

    #[tokio::test]
    async fn relink_skips_children_and_parents_that_did_not_reload() {
        use leviath_runtime::components::{ParentRef, SubAgentChildren};

        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let mpath = manifest.to_str().unwrap();
        let runs = tempfile::tempdir().unwrap();

        // Parent lists a child that is terminal (won't reload) → no SubAgentChildren.
        write_run_tree(RunFixture {
            runs_dir: runs.path(),
            run_id: "lonely-parent",
            agent_path: mpath,
            status: RunStatus::WaitingInput,
            context: None,
            parent_run_id: None,
            children: &["gone-child"],
            depth: 0,
            max_child_depth: 2,
        });
        write_run_tree(RunFixture {
            runs_dir: runs.path(),
            run_id: "gone-child",
            agent_path: mpath,
            status: RunStatus::Complete,
            context: // terminal → skipped by recovery
            None,
            parent_run_id: Some("lonely-parent"),
            children: &[],
            depth: 1,
            max_child_depth: 0,
        });
        // Child whose parent is terminal (won't reload) → left unlinked.
        write_run_tree(RunFixture {
            runs_dir: runs.path(),
            run_id: "orphan",
            agent_path: mpath,
            status: RunStatus::Running,
            context: None,
            parent_run_id: Some("gone-parent"),
            children: &[],
            depth: 1,
            max_child_depth: 0,
        });
        write_run_tree(RunFixture {
            runs_dir: runs.path(),
            run_id: "gone-parent",
            agent_path: mpath,
            status: RunStatus::Error,
            context: None,
            parent_run_id: None,
            children: &["orphan"],
            depth: 0,
            max_child_depth: 2,
        });

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 999,
                subagent_tx: sub_tx().clone(),
            },
            runs.path(),
        );
        // Only the two non-terminal runs reload.
        assert_eq!(restored.len(), 2);
        let by_id: std::collections::HashMap<_, _> =
            restored.iter().map(|(r, e)| (r.clone(), *e)).collect();
        // Parent listed a child that didn't reload → no SubAgentChildren attached.
        assert!(
            world
                .world()
                .get::<SubAgentChildren>(by_id["lonely-parent"].entity())
                .is_none()
        );
        // Orphan's parent didn't reload → no ParentRef attached.
        assert!(
            world
                .world()
                .get::<ParentRef>(by_id["orphan"].entity())
                .is_none()
        );
    }

    #[tokio::test]
    async fn reload_without_context_json_still_resumes() {
        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let runs = tempfile::tempdir().unwrap();
        write_run(
            runs.path(),
            "run-nocontext",
            manifest.to_str().unwrap(),
            RunStatus::WaitingInput,
            None, // no context.json
        );

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 999,
                subagent_tx: sub_tx().clone(),
            },
            runs.path(),
        );
        assert_eq!(restored.len(), 1);
        assert!(
            world
                .world()
                .get::<TokenTotals>(restored[0].1.entity())
                .is_some()
        );
    }

    /// A restart used to bring every stage back at zero: nothing rebuilt the
    /// ledger from `stages.json`, so the blueprint-seeded (all-zero) one was
    /// written straight over the real file on the next persist tick, and the
    /// run's whole per-stage history went with it while `meta.json` still
    /// looked healthy (issue #415).
    /// A run parked until the machine is fixed keeps its reason across a
    /// daemon restart.
    ///
    /// The marker is a live component, so without restoring it the run comes
    /// back as a bare `Paused` and the next persist tick recomputes
    /// `waiting_on` from markers that are gone - writing `null` over the
    /// reason rather than merely failing to show it. Nothing recomputes it
    /// either, because a paused run is never dispatched.
    #[tokio::test]
    async fn reload_keeps_the_reason_a_parked_run_was_parked_for() {
        use leviath_core::run_meta::{SetupBlocker, WaitReason};

        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let runs = tempfile::tempdir().unwrap();
        write_run(
            runs.path(),
            "run-parked",
            manifest.to_str().unwrap(),
            RunStatus::Paused,
            None,
        );
        // Rewrite meta with the reason the run stopped for.
        let meta_path = runs.path().join("run-parked").join("meta.json");
        let mut meta: leviath_core::run_meta::RunMeta =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta.waiting_on = Some(WaitReason::NeedsSetup {
            blocker: SetupBlocker::ProviderMissing,
            remedy: "add it to config.toml".to_string(),
        });
        std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 999,
                subagent_tx: sub_tx().clone(),
            },
            runs.path(),
        );
        assert_eq!(restored.len(), 1);

        let parked = world
            .world()
            .get::<leviath_runtime::pipeline::PausedForSetup>(restored[0].1.entity())
            .expect("the reason came back with the run");
        assert_eq!(parked.blocker, SetupBlocker::ProviderMissing);
        assert_eq!(parked.remedy, "add it to config.toml");
    }

    #[tokio::test]
    async fn reload_restores_the_persisted_stage_ledger() {
        use leviath_core::run_meta::{StageRecord, StageRunStatus};
        use leviath_runtime::pipeline::StageLedger;

        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let runs = tempfile::tempdir().unwrap();
        write_run(
            runs.path(),
            "run-stages",
            manifest.to_str().unwrap(),
            RunStatus::Running,
            None,
        );
        // The ledger as the run left it: `analyze` ran and finished, the run
        // stopped in `implement`, `review` was never reached. The trailing
        // record names a stage this blueprint no longer has.
        let mut analyze = StageRecord::new("analyze".to_string(), 0);
        analyze.status = StageRunStatus::Complete;
        analyze.entered = true;
        analyze.prompt_tokens = 1_234;
        analyze.completion_tokens = 56;
        analyze.cached_tokens = 7;
        analyze.cache_write_tokens = 8;
        analyze.first_call_prompt_tokens = Some(400);
        analyze.runaway_warned = true;
        analyze
            .region_tokens
            .insert("conversation".to_string(), 900);
        analyze.started_at = Some(10);
        analyze.ended_at = Some(20);
        analyze.active = Some(leviath_core::run_meta::ActiveClock {
            banked_secs: 8,
            since: None,
        });
        // One closed stay, priced, so the money survives the reload rather than
        // restarting from zero the way #415 restarted the tokens.
        analyze.begin_visit(10);
        analyze.record_call(
            &leviath_core::run_meta::StageCall {
                prompt_tokens: 1_234,
                cost_usd: Some(0.5),
                cost_reported: true,
                ..Default::default()
            },
            12,
        );
        analyze.close_visit(20);
        analyze.prompt_tokens = 1_234;

        let mut implement = StageRecord::new("implement".to_string(), 1);
        implement.status = StageRunStatus::Active;
        implement.entered = true;
        implement.prompt_tokens = 77;
        implement.started_at = Some(20);
        // The stage the run was in when the daemon died, its span still open.
        implement.active = Some(leviath_core::run_meta::ActiveClock {
            banked_secs: 3,
            since: Some(200),
        });
        // And so is the visit it was on, with a clock of its own left running.
        implement.begin_visit(20);
        implement.visits[0].active = Some(leviath_core::run_meta::ActiveClock {
            banked_secs: 3,
            since: Some(200),
        });
        let removed = StageRecord::new("removed_stage".to_string(), 7);
        std::fs::write(
            runs.path().join("run-stages").join("stages.json"),
            serde_json::to_string(&vec![analyze, implement, removed]).unwrap(),
        )
        .unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 999,
                subagent_tx: sub_tx().clone(),
            },
            runs.path(),
        );
        assert_eq!(restored.len(), 1);

        let ledger = world
            .world()
            .get::<StageLedger>(restored[0].1.entity())
            .expect("a reloaded agent carries a stage ledger");
        // One record per blueprint stage, in blueprint order: the record for a
        // stage that no longer exists is dropped rather than appended.
        let names: Vec<&str> = ledger.0.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["analyze", "implement", "review"]);
        assert_eq!(ledger.0[0].prompt_tokens, 1_234);
        assert_eq!(ledger.0[0].completion_tokens, 56);
        assert_eq!(ledger.0[0].cached_tokens, 7);
        assert_eq!(ledger.0[0].cache_write_tokens, 8);
        // A finished stage's clock comes back untouched; the one left open is
        // closed at the run's `updated_at` (222), not carried on to now (999).
        assert_eq!(ledger.0[0].active_runtime_secs(999), 8);
        assert_eq!(ledger.0[1].active_runtime_secs(999), 3 + 22);
        // The money comes back with the tokens. Restarting a stage's cost at
        // zero on a reload is the same bug #415 was about, one column over.
        assert_eq!(ledger.0[0].cost_usd, Some(0.5));
        assert!(ledger.0[0].cost_is_exact);
        assert_eq!(ledger.0[0].visits.len(), 1);
        assert_eq!(ledger.0[0].visits[0].cost_usd, Some(0.5));
        // The stay the daemon died on stays open - a resume is not a re-entry -
        // and its clock is settled at `updated_at` (222) for the same reason the
        // stage's is: the span ended when the process holding it did.
        let open = &ledger.0[1].visits[0];
        assert_eq!(open.left_at, None, "still in that stage");
        assert_eq!(open.active_runtime_secs(999), 3 + 22);
        assert_eq!(ledger.0[0].first_call_prompt_tokens, Some(400));
        assert!(ledger.0[0].runaway_warned);
        assert_eq!(ledger.0[0].region_tokens.get("conversation"), Some(&900));
        assert_eq!(ledger.0[0].started_at, Some(10));
        assert_eq!(ledger.0[0].ended_at, Some(20));
        assert_eq!(ledger.0[0].status, StageRunStatus::Complete);
        assert!(ledger.0[0].entered);
        assert_eq!(ledger.0[1].prompt_tokens, 77);
        assert!(ledger.0[1].entered);
        // A stage with nothing persisted against it keeps the seeded record.
        assert_eq!(ledger.0[2].prompt_tokens, 0);
        assert!(!ledger.0[2].entered);
        assert_eq!(ledger.0[2].index, 2);
    }

    #[tokio::test]
    async fn skips_missing_dir_junk_and_unreloadable_runs() {
        // A runs dir that doesn't exist → empty.
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        assert!(
            reload_persisted_agents(
                &mut world,
                crate::daemon::spawn::SpawnDeps {
                    tool_service: cli.as_ref(),
                    config: &Config::default(),
                    shared_mcp: mcp.clone(),
                    mcp_tool_defs: &[],
                    mcp_tool_owners: &Default::default(),
                    hub: &hub,
                    now_secs: 1,
                    subagent_tx: sub_tx().clone(),
                },
                std::path::Path::new("/no/such/runs/dir"),
            )
            .is_empty()
        );

        // A runs dir with junk: a dir without meta.json, a dir with corrupt
        // meta.json, and a non-terminal run pointing at a missing blueprint.
        let runs = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(runs.path().join("no-meta")).unwrap();
        let corrupt = runs.path().join("corrupt");
        std::fs::create_dir_all(&corrupt).unwrap();
        std::fs::write(corrupt.join("meta.json"), "not json").unwrap();
        write_run(
            runs.path(),
            "run-badpath",
            "/no/such/agent.leviath",
            RunStatus::Running,
            None,
        );

        let restored = reload_persisted_agents(
            &mut world,
            crate::daemon::spawn::SpawnDeps {
                tool_service: cli.as_ref(),
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                now_secs: 1,
                subagent_tx: sub_tx().clone(),
            },
            runs.path(),
        );
        assert!(restored.is_empty()); // all skipped, none fatal

        // The un-reloadable run is recorded as crashed rather than left claiming
        // it is still running (issue #109) - `lev ps` and the dashboard would
        // otherwise show a live run that no longer exists.
        let meta: RunMeta = serde_json::from_str(
            &std::fs::read_to_string(runs.path().join("run-badpath").join("meta.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(meta.status, RunStatus::Error);
        let error = meta.error.unwrap_or_default();
        assert!(error.contains("could not be recovered"), "got: {error}");
        assert_eq!(meta.updated_at, 1);
        // Junk that never parsed as a run has nothing to rewrite.
        assert!(!runs.path().join("no-meta").join("meta.json").exists());
        assert_eq!(
            std::fs::read_to_string(corrupt.join("meta.json")).unwrap(),
            "not json"
        );
    }

    #[test]
    fn marking_a_crash_is_best_effort() {
        // The run directory can vanish between the scan and the rewrite (a
        // concurrent `lev rm`, a wiped runs dir). Recovery must log and carry
        // on - the daemon is mid-startup and the other runs still need it.
        let runs = tempfile::tempdir().unwrap();
        write_run(
            runs.path(),
            "run-x",
            "/no/such/agent.leviath",
            RunStatus::Running,
            None,
        );
        let meta = read_meta(&runs.path().join("run-x")).expect("written above");
        mark_crashed(&runs.path().join("gone"), meta, "boom", 7);
        assert!(!runs.path().join("gone").exists());
    }

    #[tokio::test]
    async fn fake_provider_methods_are_exercised() {
        use leviath_providers::Provider;
        let p = FakeProvider;
        assert_eq!(p.name(), "fake");
        assert_eq!(p.count_tokens("t", "m").await, 1);
        assert_eq!(p.max_context_tokens("m"), 1000);
        let _ = p.capabilities("m");
        assert!(
            p.infer(&leviath_providers::InferenceRequest {
                system: vec![],
                messages: vec![],
                model: "m".to_string(),
                max_tokens: 1,
                temperature: 0.0,
                tools: vec![],
                extra: serde_json::Value::Null,
                request_timeout_secs: None,
            })
            .await
            .is_err()
        );
    }

    #[test]
    fn is_finished_covers_all_statuses() {
        assert!(is_finished(&RunStatus::Complete));
        assert!(is_finished(&RunStatus::Error));
        // The point of #576: a cancelled run stopped, it did not end, and
        // everything it needs to carry on is still on disk.
        assert!(!is_finished(&RunStatus::Cancelled));
        assert!(!is_finished(&RunStatus::Running));
        assert!(!is_finished(&RunStatus::WaitingInput));
    }
}
