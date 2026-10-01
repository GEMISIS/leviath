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
//! in [`place`]. What only the host can build (compiled code, tool services,
//! the taint gate, the title chain) comes from the [`Bindings`].

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
#[derive(Component, Default)]
pub struct RegionScripts(pub HashMap<String, Arc<leviath_scripting::region_hook::RegionScript>>);

/// Place a run into the world and return its entity.
///
/// The spec, the state and the bindings are all it reads, besides the world's
/// persistence lane (which the window's change records go down when the world
/// has one) and the entities of any fan-out workers already placed.
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
        world.get_resource::<crate::pipeline::PersistenceStage>(),
    );
    let workers = place::worker_entities(world, state);
    let mut entity = world.spawn((
        RunSpecC(spec.clone()),
        place::agent_state(&spec, state),
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
    ));
    place::spec_components(&mut entity, &spec, &setup);
    place::optional_state(&mut entity, state);
    place::phase(&mut entity, &spec, state);
    bindings.apply(&mut entity);
    if let Some(scripts) = entity.take::<RegionScripts>()
        && let Some(mut window) = entity.get_mut::<ContextWindow>()
    {
        window.region_scripts = scripts.0;
    }
    let id = entity.id();
    place::fan_out(world, id, &spec, state, &workers);
    id
}

/// The state a new run starts in: at its entry stage, visited once, with its
/// window laid out from the graph, filled from what the spec seeded, and the
/// entry stage's instructions in place.
///
/// A prompt too large for its region is left out rather than failing: the
/// resolver refuses such a spec before it is ever inserted.
pub fn initial_state(spec: &RunSpec) -> RunState {
    let mut window = seeded_window(spec, &HashMap::new());
    let _ = enter_first_stage(spec, &mut window);
    initial_state_from(spec, &window)
}

/// The graph's layout as an otherwise empty blueprint, for the window setup
/// that still reads one.
fn layout_only(layout: crate::spec::ContextLayout) -> crate::spec::Blueprint {
    crate::spec::Blueprint::new(String::new(), String::new(), Vec::new(), layout)
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
    let mut window = ContextWindow::new(layout.total_budget_tokens);
    // Before seeding, so seed writes pass through each region's `on_write`
    // hook like any other entry.
    window.region_scripts = scripts.clone();
    crate::context_setup::init_window_seeded(&mut window, &layout_only(layout), &HashMap::new());
    if spec.graph.taint_tracking == Some(true) {
        window.enable_taint_tracking();
    }
    for (name, seeded) in &spec.seeded {
        let budget = window.get_region(name.as_str()).map_or(0, |r| r.max_tokens);
        if !seeded.text.is_empty() || seeded.parts.is_empty() {
            let fitted = fit_seed_to_budget(&seeded.text, budget);
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

/// Marker appended to a seed that was trimmed to fit its region.
const SEED_TRUNCATION_MARKER: &str =
    "\n[...truncated by leviath: seed exceeded this region's budget]";

/// Trim `content` so that its token estimate fits `max_tokens`, leaving room
/// for [`SEED_TRUNCATION_MARKER`]; unchanged when it already fits. A region too
/// small to hold even the marker gets nothing.
fn fit_seed_to_budget(content: &str, max_tokens: usize) -> String {
    let allowed = max_tokens.saturating_sub(1).saturating_mul(4);
    if content.len() <= allowed {
        return content.to_string();
    }
    let Some(room) = allowed.checked_sub(SEED_TRUNCATION_MARKER.len()) else {
        return String::new();
    };
    format!(
        "{}{SEED_TRUNCATION_MARKER}",
        leviath_core::truncate_at_boundary(content, room)
    )
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
    let entry = spec_view::entry_index(&spec.graph);
    crate::pipeline::apply_stage_context(&spec_view::stage_setup(spec, entry), window)
}

/// The state a new run starts in, with `window` as its context: at the entry
/// stage, visited once, its first visit open in the ledger, active.
pub(crate) fn initial_state_from(spec: &RunSpec, window: &ContextWindow) -> RunState {
    let entry = spec_view::entry_index(&spec.graph);
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
    state
}

#[cfg(test)]
#[path = "insert_tests.rs"]
mod tests;
