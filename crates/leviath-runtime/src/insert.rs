//! Placing a resolved run into the world.
//!
//! Everything was decided when the run was resolved and every live handle
//! was built when it was bound, so this does no I/O, asks nothing, and
//! cannot fail. It is the same call for a new run and a resumed one; they
//! differ only in the [`RunState`] they start from: a new run starts from
//! [`initial_state`], a resumed one from the last state it recorded.
//!
//! Each component comes from one place. The graph and the stage plans come
//! from the [`RunSpec`] (the current stage's inference, routing and inference
//! settings, the run's metadata, compaction, loop detection and markers). What
//! the run has done so far comes from the [`RunState`], one field at a time,
//! in [`place`]. What only the host can build (compiled code, the run's mime
//! registry, tool services, the taint gate, the title chain) comes from the
//! [`Bindings`]; a run asks for its title only when the host bound a chain.

use std::collections::HashMap;
use std::sync::Arc;

use bevy_ecs::prelude::*;

use crate::components::ContextWindow;
use crate::pipeline::spec_view;
use crate::spec::env::Bindings;
use crate::spec::run_spec::RunSpec;
use crate::state::{RunState, RunStatus};

pub mod place;

/// A run's resolved spec, on its entity. Shared, because every system that
/// reads the graph reads the same one.
#[derive(Component, Debug, Clone)]
pub struct RunSpecC(pub Arc<RunSpec>);

/// Compiled custom-region render hooks, keyed by the code each region names.
///
/// Bound like any other compiled code. [`insert`] moves them into the run's
/// window, which is where the render and write paths look for them, and the
/// component itself does not stay on the entity.
pub use crate::bind::RegionScripts;

/// Place a run into the world and return its entity.
///
/// The spec, the state and the bindings are all it reads, besides the world's
/// journal (which the window's change records go to when the world has one)
/// and the entities of any fan-out workers already placed.
pub fn insert(
    world: &mut World,
    spec: Arc<RunSpec>,
    bindings: Bindings,
    state: &RunState,
) -> Entity {
    let idx = place::stage_index(&spec, state);
    let setup = spec_view::stage_setup(&spec, idx);
    let mut window = place::context_window(&spec, state);
    window.attach_journal(
        spec.run_id.as_str(),
        world.get_resource::<crate::pipeline::JournalSender>(),
    );
    let workers = place::worker_entities(world, state);
    // A resumed run is at the step its file ended on, until it takes another.
    if let Some(lane) = world.get_resource::<crate::pipeline::PersistLaneHealth>() {
        lane.0.stepped(spec.run_id.as_str(), state.seq);
    }
    let mut entity = world.spawn((
        RunSpecC(spec.clone()),
        place::placed_agent_state(&spec, state),
        place::message_inbox(&spec, state),
        crate::pipeline::StageCursor { index: idx },
        place::stage_progress(state),
        place::visit_counts(state),
        window,
        spec_view::stage_inference(&spec, idx, None),
        setup.inference_config.clone(),
        place::stage_ledger(state),
        place::io_buffer(&spec, state),
        spec_view::StageToolOverrides::default(),
    ));
    entity.insert((
        place::run_metadata(&spec, state),
        place::token_totals(state),
        place::run_clock(state),
        crate::pipeline::PersistWatermark::default(),
        place::outcome_flags(state),
        crate::pipeline::RunBlobs(state.blobs.clone()),
    ));
    place::spec_components(&mut entity, &spec, &setup);
    place::optional_state(&mut entity, state);
    place::point_progress(&mut entity, state);
    place::phase(&mut entity, &spec, state);
    bindings.apply(&mut entity);
    place::title_request(&mut entity, &spec, state);
    if let Some(scripts) = entity.take::<RegionScripts>()
        && let Some(mut window) = entity.get_mut::<ContextWindow>()
    {
        window.region_scripts = scripts.0;
    }
    let id = entity.id();
    place::fan_out(world, id, state, &workers);
    id
}

/// The state a new run starts in: at its first stage (a fan-out worker's own stage),
/// visited once, with its window laid out from the graph, filled from what the
/// spec seeded, and that stage's instructions in place.
///
/// A prompt too large for its region is left out rather than failing: the
/// resolver refuses such a spec before it is ever inserted.
pub fn initial_state(spec: &RunSpec) -> RunState {
    let mut window = seeded_window(spec, &HashMap::new());
    let _ = enter_first_stage(spec, &mut window);
    initial_state_from(spec, &window)
}

