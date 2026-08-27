//! Restart recovery: bring a freshly-spawned agent back to its persisted running
//! state so the daemon resumes it where it stopped.
//!
//! When the daemon restarts, the CLI reloads each non-terminal run's blueprint
//! and spawns a fresh agent, then calls [`restore_agent`] to overlay the persisted
//! context, jump to the persisted stage + iteration, and restore token totals,
//! plus [`restore_stage_ledger`] for the run's per-stage history. The
//! agent keeps the `ReadyToInfer` marker `spawn_agent` set, so **any inference
//! that was in flight when the daemon stopped is re-issued** on the next tick -
//! nothing is left stuck awaiting a job that died with the old process.
//!
//! A tool batch that was in flight is not blindly re-issued, though: when the run
//! journal holds a dispatched-but-unapplied batch, [`restore_pending_batch`]
//! reconstructs its assistant turn in the window first - real journaled results
//! for calls that completed, a verify-first [`INTERRUPTED_TOOL_RESULT`] for calls
//! that didn't - so the re-issued inference sees exactly what already ran and
//! completed side effects never run twice (issue #96).

use bevy_ecs::prelude::*;
use leviath_core::region::RegionEntry;
use leviath_core::run_meta::{ContextSnapshot, RunMeta, RunStatus};

use crate::components::{AgentState, AgentStatus, ContextWindow};
use crate::persistence::TokenTotals;
use crate::pipeline::{StageCursor, StageInferences, StageSetups};

/// How urgently a persisted run should be brought back on restart. Ordered so a
/// higher value restores first (see [`triage_restores`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RestorePriority {
    /// Restorable, but can make no immediate progress: blocked on user input,
    /// done-but-interactive (awaiting optional follow-up), or a parent parked mid
    /// fan-out waiting on its children. Brought back after the actionable runs.
    Blocked,
    /// Actionable now: an in-flight inference to re-dispatch or pending tool
    /// results to process. These resume real work the moment they're reloaded, so
    /// they come back first.
    Active,
}

/// Classify one persisted run for restart recovery from its on-disk status and
/// whether it is parked mid fan-out (a `<run_dir>/fanout.json` is present).
///
/// Returns `None` for a **terminal** run (`Complete` / `Error` / `Cancelled`) -
/// those are never resumed. A run parked on a fan-out is [`Blocked`] regardless of
/// its status: it can't progress until its children finish.
///
/// [`Blocked`]: RestorePriority::Blocked
pub fn classify_restore(status: &RunStatus, parked_on_fanout: bool) -> Option<RestorePriority> {
    match status {
        RunStatus::Complete | RunStatus::Error | RunStatus::Cancelled => None,
        _ if parked_on_fanout => Some(RestorePriority::Blocked),
        RunStatus::Starting | RunStatus::Running => Some(RestorePriority::Active),
        RunStatus::WaitingInput | RunStatus::CompleteInteractive | RunStatus::Paused => {
            Some(RestorePriority::Blocked)
        }
    }
}

/// Triage a set of persisted runs into the order they should be restored on
/// restart: drop terminal runs, then rank the rest **actionable-first**
/// ([`RestorePriority::Active`] before [`Blocked`]), breaking ties by most-recently
/// updated. Each input is `(meta, parked_on_fanout)` where `parked_on_fanout` is
/// whether the run has a `fanout.json` (see [`classify_restore`]); the returned
/// [`RunMeta`]s are ready to reload in order.
///
/// This lets a resource- or time-constrained caller restore only a prefix (the most
/// actionable agents) and still make the most progress possible.
///
/// [`Blocked`]: RestorePriority::Blocked
pub fn triage_restores(candidates: Vec<(RunMeta, bool)>) -> Vec<RunMeta> {
    let mut ranked: Vec<(RestorePriority, RunMeta)> = candidates
        .into_iter()
        .filter_map(|(meta, parked)| {
            classify_restore(&meta.status, parked).map(|prio| (prio, meta))
        })
        .collect();
    // Higher priority first; within a tier, most-recently updated first. `sort_by`
    // is stable, so equal keys keep their scan order.
    ranked.sort_by(|(a_prio, a), (b_prio, b)| {
        b_prio
            .cmp(a_prio)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
    });
    ranked.into_iter().map(|(_, meta)| meta).collect()
}

