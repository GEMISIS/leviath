//! Agent execution engine using bevy_ecs.

use bevy_ecs::prelude::*;
use tracing::info;

/// The main agent execution engine.
///
/// Manages the ECS world, schedules systems, and drives agent execution
/// through a game-loop-inspired tick model.
pub struct AgentEngine {
    /// ECS world containing all agents and their components
    world: World,

    /// System schedule for executing agent behaviors
    schedule: Schedule,
}

impl AgentEngine {
    /// Create a new agent engine.
    pub fn new() -> Self {
        info!("Initializing Leviath agent engine");
        
        let mut world = World::new();
        let mut schedule = Schedule::default();

        // TODO: Add systems to schedule
        // schedule.add_systems((
        //     context_management_system,
        //     inference_system,
        //     eviction_system,
        //     pool_management_system,
        // ));

        Self { world, schedule }
    }

    /// Execute one tick of the agent engine.
    ///
    /// This runs all systems in the schedule once, processing all agents.
    pub fn tick(&mut self) {
        self.schedule.run(&mut self.world);
    }

    /// Get a reference to the ECS world.
    pub fn world(&self) -> &World {
        &self.world
    }

    /// Get a mutable reference to the ECS world.
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }
}

impl Default for AgentEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_engine_creation() {
        let engine = AgentEngine::new();
        assert!(engine.world().entities().len() == 0);
    }
}
