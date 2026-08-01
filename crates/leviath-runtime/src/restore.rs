//! Restart recovery: bring a freshly-spawned agent back to its persisted running
//! state so the daemon resumes it where it stopped.
//!
//! When the daemon restarts, the CLI reloads each non-terminal run's blueprint
//! and spawns a fresh agent, then calls [`restore_agent`] to overlay the persisted
//! context, jump to the persisted stage + iteration, and restore token totals. The
//! agent keeps the `ReadyToInfer` marker `spawn_agent` set, so **any inference
//! that was in flight when the daemon stopped is re-issued** on the next tick -
//! nothing is left stuck awaiting a job that died with the old process.

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
        RunStatus::WaitingInput | RunStatus::CompleteInteractive => Some(RestorePriority::Blocked),
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
        let interner = window.interner().clone();
        for snap_region in &snapshot.regions {
            if let Some(region) = window
                .regions
                .iter_mut()
                .find(|r| r.name == snap_region.name)
            {
                region.content = snap_region
                    .entries
                    .iter()
                    .map(|e| {
                        RegionEntry::from_parts_interned(
                            &interner,
                            &e.content,
                            e.tokens,
                            0,
                            e.metadata.clone(),
                            e.kind.clone(),
                            e.key.clone(),
                        )
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

    // 2. Jump to the persisted stage, swapping in its inference config.
    if let Some(inf) = world
        .get::<StageInferences>(entity)
        .expect("a spawned agent has stage inferences")
        .0
        .get(stage_index)
        .cloned()
    {
        let cfg = world
            .get::<StageSetups>(entity)
            .expect("a spawned agent has stage setups")
            .0[stage_index]
            .inference_config
            .clone();
        world.entity_mut(entity).insert((inf, cfg));
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
                request_timeout_secs: None,
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            system_prompt: None,
        }
    }

    fn si(model: &str) -> StageInference {
        StageInference {
            provider_name: "p".to_string(),
            model: model.to_string(),
            tools: vec![],
            tool_filter: None,
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
        assert_eq!(region.content[0].content(), "prior user turn");
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
