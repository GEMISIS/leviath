//! Turning one name the blueprint wrote into the thing it names.
//!
//! A blueprint addresses its own regions and stages by name, and the schema
//! serves objects. One place does the lookup, so every field that resolves a
//! region agrees about which layouts count and in what order.
//!
//! A region is looked for in the graph's own layout first and then in each
//! stage's own. Nothing is invented: a name no layout declares resolves to
//! nothing, and the field that asked says what that means.

use std::sync::Arc;

use leviath_runtime::spec::graph::RegionLayoutDef;

use super::super::blueprint::Region;
use super::stage::Stage;
use crate::commands::serve::core::blueprints::ParsedBlueprint;

/// The region a name points at, or nothing when no layout declares it.
///
/// The regions the runtime carries whatever a blueprint says, such as
/// `conversation`, resolve only where a layout declares them too. A blueprint
/// may name one it left to the runtime, and this answers nothing for it rather
/// than describing a declaration nobody wrote.
pub(crate) fn region(blueprint: &Arc<ParsedBlueprint>, name: &str) -> Option<Region> {
    if let Some(at) = position(&blueprint.graph.layout, name) {
        return Some(Region {
            blueprint: Arc::clone(blueprint),
            stage: None,
            at,
        });
    }
    blueprint
        .graph
        .stages
        .iter()
        .enumerate()
        .find_map(|(stage, def)| {
            let at = position(def.layout.as_ref()?, name)?;
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
/// so nothing a blueprint wrote disappears.
pub(crate) fn regions<T: AsRef<str>>(blueprint: &Arc<ParsedBlueprint>, names: &[T]) -> Vec<Region> {
    names
        .iter()
        .filter_map(|name| region(blueprint, name.as_ref()))
        .collect()
}

/// The stage a name points at, or nothing when the graph declares no stage
/// under it.
pub(crate) fn stage(blueprint: &Arc<ParsedBlueprint>, name: &str) -> Option<Stage> {
    let at = blueprint
        .graph
        .stages
        .iter()
        .position(|def| def.name.as_str() == name)?;
    Some(Stage {
        blueprint: Arc::clone(blueprint),
        at,
    })
}

/// Where `name` sits in one layout.
fn position(layout: &RegionLayoutDef, name: &str) -> Option<usize> {
    layout
        .regions
        .iter()
        .position(|region| region.name.as_str() == name)
}
