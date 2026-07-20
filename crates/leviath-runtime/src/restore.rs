//! Restart recovery: bring a freshly-spawned agent back to its persisted running
//! state so the daemon resumes it where it stopped.
//!
//! When the daemon restarts, the CLI reloads each non-terminal run's blueprint
//! and spawns a fresh agent, then calls [`restore_agent`] to overlay the persisted
//! context, jump to the persisted stage + iteration, and restore token totals. The
//! agent keeps the `ReadyToInfer` marker `spawn_agent` set, so **any inference
//! that was in flight when the daemon stopped is re-issued** on the next tick —
//! nothing is left stuck awaiting a job that died with the old process.

use bevy_ecs::prelude::*;
use leviath_core::region::RegionEntry;
use leviath_core::run_meta::ContextSnapshot;

use crate::components::{AgentState, AgentStatus, ContextWindow};
use crate::persistence::TokenTotals;
use crate::pipeline::{StageCursor, StageInferences, StageSetups};

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
                        },
                        RegionEntrySnapshot {
                            content: "prior assistant".to_string(),
                            tokens: 3,
                            kind: EntryKind::AssistantTurn { tool_calls: vec![] },
                            metadata: None,
                            key: None,
                        },
                    ],
                },
                // A region that no longer exists in the window — skipped.
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
                    }],
                },
            ],
        }
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