/// A window laid out from the graph's layout and filled from the spec's seeds:
/// each seeded region gets its text (trimmed to the region's budget) and then
/// each seeded part as an entry of its own. A seed with parts and no text adds
/// no empty text entry. Taint tracking is on from the start when the graph
/// asks for it.
pub(crate) fn seeded_window(
    spec: &RunSpec,
    scripts: &HashMap<String, Arc<leviath_scripting::region_hook::RegionScript>>,
) -> ContextWindow {
    let layout = spec_view::graph_layout(spec);
    let mut window = ContextWindow::new(layout.total);
    // Before seeding, so seed writes pass through each region's `on_write`
    // hook like any other entry.
    window.region_scripts = scripts.clone();
    crate::context_setup::lay_out(&mut window, layout.regions);
    if spec.graph.taint_tracking == Some(true) {
        window.enable_taint_tracking();
    }
    for (name, seeded) in &spec.seeded {
        let budget = window.get_region(name.as_str()).map_or(0, |r| r.max_tokens);
        if !seeded.text.is_empty() || seeded.parts.is_empty() {
            let fitted = crate::context_setup::fit_seed_to_budget(&seeded.text, budget);
            let tokens = leviath_core::estimate_tokens(&fitted);
            let _ = window.add_to_region_caused(
                leviath_core::ContextCause::Seed,
                name.as_str(),
                fitted,
                tokens,
            );
        }
        for part in &seeded.parts {
            let content = leviath_core::region::EntryContent::from_parts(vec![
                crate::components::part_from_state(part),
            ]);
            let tokens = content.tokens(None);
            let _ = window.add_content_entry(
                leviath_core::ContextCause::Seed,
                name.as_str(),
                leviath_core::EntryKind::Text,
                content,
                tokens,
            );
        }
    }
    window
}

/// Enter the run's entry stage in `window`: give stage prompts a region of
/// their own when the layout has none, then apply the entry stage's layout,
/// hidden and reset regions and instructions. `Err` when the instructions do
/// not fit their region.
pub(crate) fn enter_first_stage(spec: &RunSpec, window: &mut ContextWindow) -> Result<(), String> {
    let prompts: Vec<Option<String>> = (0..spec.graph.stages.len())
        .map(|i| spec_view::stage_setup(spec, i).system_prompt)
        .collect();
    crate::context_setup::ensure_stage_instructions_region(window, &prompts);
    let entry = start_index(spec);
    crate::pipeline::apply_stage_context(&spec_view::stage_setup(spec, entry), window)
}

/// The stage a new run starts in: for a fan-out worker, the stage of its
/// parent's graph it was started to run; for any other run, the graph's entry
/// stage.
pub(crate) fn start_index(spec: &RunSpec) -> usize {
    spec.placement
        .worker_stage
        .as_ref()
        .and_then(|stage| spec.graph.stages.iter().position(|s| s.name == *stage))
        .unwrap_or_else(|| spec_view::entry_index(&spec.graph))
}

/// The state a new run starts in, with `window` as its context: at the entry
/// stage, visited once, its first visit open in the ledger, active.
pub(crate) fn initial_state_from(spec: &RunSpec, window: &ContextWindow) -> RunState {
    let entry = start_index(spec);
    let setup = spec_view::stage_setup(spec, entry);
    let mut state = RunState::initial(
        spec.graph.stages[entry].name.clone(),
        window.to_state(),
        setup.accepts_messages,
    );
    let now = chrono::Utc::now().timestamp();
    let visit = leviath_core::execution::mint_visit_id();
    state.status = RunStatus::Active;
    state.cursor.visit = visit.clone();
    state.visits.insert(state.cursor.stage.clone(), 1);
    state.ledger = spec
        .graph
        .stages
        .iter()
        .map(|s| place::pending_stage(s.name.clone()))
        .collect();
    // The entry stage is the one stage no transition enters, so its first visit
    // is opened here, as entering any other stage opens one.
    state.ledger[entry].visits.push(crate::state::VisitRecord {
        id: visit,
        entered_at: now,
        left_at: None,
        spend: Default::default(),
        clock: Default::default(),
    });
    state.flags.no_output_tools = !spec_view::any_stage_can_modify(&spec.graph);
    // What the entry stage's `require_region_updated` gates compare against,
    // as a transition records it for every other stage. Without it the gate
    // has no baseline and lets an untouched region through.
    state.progress.entry_region_digests =
        crate::pipeline::watched_region_digests(&spec.graph, &spec.graph.stages[entry], window)
            .into_iter()
            .collect();
    state
}

#[cfg(test)]
#[path = "insert_tests.rs"]
mod tests;