/// Restore a just-spawned `entity` to the persisted state captured in `snapshot`
/// (its context), `stage_index` + `iteration` (its position), and `totals` (its
/// running token/tool counts). The agent stays `Active` + `ReadyToInfer` so it
/// resumes on the next tick.
///
/// Context is overlaid by region **name**: each persisted region replaces the
/// matching window region's entries (rebuilt from the blueprint layout, so region
/// kinds/limits are correct). A persisted region with no matching window region
/// is skipped. An out-of-range `stage_index` (e.g. the blueprint gained/lost
/// stages) leaves the spawned stage-0 config in place.
pub fn restore_agent(
    world: &mut World,
    entity: Entity,
    snapshot: &ContextSnapshot,
    stage_index: usize,
    iteration: usize,
    totals: TokenTotals,
) {
    // 1. Overlay the persisted context onto the (blueprint-built) window.
    {
        let mut window = world
            .get_mut::<ContextWindow>(entity)
            .expect("a spawned agent has a context window");
        for snap_region in &snapshot.regions {
            if let Some(region) = window
                .regions
                .iter_mut()
                .find(|r| r.name == snap_region.name)
            {
                region.content = snap_region
                    .entries
                    .iter()
                    .map(|e| RegionEntry {
                        content: e.content.clone(),
                        tokens: e.tokens,
                        timestamp: 0,
                        metadata: e.metadata.clone(),
                        kind: e.kind.clone(),
                        key: e.key.clone(),
                    })
                    .collect();
                // Rebuild the taint alongside the content. Assigning `content`
                // directly bypasses `add_tainted_entry`, which is the only thing
                // that records per-entry taint - so without this the region came
                // back `Public` no matter how sensitive it had been, while the
                // gate reported itself armed.
                // Only where the region already tracks taint: restoring it onto
                // a region with tracking off would invent a level nothing reads.
                if region.taint.is_some() {
                    region.taint = Some(leviath_core::taint::RegionTaint::from_entry_taints(
                        snap_region.entries.iter().map(|e| e.taint).collect(),
                    ));
                }
                region.current_tokens = region.content.iter().map(|e| e.tokens).sum();
            }
        }
        window.current_tokens = window.calculate_tokens();
    }

    // 2. Jump to the persisted stage, swapping in its inference config and
    //    tool-result routing.
    if let Some(inf) = world
        .get::<StageInferences>(entity)
        .expect("a spawned agent has stage inferences")
        .0
        .get(stage_index)
        .cloned()
    {
        let setup = &world
            .get::<StageSetups>(entity)
            .expect("a spawned agent has stage setups")
            .0[stage_index];
        let cfg = setup.inference_config.clone();
        let routing = setup.routing.clone();
        world.entity_mut(entity).insert((inf, cfg));
        // Mirror `attach_stage_components`' routing arm: present ⇒ insert,
        // absent ⇒ clear the stale one. Without this a reloaded agent kept the
        // spawn stage's routing (or none) for every future tool batch.
        match routing {
            Some(routing) => {
                world
                    .entity_mut(entity)
                    .insert(crate::components::ToolResultRoutingComponent { routing });
            }
            None => {
                world
                    .entity_mut(entity)
                    .remove::<crate::components::ToolResultRoutingComponent>();
            }
        }
        world
            .get_mut::<StageCursor>(entity)
            .expect("a spawned agent has a stage cursor")
            .index = stage_index;
    }

    // 3. Restore the agent's running state + token totals.
    {
        let mut state = world
            .get_mut::<AgentState>(entity)
            .expect("a spawned agent has state");
        state.current_stage = snapshot.stage_name.clone();
        state.iteration = iteration;
        state.status = AgentStatus::Active;
    }
    world.entity_mut(entity).insert(totals);
}

/// Put the persisted per-stage ledger back on a just-spawned `entity`, matching
/// `records` (as read from the run's `stages.json`) onto the blueprint-seeded
/// [`StageLedger`](crate::pipeline::StageLedger) **by stage name**.
///
/// Nothing else rebuilds this. `spawn_agent` seeds one all-zero record per
/// blueprint stage, so without this a reloaded run came back with no tokens, no
/// `entered` flags and no timestamps against any stage - and since the persist
/// tick writes the whole ledger, the next one wrote those zeros over the real
/// `stages.json`. The run-level totals in `meta.json` survived that, so the run
/// looked healthy while `lev stages` and the stages API served zeroed records
/// (issue #415).
///
/// The seeded shape wins: a persisted record whose stage the blueprint no longer
/// has is dropped, and a stage with no persisted record keeps its zeroed one.
/// Matching on name rather than position is what keeps a blueprint that gained
/// or lost a stage from filing one stage's history under another; the seeded
/// `index` is kept for the same reason.
///
/// Call after [`restore_agent`]. An agent without a ledger (a test world, or one
/// spawned outside the blueprint path) is left alone.
pub fn restore_stage_ledger(
    world: &mut World,
    entity: Entity,
    records: &[leviath_core::run_meta::StageRecord],
) {
    let Some(mut ledger) = world.get_mut::<crate::pipeline::StageLedger>(entity) else {
        return;
    };
    for rec in ledger.0.iter_mut() {
        if let Some(saved) = records.iter().find(|saved| saved.name == rec.name) {
            let index = rec.index;
            *rec = saved.clone();
            rec.index = index;
        }
    }
}

