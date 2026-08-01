//! World-scoped content interner resource.
//!
//! One [`ContentInternerRes`] is inserted into the ECS World at pipeline
//! construction. Every spawned [`crate::ContextWindow`] clones the handle so
//! agents share interned entry text without any process-global table.

use bevy_ecs::prelude::Resource;
use leviath_core::ContentInterner;

/// Bevy resource wrapping the shareable [`ContentInterner`] handle.
#[derive(Resource, Clone, Default, Debug)]
pub struct ContentInternerRes(pub ContentInterner);

impl ContentInternerRes {
    pub fn new() -> Self {
        Self(ContentInterner::new())
    }

    pub fn handle(&self) -> ContentInterner {
        self.0.clone()
    }
}
