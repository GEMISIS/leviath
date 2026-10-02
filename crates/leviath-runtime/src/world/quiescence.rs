//! How the driver decides a tick changed nothing: per-phase marker counts,
//! the per-agent progress digest, and whether anything is still in flight -
//! and how what is still in flight is stopped when the world shuts down.

use super::*;

impl PipelineWorld {
    /// Cancel every job still in flight, so shutdown does not wait on one.
    ///
    /// A batch parked on an approval prompt is waiting on a person, and with no
    /// interaction timeout that is for ever. Cancelling drops the batch
    /// instead. Its calls are not marked done and its assistant turn is
    /// already journalled with the batch pending, so the run reloads on the
    /// next daemon start exactly where it was: parked, and asking again.
    pub(super) fn abort_in_flight_work(&mut self) {
        let mut agents = self.world.query::<&crate::pipeline::InFlightWork>();
        for in_flight in agents.iter(&self.world) {
            for token in &in_flight.0 {
                token.cancel();
            }
        }
        crate::inference_call::drop_held_calls(&mut self.world);
    }

    pub(super) fn count<F: QueryFilter>(&mut self) -> usize {
        let mut q = self.world.query_filtered::<(), F>();
        q.iter(&self.world).count()
    }

    /// Digest the run progress a phase marker cannot show: each agent's status,
    /// which stage it is in, and its per-stage counters.
    ///
    /// Only values that step on a real event go in. Anything that moves on its
    /// own (a clock, a stall timestamp) would keep the fixed-point loop from ever
    /// converging, which is a spinning daemon rather than a parked one.
    ///
    /// The per-agent digests are XOR-folded, so archetype iteration order doesn't
    /// matter; each one includes the entity id so two agents swapping states
    /// can't cancel out.
    pub(super) fn agent_digest(&mut self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut query = self.world.query::<(
            Entity,
            &AgentState,
            Option<&crate::pipeline::StageCursor>,
            Option<&crate::pipeline::StageProgress>,
        )>();
        query
            .iter(&self.world)
            .map(|(entity, state, cursor, progress)| {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                entity.to_bits().hash(&mut hasher);
                state.status.hash(&mut hasher);
                state.current_stage.hash(&mut hasher);
                state.iteration.hash(&mut hasher);
                cursor.map(|c| c.index).hash(&mut hasher);
                progress
                    .map(|p| {
                        (
                            p.iterations,
                            p.total_tool_calls,
                            p.modifying_tool_calls,
                            p.gate_reentries,
                            p.stuck_fired,
                        )
                    })
                    .hash(&mut hasher);
                hasher.finish()
            })
            .fold(0, |acc, digest| acc ^ digest)
    }

    /// Snapshot the per-phase marker counts and the per-agent progress digest.
    pub(super) fn fingerprint(&mut self) -> Fingerprint {
        let markers = [
            self.count::<With<ReadyToInfer>>(),
            self.count::<With<AwaitingInference>>(),
            self.count::<With<ProcessResponse>>(),
            self.count::<With<ReadyForTools>>(),
            self.count::<With<ReadyForTransition>>(),
            self.count::<With<ResolveTransition>>(),
            self.count::<With<AwaitingTools>>(),
            self.count::<With<AwaitingTransitionChoice>>(),
            self.count::<With<AwaitingTransitionResponse>>(),
            self.count::<With<AwaitingCompaction>>(),
            self.count::<With<crate::title::PendingTitle>>(),
            self.count::<With<crate::title::AwaitingTitle>>(),
            self.count::<With<crate::pipeline::lane_batch::PendingBatch>>(),
        ];
        Fingerprint {
            markers,
            agents: self.agent_digest(),
        }
    }

    /// Any agent waiting on an in-flight async job (inference, tools, a
    /// transition choice, or compaction) whose completion will wake the driver.
    pub(super) fn has_async_inflight(&mut self) -> bool {
        self.count::<With<AwaitingInference>>() > 0
            || self.count::<With<AwaitingTools>>() > 0
            || self.count::<With<AwaitingTransitionResponse>>() > 0
            || self.count::<With<AwaitingCompaction>>() > 0
            || self.count::<With<crate::title::AwaitingTitle>>() > 0
    }
}