/// The synthesized result for a call whose completion never reached the journal.
/// It tells the model plainly that the effect may or may not have landed, so the
/// re-issued turn verifies before re-running side-effecting work.
pub const INTERRUPTED_TOOL_RESULT: &str = "[error] interrupted: the daemon restarted while this tool call was executing and its \
     result was lost. Verify whether it took effect before re-running side-effecting work.";

/// The synthesized result for one interrupted call: the base text, plus - for a
/// sub-agent tool on a run with known children - the child runs to check before
/// spawning again. Mechanical dedupe is impossible here (the model mints a fresh
/// call id when it re-issues), so informed re-issue is the guarantee.
fn interrupted_result(tool_name: &str, children: &[String]) -> String {
    if leviath_tools::is_subagent_tool(tool_name) && !children.is_empty() {
        format!(
            "{INTERRUPTED_TOOL_RESULT} This run already has child agent runs: {}; check them \
             with check_agent before spawning again.",
            children.join(", ")
        )
    } else {
        INTERRUPTED_TOOL_RESULT.to_string()
    }
}

/// Replay a tool batch that was dispatched but never applied before the crash
/// (folded from the run journal as a
/// [`PendingToolBatch`](leviath_core::run_archive::PendingToolBatch)): land the
/// assistant turn plus one result per call in the context window, exactly as
/// `apply_tool_results` would have - real journaled results for calls that
/// finished, [`INTERRUPTED_TOOL_RESULT`] for calls that didn't. The turn is
/// always fully paired, so the request assembler's orphan sanitizer keeps it,
/// and the re-issued inference sees precisely what already ran instead of
/// blindly re-executing the whole batch (issue #96).
///
/// Call after [`restore_agent`], which swaps the restored stage's
/// `ToolResultRoutingComponent` in - the routing and per-tool sensitivities are
/// read off the entity so replayed results route and taint like live ones.
/// `children` is the run's known child-run ids (`meta.children`), folded into
/// the synthesized text of interrupted sub-agent calls. Secondary bookkeeping
/// (modification counters, telemetry, file tracking, log lines) is deliberately
/// skipped: totals and outcome flags are already restored from the persisted
/// metadata, and the dead process's calls have no live stage to report to.
pub fn restore_pending_batch(
    world: &mut World,
    entity: Entity,
    batch: &leviath_core::run_archive::PendingToolBatch,
    children: &[String],
) {
    let calls: Vec<crate::components::ToolCall> = batch
        .calls
        .iter()
        .map(|c| crate::components::ToolCall {
            tool_id: c.id.clone(),
            name: c.name.clone(),
            // Journaled arguments are stringified JSON; a record that doesn't
            // parse (torn write) survives as a raw string rather than dropping
            // the call and orphaning the turn.
            arguments: serde_json::from_str(&c.arguments)
                .unwrap_or_else(|_| serde_json::Value::String(c.arguments.clone())),
            thought_signature: c.thought_signature.clone(),
        })
        .collect();
    let merged: Vec<(String, String)> = batch
        .calls
        .iter()
        .map(|c| {
            let result = c
                .result
                .clone()
                .unwrap_or_else(|| interrupted_result(&c.name, children));
            (c.id.clone(), result)
        })
        .collect();
    let routing = world
        .get::<crate::components::ToolResultRoutingComponent>(entity)
        .map(|c| c.routing.clone());
    let sensitivities = world
        .get::<crate::pipeline::ToolSensitivities>(entity)
        .map(|s| s.0.clone());
    let mut window = world
        .get_mut::<ContextWindow>(entity)
        .expect("a spawned agent has a context window");
    crate::pipeline::apply_tool_results(
        &mut window,
        &batch.response,
        &calls,
        &merged,
        routing.as_ref(),
        sensitivities.as_ref(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::InferenceConfig;
    use crate::pipeline::{ReadyToInfer, StageInference, StageSetup};
    use leviath_core::region::EntryKind;
    use leviath_core::run_meta::{RegionEntrySnapshot, RegionSnapshot};
    use leviath_core::{Region, RegionKind};

    fn setup(temp: Option<f32>) -> StageSetup {
        StageSetup {
            inference_config: InferenceConfig {
                temperature: temp,
                max_output_tokens: None,
                extra_params: Default::default(),
                batch_tool_hint: false,
                shell_hint: false,
                request_timeout_secs: None,
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            context_hide: Vec::new(),
            system_prompt: None,
            output: None,
        }
    }

    fn si(model: &str) -> StageInference {
        StageInference {
            provider_name: "p".to_string(),
            model: model.to_string(),
            tools: vec![],
            tool_filter: None,
            fallbacks: Vec::new(),
            output: None,
        }
    }

    /// A world with one spawned-looking agent: a `conversation` region window,
    /// two stages, cursor at 0, `ReadyToInfer`.
    fn agent_world() -> (World, Entity) {
        let mut world = World::new();
        let mut window = ContextWindow::new(10_000);
        window.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        ));
        let _ = window.add_to_region("conversation", "fresh task seed".to_string(), 3);
        let entity = world
            .spawn((
                window,
                StageCursor { index: 0 },
                AgentState {
                    agent_id: "a".to_string(),
                    current_stage: "s0".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: vec![],
                    pending_wait: None,
                    accepts_messages: true,
                },
                StageInferences(vec![si("m0"), si("m1")]),
                StageSetups(vec![setup(None), setup(Some(0.5))]),
                si("m0"),
                setup(None).inference_config,
                TokenTotals::default(),
                ReadyToInfer,
            ))
            .id();
        (world, entity)
    }

    fn snapshot() -> ContextSnapshot {
        ContextSnapshot {
            stage_name: "s1".to_string(),
            total_tokens: 8,
            max_tokens: 10_000,
            regions: vec![
                RegionSnapshot {
                    name: "conversation".to_string(),
                    kind: "clearable".to_string(),
                    current_tokens: 8,
                    max_tokens: 10_000,
                    entries: vec![
                        RegionEntrySnapshot {
                            content: "prior user turn".to_string(),
                            tokens: 5,
                            kind: EntryKind::UserMessage,
                            metadata: None,
                            key: None,
                            taint: Default::default(),
                        },
                        RegionEntrySnapshot {
                            content: "prior assistant".to_string(),
                            tokens: 3,
                            kind: EntryKind::AssistantTurn { tool_calls: vec![] },
                            metadata: None,
                            key: None,
                            taint: Default::default(),
                        },
                    ],
                    description: None,
                },
                // A region that no longer exists in the window - skipped.
                RegionSnapshot {
                    name: "ghost".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 1,
                    max_tokens: 10,
                    entries: vec![RegionEntrySnapshot {
                        content: "orphan".to_string(),
                        tokens: 1,
                        kind: EntryKind::Text,
                        metadata: None,
                        key: None,
                        taint: Default::default(),
                    }],
                    description: None,
                },
            ],
        }
    }

    /// Taint was not persisted at all, so a restart, resume or page-in brought
    /// every region back `Public` no matter how sensitive it had been - while
    /// the gate went on reporting itself armed. It is rebuilt from the entries,
    /// and only where the region already tracks taint: restoring a level onto a
    /// region with tracking off would invent one nothing reads.
    #[test]
    fn restore_rebuilds_region_taint_from_the_persisted_entries() {
        use leviath_core::taint::TaintLevel;

        let mut snap = snapshot();
        snap.regions[0].entries[0].taint = TaintLevel::Private;
        snap.regions[0].entries[1].taint = TaintLevel::Public;

        // Tracking off: the region stays untainted rather than gaining a level.
        let (mut world, entity) = agent_world();
        restore_agent(&mut world, entity, &snap, 1, 7, TokenTotals::default());
        assert!(
            world
                .get::<ContextWindow>(entity)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .taint
                .is_none()
        );

        // Tracking on: the level comes back, per entry and in aggregate.
        let (mut world, entity) = agent_world();
        world
            .get_mut::<ContextWindow>(entity)
            .unwrap()
            .get_region_mut("conversation")
            .unwrap()
            .enable_taint_tracking();
        restore_agent(&mut world, entity, &snap, 1, 7, TokenTotals::default());

        let window = world.get::<ContextWindow>(entity).unwrap();
        let region = window.get_region("conversation").unwrap();
        assert_eq!(region.taint_level(), Some(TaintLevel::Private));
        let taint = region.taint.as_ref().unwrap();
        assert_eq!(taint.entry_taint(0), Some(TaintLevel::Private));
        assert_eq!(taint.entry_taint(1), Some(TaintLevel::Public));
    }

    /// The per-stage ledger was the one piece of persisted state nothing
    /// rebuilt, so a reloaded run came back with every stage at zero - and
    /// because the persist tick rewrites `stages.json` whole, the next one
    /// wrote those zeros over the run's real history (issue #415).
    #[test]
    fn restore_stage_ledger_overlays_the_persisted_records_by_name() {
        use crate::pipeline::StageLedger;
        use leviath_core::run_meta::{StageRecord, StageRunStatus};

        let (mut world, entity) = agent_world();
        // An agent with no ledger at all is left alone rather than panicking.
        restore_stage_ledger(&mut world, entity, &[StageRecord::new("s0".to_string(), 0)]);
        assert!(world.get::<StageLedger>(entity).is_none());

        world.entity_mut(entity).insert(StageLedger(vec![
            StageRecord::new("s0".to_string(), 0),
            StageRecord::new("s1".to_string(), 1),
        ]));
        // `s1` as the run left it, filed under a stale index; plus a record for
        // a stage this blueprint no longer has.
        let mut saved = StageRecord::new("s1".to_string(), 4);
        saved.status = StageRunStatus::Complete;
        saved.entered = true;
        saved.prompt_tokens = 900;
        saved.completion_tokens = 30;
        saved.cached_tokens = 12;
        saved.cache_write_tokens = 4;
        saved.first_call_prompt_tokens = Some(300);
        saved.runaway_warned = true;
        saved.region_tokens.insert("conversation".to_string(), 120);
        saved.started_at = Some(5);
        saved.ended_at = Some(9);
        restore_stage_ledger(
            &mut world,
            entity,
            &[saved, StageRecord::new("removed".to_string(), 9)],
        );

        let ledger = world.get::<StageLedger>(entity).unwrap();
        assert_eq!(
            ledger.0.len(),
            2,
            "a record for a stage the blueprint no longer has is dropped, not appended"
        );
        // Nothing persisted against `s0`: its seeded record stands.
        assert_eq!(ledger.0[0].prompt_tokens, 0);
        assert_eq!(ledger.0[0].status, StageRunStatus::Pending);
        assert!(!ledger.0[0].entered);
        // `s1` comes back whole, under its blueprint index rather than the
        // stale persisted one.
        assert_eq!(ledger.0[1].index, 1);
        assert_eq!(ledger.0[1].name, "s1");
        assert_eq!(ledger.0[1].prompt_tokens, 900);
        assert_eq!(ledger.0[1].completion_tokens, 30);
        assert_eq!(ledger.0[1].cached_tokens, 12);
        assert_eq!(ledger.0[1].cache_write_tokens, 4);
        assert_eq!(ledger.0[1].first_call_prompt_tokens, Some(300));
        assert!(ledger.0[1].runaway_warned);
        assert_eq!(ledger.0[1].region_tokens.get("conversation"), Some(&120));
        assert_eq!(ledger.0[1].started_at, Some(5));
        assert_eq!(ledger.0[1].ended_at, Some(9));
        assert_eq!(ledger.0[1].status, StageRunStatus::Complete);
        assert!(ledger.0[1].entered);
    }

    #[test]
    fn restore_overlays_context_and_jumps_to_stage() {
        let (mut world, entity) = agent_world();
        restore_agent(
            &mut world,
            entity,
            &snapshot(),
            1,
            7,
            TokenTotals {
                prompt_tokens: 100,
                ..Default::default()
            },
        );

        // Context replaced by the persisted entries (with kinds), not the seed.
        let window = world.get::<ContextWindow>(entity).unwrap();
        let region = window.get_region("conversation").unwrap();
        assert_eq!(region.content.len(), 2);
        assert_eq!(region.content[0].content, "prior user turn");
        assert_eq!(region.content[0].kind, EntryKind::UserMessage);
        assert_eq!(region.current_tokens, 8);

        // Jumped to stage 1 (its config swapped in) + iteration restored.
        assert_eq!(world.get::<StageCursor>(entity).unwrap().index, 1);
        let state = world.get::<AgentState>(entity).unwrap();
        assert_eq!(state.current_stage, "s1");
        assert_eq!(state.iteration, 7);
        assert_eq!(state.status, AgentStatus::Active);
        assert_eq!(
            world.get::<InferenceConfig>(entity).unwrap().temperature,
            Some(0.5)
        );
        assert_eq!(world.get::<StageInference>(entity).unwrap().model, "m1");
        assert_eq!(world.get::<TokenTotals>(entity).unwrap().prompt_tokens, 100);
        // Still ready to (re-)infer.
        assert!(world.get::<ReadyToInfer>(entity).is_some());
    }

    // ── pending-batch replay (#96) ──

    fn pending_call(
        id: &str,
        name: &str,
        result: Option<&str>,
    ) -> leviath_core::run_archive::ToolCallRecord {
        leviath_core::run_archive::ToolCallRecord {
            id: id.to_string(),
            name: name.to_string(),
            arguments: r#"{"path":"x.txt"}"#.to_string(),
            result: result.map(str::to_string),
            thought_signature: None,
        }
    }

    fn pending_batch(
        calls: Vec<leviath_core::run_archive::ToolCallRecord>,
    ) -> leviath_core::run_archive::PendingToolBatch {
        leviath_core::run_archive::PendingToolBatch {
            stage_index: 1,
            iteration: 7,
            response: "writing then checking".to_string(),
            calls,
        }
    }

    /// The `conversation` entries of `entity`'s window.
    fn conv_entries(world: &World, entity: Entity) -> Vec<RegionEntry> {
        world
            .get::<ContextWindow>(entity)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .content
            .clone()
    }

    #[test]
    fn pending_batch_replays_real_results_and_synthesizes_interrupted_ones() {
        let (mut world, entity) = agent_world();
        restore_agent(
            &mut world,
            entity,
            &snapshot(),
            1,
            7,
            TokenTotals::default(),
        );
        restore_pending_batch(
            &mut world,
            entity,
            &pending_batch(vec![
                pending_call("c1", "write_file", Some("Wrote 42 bytes to x.txt")),
                pending_call("c2", "shell", None),
            ]),
            &[],
        );

        let entries = conv_entries(&world, entity);
        // The assistant turn landed with both calls, then one result per call:
        // the journaled real result and the synthesized interrupted one.
        let turn = entries
            .iter()
            .find_map(|e| match &e.kind {
                EntryKind::AssistantTurn { tool_calls } if !tool_calls.is_empty() => {
                    Some(tool_calls.clone())
                }
                _ => None,
            })
            .expect("assistant turn appended");
        assert_eq!(turn.len(), 2);
        assert_eq!(turn[0].id, "c1");
        assert_eq!(
            turn[0].arguments,
            serde_json::json!({"path": "x.txt"}),
            "journaled arguments parsed back to JSON"
        );
        let result_of = |id: &str| {
            entries
                .iter()
                .find(|e| {
                    matches!(&e.kind, EntryKind::ToolResult { tool_call_id, .. } if tool_call_id == id)
                })
                .map(|e| e.content.clone())
                .expect("a result per call")
        };
        assert_eq!(result_of("c1"), "Wrote 42 bytes to x.txt");
        assert!(result_of("c2").contains("interrupted"));
        assert!(result_of("c2").contains("Verify whether it took effect"));
    }

    #[test]
    fn pending_batch_survives_request_assembly_unstripped() {
        // The whole point of pairing the turn with a result per call: the
        // assembler's orphan sanitizer must keep every block, so the re-issued
        // request shows the model exactly what already ran. A sliding-window
        // conversation, since that's the kind assembled as typed messages.
        let (mut world, entity) = agent_world();
        world
            .get_mut::<ContextWindow>(entity)
            .unwrap()
            .get_region_mut("conversation")
            .unwrap()
            .kind = RegionKind::SlidingWindow {
            max_items: 100,
            eviction_strategy: Default::default(),
        };
        restore_agent(
            &mut world,
            entity,
            &snapshot(),
            1,
            7,
            TokenTotals::default(),
        );
        restore_pending_batch(
            &mut world,
            entity,
            &pending_batch(vec![pending_call("c1", "shell", None)]),
            &[],
        );

        let assembled = world.get::<ContextWindow>(entity).unwrap().assemble();
        let mut tool_uses = 0;
        let mut tool_results = 0;
        for msg in &assembled.messages {
            if let leviath_providers::MessageContent::Blocks(blocks) = &msg.content {
                for block in blocks {
                    match block {
                        leviath_providers::ContentBlock::ToolUse { id, .. } => {
                            assert_eq!(id, "c1");
                            tool_uses += 1;
                        }
                        leviath_providers::ContentBlock::ToolResult { tool_use_id, .. } => {
                            assert_eq!(tool_use_id, "c1");
                            tool_results += 1;
                        }
                        _ => {}
                    }
                }
            }
        }
        assert_eq!((tool_uses, tool_results), (1, 1), "nothing stripped");
    }

    #[test]
    fn pending_batch_routes_results_through_the_restored_stage_routing() {
        // Stage 1 routes results to `knowledge`: the replayed result's full text
        // lands there and the conversation keeps the pointer - identical to the
        // live apply path, because it IS the live apply path.
        let (mut world, entity) = agent_world();
        world
            .get_mut::<ContextWindow>(entity)
            .unwrap()
            .add_region(Region::new(
                "knowledge".to_string(),
                RegionKind::Pinned,
                10_000,
            ));
        world
            .get_mut::<StageSetups>(entity)
            .unwrap()
            .0
            .get_mut(1)
            .unwrap()
            .routing = Some(leviath_core::ToolResultRouting {
            default_region: "knowledge".to_string(),
            ..Default::default()
        });
        restore_agent(
            &mut world,
            entity,
            &snapshot(),
            1,
            7,
            TokenTotals::default(),
        );
        restore_pending_batch(
            &mut world,
            entity,
            &pending_batch(vec![pending_call("c1", "read_file", Some("the file body"))]),
            &[],
        );

        let window = world.get::<ContextWindow>(entity).unwrap();
        let knowledge = window.get_region("knowledge").unwrap();
        assert!(
            knowledge
                .content
                .iter()
                .any(|e| e.content.contains("the file body")),
            "full text routed to the knowledge region"
        );
        assert!(
            conv_entries(&world, entity).iter().any(
                |e| matches!(&e.kind, EntryKind::ToolResult { tool_call_id, .. } if tool_call_id == "c1")
            ),
            "conversation keeps the paired pointer result"
        );
    }

    #[test]
    fn pending_batch_taints_results_per_tool_sensitivity() {
        use leviath_core::taint::TaintLevel;
        let (mut world, entity) = agent_world();
        world
            .get_mut::<ContextWindow>(entity)
            .unwrap()
            .get_region_mut("conversation")
            .unwrap()
            .enable_taint_tracking();
        world
            .entity_mut(entity)
            .insert(crate::pipeline::ToolSensitivities(
                [("read_file".to_string(), TaintLevel::Private)]
                    .into_iter()
                    .collect(),
            ));
        restore_agent(
            &mut world,
            entity,
            &snapshot(),
            1,
            7,
            TokenTotals::default(),
        );
        restore_pending_batch(
            &mut world,
            entity,
            &pending_batch(vec![pending_call("c1", "read_file", Some("secret body"))]),
            &[],
        );

        let window = world.get::<ContextWindow>(entity).unwrap();
        assert_eq!(
            window.get_region("conversation").unwrap().taint_level(),
            Some(TaintLevel::Private),
            "replayed result tainted like a live one"
        );
    }

    #[test]
    fn unparseable_journaled_arguments_survive_as_a_raw_string() {
        let (mut world, entity) = agent_world();
        restore_agent(
            &mut world,
            entity,
            &snapshot(),
            1,
            7,
            TokenTotals::default(),
        );
        let mut call = pending_call("c1", "shell", None);
        call.arguments = "not json {".to_string();
        restore_pending_batch(&mut world, entity, &pending_batch(vec![call]), &[]);

        let entries = conv_entries(&world, entity);
        let turn = entries
            .iter()
            .find_map(|e| match &e.kind {
                EntryKind::AssistantTurn { tool_calls } if !tool_calls.is_empty() => {
                    Some(tool_calls.clone())
                }
                _ => None,
            })
            .expect("turn still lands");
        assert_eq!(
            turn[0].arguments,
            serde_json::Value::String("not json {".to_string())
        );
    }

    #[test]
    fn interrupted_subagent_calls_point_at_known_children() {
        // A sub-agent call with known children gets the check-first note; other
        // shapes (children but a non-subagent tool, a subagent tool but no
        // children) get the plain interrupted text.
        let kids = vec!["run-kid-1".to_string(), "run-kid-2".to_string()];
        let enriched = interrupted_result("spawn_agent", &kids);
        assert!(enriched.contains("run-kid-1, run-kid-2"));
        assert!(enriched.contains("check_agent"));
        assert_eq!(interrupted_result("shell", &kids), INTERRUPTED_TOOL_RESULT);
        assert_eq!(
            interrupted_result("spawn_agent", &[]),
            INTERRUPTED_TOOL_RESULT
        );

        // And end-to-end: the enriched text is what lands in the window.
        let (mut world, entity) = agent_world();
        restore_agent(
            &mut world,
            entity,
            &snapshot(),
            1,
            7,
            TokenTotals::default(),
        );
        restore_pending_batch(
            &mut world,
            entity,
            &pending_batch(vec![pending_call("c1", "spawn_agent", None)]),
            &kids,
        );
        assert!(
            conv_entries(&world, entity)
                .iter()
                .any(|e| e.content.contains("already has child agent runs")),
            "the synthesized sub-agent note lands in the window"
        );
    }

    #[test]
    fn restore_swaps_in_the_stage_routing_and_clears_stale() {
        use crate::components::ToolResultRoutingComponent;

        // The restored stage routes tool results: the component comes in.
        let (mut world, entity) = agent_world();
        let routed = leviath_core::ToolResultRouting {
            default_region: "knowledge".to_string(),
            ..Default::default()
        };
        world
            .get_mut::<StageSetups>(entity)
            .unwrap()
            .0
            .get_mut(1)
            .unwrap()
            .routing = Some(routed);
        restore_agent(
            &mut world,
            entity,
            &snapshot(),
            1,
            7,
            TokenTotals::default(),
        );
        assert_eq!(
            world
                .get::<ToolResultRoutingComponent>(entity)
                .expect("stage 1's routing swapped in")
                .routing
                .default_region,
            "knowledge"
        );

        // The restored stage has no routing: a stale component (left over from
        // the spawn stage) is cleared rather than routing future batches.
        let (mut world, entity) = agent_world();
        world.entity_mut(entity).insert(ToolResultRoutingComponent {
            routing: leviath_core::ToolResultRouting::default(),
        });
        restore_agent(
            &mut world,
            entity,
            &snapshot(),
            1,
            7,
            TokenTotals::default(),
        );
        assert!(world.get::<ToolResultRoutingComponent>(entity).is_none());
    }

    fn meta_with(run_id: &str, status: RunStatus, updated_at: i64) -> RunMeta {
        let mut m = RunMeta::new(
            run_id.to_string(),
            "a".to_string(),
            "/p".to_string(),
            "t".to_string(),
            None,
            "/w".to_string(),
            1,
        );
        m.status = status;
        m.updated_at = updated_at;
        m
    }

    #[test]
    fn classify_restore_skips_terminal_and_ranks_the_rest() {
        // Terminal → skipped.
        assert_eq!(classify_restore(&RunStatus::Complete, false), None);
        assert_eq!(classify_restore(&RunStatus::Error, false), None);
        assert_eq!(classify_restore(&RunStatus::Cancelled, false), None);
        // Actionable → Active.
        assert_eq!(
            classify_restore(&RunStatus::Running, false),
            Some(RestorePriority::Active)
        );
        assert_eq!(
            classify_restore(&RunStatus::Starting, false),
            Some(RestorePriority::Active)
        );
        // No immediate progress → Blocked.
        assert_eq!(
            classify_restore(&RunStatus::WaitingInput, false),
            Some(RestorePriority::Blocked)
        );
        assert_eq!(
            classify_restore(&RunStatus::Paused, false),
            Some(RestorePriority::Blocked)
        );
        assert_eq!(
            classify_restore(&RunStatus::CompleteInteractive, false),
            Some(RestorePriority::Blocked)
        );
        // Parked mid fan-out is Blocked even when otherwise Running.
        assert_eq!(
            classify_restore(&RunStatus::Running, true),
            Some(RestorePriority::Blocked)
        );
        // A terminal run parked on a fan-out is still skipped.
        assert_eq!(classify_restore(&RunStatus::Complete, true), None);
    }

    #[test]
    fn triage_orders_actionable_first_then_by_recency_and_drops_terminal() {
        let candidates = vec![
            (
                meta_with("blocked-old", RunStatus::WaitingInput, 100),
                false,
            ),
            (meta_with("active-old", RunStatus::Running, 200), false),
            (meta_with("terminal", RunStatus::Complete, 999), false),
            (meta_with("active-new", RunStatus::Starting, 300), false),
            (meta_with("parked", RunStatus::Running, 999), true), // fan-out → Blocked
            (
                meta_with("blocked-new", RunStatus::WaitingInput, 400),
                false,
            ),
        ];
        let order: Vec<String> = triage_restores(candidates)
            .into_iter()
            .map(|m| m.run_id)
            .collect();
        // Active tier first (most-recent first), then Blocked tier (most-recent
        // first, with the fan-out-parked run demoted into it). Terminal dropped.
        assert_eq!(
            order,
            vec![
                "active-new".to_string(),  // Active, updated 300
                "active-old".to_string(),  // Active, updated 200
                "parked".to_string(),      // Blocked (fan-out), updated 999
                "blocked-new".to_string(), // Blocked, updated 400
                "blocked-old".to_string(), // Blocked, updated 100
            ]
        );
    }

    #[test]
    fn restore_with_out_of_range_stage_keeps_spawn_config() {
        let (mut world, entity) = agent_world();
        let mut snap = snapshot();
        snap.stage_name = "s0".to_string();
        // The blueprint now has fewer stages than the persisted index.
        restore_agent(&mut world, entity, &snap, 9, 2, TokenTotals::default());

        // Stage jump skipped: cursor + config stay at stage 0.
        assert_eq!(world.get::<StageCursor>(entity).unwrap().index, 0);
        assert_eq!(world.get::<StageInference>(entity).unwrap().model, "m0");
        // State + context still restored.
        assert_eq!(world.get::<AgentState>(entity).unwrap().iteration, 2);
        assert_eq!(
            world
                .get::<ContextWindow>(entity)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .content
                .len(),
            2
        );
    }
}
