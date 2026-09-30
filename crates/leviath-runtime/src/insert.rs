//! Placing a resolved run into the world.
//!
//! Everything was decided when the run was resolved and every live handle
//! was built when it was bound, so this does no I/O, asks nothing, and
//! cannot fail. It is the same call for a new run and a resumed one; they
//! differ only in the [`RunState`] they start from.

use std::sync::Arc;

use bevy_ecs::prelude::*;

use crate::spec::env::Bindings;
use crate::spec::run_spec::RunSpec;
use crate::state::RunState;

/// A run's resolved spec, on its entity. Shared, because every system that
/// reads the graph reads the same one.
#[derive(Component, Debug, Clone)]
pub struct RunSpecC(pub Arc<RunSpec>);

/// Place a run into the world and return its entity.
///
/// The spec and the bindings are placed here; the pipeline's components are
/// derived from `_state` by the systems that own them.
pub fn insert(
    world: &mut World,
    spec: Arc<RunSpec>,
    bindings: Bindings,
    _state: &RunState,
) -> Entity {
    let mut entity = world.spawn(RunSpecC(spec));
    bindings.apply(&mut entity);
    entity.id()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Component, Debug, PartialEq)]
    struct Marker(u8);

    #[test]
    fn a_run_lands_with_its_spec_and_its_bindings() {
        let spec = Arc::new(crate::spec::run_spec::tests::spec());
        let state = RunState::initial(
            crate::spec::names::StageName::new("plan").unwrap(),
            Default::default(),
            true,
        );
        let mut world = World::new();
        let bindings = Bindings::new().with(Marker(7));
        assert_eq!(bindings.len(), 1);
        assert!(!bindings.is_empty());
        assert_eq!(format!("{bindings:?}"), "Bindings(1 bundles)");
        let e = insert(&mut world, spec.clone(), bindings, &state);
        assert_eq!(world.get::<Marker>(e), Some(&Marker(7)));
        assert_eq!(world.get::<RunSpecC>(e).unwrap().0.run_id, spec.run_id);
        let mut more = Bindings::new();
        more.extend(Bindings::new().with(Marker(1)));
        assert_eq!(more.len(), 1);
    }
}
