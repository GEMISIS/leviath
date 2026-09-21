//! Turning one name the manifest wrote into the thing it names.
//!
//! A manifest addresses its own regions and stages by name, and the schema
//! serves objects. One place does the lookup, so every field that resolves a
//! region agrees about which layouts count and in what order.
//!
//! A region is looked for in the blueprint's own layout first and then in each
//! stage's own, which is the order [`leviath_core::Blueprint`] itself uses when
//! it decides whether a name exists anywhere. Nothing is invented: a name no
//! layout declares resolves to nothing, and the field that asked says what that
//! means.

use std::sync::Arc;

use leviath_core::Blueprint as CoreBlueprint;

use super::super::blueprint::Region;
use super::stage::Stage;

/// The region a name points at, or nothing when no layout declares it.
///
/// The four regions the runtime carries whatever a manifest says -
/// `conversation`, `tool_results`, `final_output` and `stage_instructions` -
/// resolve only where a layout declares them too. A blueprint may name one it
/// left to the runtime, and this answers nothing for it rather than describing a
/// declaration nobody wrote.
pub(crate) fn region(blueprint: &Arc<CoreBlueprint>, name: &str) -> Option<Region> {
    if let Some(at) = position(&blueprint.context_layout, name) {
        return Some(Region {
            blueprint: Arc::clone(blueprint),
            stage: None,
            at,
        });
    }
    blueprint
        .stages
        .iter()
        .enumerate()
        .find_map(|(stage, def)| {
            let at = position(def.context_layout.as_ref()?, name)?;
            Some(Region {
                blueprint: Arc::clone(blueprint),
                stage: Some(stage),
                at,
            })
        })
}

/// Every name in `names` that a layout declares, in the order they were written.
///
/// A name with no declaration is left out rather than standing in for one, and
/// every field that calls this serves the names it was given beside the result,
/// so nothing a manifest wrote disappears.
pub(crate) fn regions(blueprint: &Arc<CoreBlueprint>, names: &[String]) -> Vec<Region> {
    names
        .iter()
        .filter_map(|name| region(blueprint, name))
        .collect()
}

/// The stage a name points at, or nothing when the blueprint declares no stage
/// under it.
pub(crate) fn stage(blueprint: &Arc<CoreBlueprint>, name: &str) -> Option<Stage> {
    let at = blueprint.stages.iter().position(|def| def.name == name)?;
    Some(Stage {
        blueprint: Arc::clone(blueprint),
        at,
    })
}

/// Where `name` sits in one layout.
fn position(layout: &leviath_core::layout::ContextLayout, name: &str) -> Option<usize> {
    layout.regions.iter().position(|region| region.name == name)
}
