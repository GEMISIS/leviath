//! Agent status control on [`PipelineWorld`]: read a status, set one, and the
//! three transitions a person can ask for.
//!
//! Split out of `world.rs` to keep that file inside the workspace's structure
//! limit. It is one coherent unit rather than an arbitrary cut: every method
//! here reads or writes `AgentState::status`, they share the foreign-id guard
//! in [`PipelineWorld::agent_status`], and the guards they apply to each other
//! (`pause` only from `Active`/`Idle`, `resume` only from `Paused`/`Idle`) only
//! make sense side by side.

use super::*;

impl PipelineWorld {
    /// The status of an agent, if it still exists.
    ///
    /// An id another world minted reports `None` rather than this world's agent
    /// of the same raw entity - see [`AgentId`]. `pause`, `resume` and `cancel`
    /// all read status through here, so guarding it guards them.
    pub fn agent_status(&self, agent: AgentId) -> Option<AgentStatus> {
        if agent.world != self.id {
            return None;
        }
        self.world
            .get::<AgentState>(agent.entity)
            .map(|s| s.status.clone())
    }

    /// Set an agent's status and wake the driver. Returns `false` if the agent no
    /// longer exists. The async-starting dispatchers only act on `Active` agents,
    /// so this is how the world pauses/resumes/cancels an agent - a non-`Active`
    /// agent is simply data the systems skip until it is `Active` again.
    pub fn set_status(&mut self, agent: AgentId, status: AgentStatus) -> bool {
        // Every status mutation funnels through here, so this is the one place a
        // foreign id has to be refused.
        if agent.world != self.id {
            return false;
        }
        let Some(mut state) = self.world.get_mut::<AgentState>(agent.entity) else {
            return false;
        };
        state.status = status;
        self.wake.notify_one();
        true
    }

    /// Pause an agent (it finishes any in-flight step, then stops before starting
    /// new work). Only `Active` and `Idle` agents can be paused: a `Waiting`
    /// agent's status is the marker the fan-out merge poll and interaction
    /// resolution depend on, so overwriting it would wedge the run, and pausing
    /// a terminal agent is meaningless. Returns `false` if the agent no longer
    /// exists or is not in a pausable state.
    pub fn pause(&mut self, agent: AgentId) -> bool {
        match self.agent_status(agent) {
            Some(AgentStatus::Active | AgentStatus::Idle) => {
                self.set_status(agent, AgentStatus::Paused)
            }
            _ => false,
        }
    }

    /// Resume a paused agent. `Idle` is also accepted (resume-as-nudge for an
    /// agent that has not ticked yet); anything else returns `false`.
    pub fn resume(&mut self, agent: AgentId) -> bool {
        // Resolved once, up front. Doing it again inside the arm below would
        // be a second check that cannot fail - the status lookup already
        // proved the entity is here - and an arm nothing can reach is an arm
        // nothing can test.
        let Some(entity) = agent.resolve_in(&self.world) else {
            return false;
        };
        match self.agent_status(agent) {
            Some(AgentStatus::Paused | AgentStatus::Idle) => {
                // An explicit resume says conditions have changed - most often
                // a top-up after a run paused on exhausted credits (issue
                // #413). A tripped breaker would otherwise hold the retry
                // until its cooldown lapses, making the resume look ignored.
                if let Some(mut circuits) = self
                    .world
                    .get_resource_mut::<crate::pipeline::ProviderCircuits>()
                {
                    circuits.reset();
                }
                // The run no longer claims to need setup. If the machine was
                // not actually fixed the watchdog re-parks it a minute later
                // with a fresh message, which is better than carrying a stale
                // one that describes a problem somebody may have just solved.
                self.world
                    .entity_mut(entity)
                    .remove::<crate::pipeline::PausedForSetup>();
                self.replay_held_inference(entity);
                self.set_status(agent, AgentStatus::Active)
            }
            _ => false,
        }
    }

    /// Put back an inference outcome that landed while the agent was paused.
    ///
    /// The result is sent to the same channel it originally came in on, so the
    /// collect system applies it through its ordinary arms on the next tick -
    /// no duplicated success/failure handling, and no wasted call: whatever the
    /// provider already charged for is still used. The agent kept its
    /// `Awaiting*` marker while held, which is what makes it visible to that
    /// system's query.
    ///
    /// A no-op for the ordinary case of a run that was paused with nothing in
    /// flight.
    fn replay_held_inference(&mut self, entity: Entity) {
        let Some(held) = self
            .world
            .entity_mut(entity)
            .take::<crate::pipeline::HeldInference>()
        else {
            return;
        };
        // Installed unconditionally by `PipelineWorld::new`, which is the only
        // way to get a world that can reach this at all.
        let stage = self.world.resource::<crate::pipeline::InferenceStage>();
        let channel = match held.lane {
            crate::pipeline::HeldLane::Stage => &stage.outcomes,
            crate::pipeline::HeldLane::TransitionChoice => &stage.transition_outcomes,
        };
        // A closed channel means the pipeline is shutting down; the run is being
        // torn down with it, so there is nothing to salvage and nothing to warn
        // about.
        let _ = channel.send(held.outcome);
    }

    /// Cancel an agent (it stops starting new work; in-flight results still land).
    pub fn cancel(&mut self, agent: AgentId) -> bool {
        self.set_status(agent, AgentStatus::Cancelled)
    }
}
