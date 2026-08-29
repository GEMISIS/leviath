//! Whether the daemon is keeping up, and what to do when it is not.
//!
//! Three questions that only make sense together: how often the world is being
//! re-driven, whether a lane is wedged badly enough to need relief, and what
//! `lev daemon status` should say about it. All three read the same lane
//! snapshot, which is why they are one module rather than three.

use super::*;

impl WorldHost {
    /// Take stock once per safety re-drive: has anything moved, and are the lanes
    /// full? Updates the dead-cycle count and reports.
    ///
    /// A *dead cycle* is a whole re-drive interval in which some lane was at
    /// capacity with work queued behind it and no run observably moved. Both
    /// halves matter. Pressure on its own is just a busy daemon. Stillness on its
    /// own is an idle one, or one agent in a long inference with nobody waiting.
    /// Together they are the shape issue #191 reported: work to do, no capacity to
    /// do it with, and no sign of that ever changing.
    pub(super) fn observe_redrive(&mut self) {
        let snapshot = self.world.lane_snapshot();
        let progress = self.progress_fingerprint();
        let went_nowhere = snapshot.is_under_pressure() && self.last_progress == Some(progress);
        self.last_progress = Some(progress);
        self.dead_cycles = match went_nowhere {
            true => self.dead_cycles.saturating_add(1),
            false => 0,
        };
        self.log_lane_pressure(&snapshot);
        let relief = self.relieve_if_wedged(&snapshot);
        self.decay_relief_if_healthy(&snapshot);
        self.observe_lanes(&snapshot, relief);
    }

    /// The relief valve's give-back half: once the lane has been demonstrably
    /// healthy for [`HEALTHY_CYCLES_BEFORE_DECAY`] consecutive re-drives,
    /// reclaim one granted permit per further healthy cycle until the lane is
    /// back at its configured width.
    ///
    /// The guards are what keep this on the safe side of the wedge detection
    /// that granted the relief in the first place (issue #191): nothing is
    /// reclaimed while `dead_cycles` is non-zero (a wedge may be forming),
    /// nothing is reclaimed while the extra capacity is in use (`narrow` only
    /// takes *idle* permits), and the width can never drop below what the
    /// config asked for, because only permits this valve granted are counted.
    pub(super) fn decay_relief_if_healthy(&mut self, snapshot: &LaneSnapshot) {
        if self.relief_granted == 0 {
            self.healthy_cycles = 0;
            return;
        }
        let healthy = self.dead_cycles == 0 && snapshot.tools_queued == 0;
        self.healthy_cycles = match healthy {
            true => self.healthy_cycles.saturating_add(1),
            false => 0,
        };
        if self.healthy_cycles < HEALTHY_CYCLES_BEFORE_DECAY {
            return;
        }
        let narrowed = self.world.narrow_tool_lane(1);
        if narrowed > 0 {
            self.relief_granted -= narrowed;
            tracing::info!(
                narrowed,
                relief_granted = self.relief_granted,
                "the jam is over; reclaiming relief capacity from the tool lane"
            );
        }
    }

    /// Widen the tool lane if the daemon has been going nowhere long enough, and
    /// report how much capacity was added.
    ///
    /// Deliberately additive. The tempting reading of "force-reclaim stuck
    /// slots" is to kill whatever is holding them, and that is the wrong move
    /// here: a run parked on an `ask_user` is doing exactly what it should, and
    /// an operator who mistook `waiting` for `stuck` and started killing healthy
    /// runs is the story behind issue #184. Handing out more capacity unwedges a
    /// jammed lane without having to be right about which run deserves to die.
    ///
    /// Only the tool lane is widened. A full inference pool is a deliberate cap
    /// on requests in flight to a provider, and forcing extra ones past it would
    /// trade a wedge for a rate limit.
    ///
    /// Capped at one extra lane's worth over the daemon's life. If that is not
    /// enough, the problem is not capacity and more of it will not help.
    pub(super) fn relieve_if_wedged(&mut self, snapshot: &LaneSnapshot) -> usize {
        let threshold = self.dead_cycles_before_relief;
        if threshold == 0 || self.dead_cycles < threshold || !snapshot.tools_saturated {
            return 0;
        }
        // The snapshot's width already includes everything granted so far, so
        // back it out to get the lane's configured width - the budget.
        let configured = snapshot.tools_workers.saturating_sub(self.relief_granted);
        let remaining = configured.saturating_sub(self.relief_granted);
        let granted = self
            .world
            .relieve_tool_lane(remaining.min(snapshot.tools_queued));
        self.relief_granted += granted;
        tracing::error!(
            dead_cycles = self.dead_cycles,
            granted,
            relief_granted = self.relief_granted,
            tools_queued = snapshot.tools_queued,
            tools_parked = snapshot.tools_parked,
            "the tool lane has not drained in {} cycles; widening it by {granted}",
            self.dead_cycles
        );
        // Give the widened lane a fresh interval to show whether it helped,
        // rather than granting again on the very next re-drive.
        self.dead_cycles = 0;
        granted
    }

    /// How many dead cycles the daemon tolerates before widening the tool lane.
    /// `0` disables relief; detection and reporting are unaffected. Served from
    /// `[limits] dead_cycles_before_relief`.
    pub fn set_dead_cycles_before_relief(&mut self, cycles: u32) {
        self.dead_cycles_before_relief = cycles;
    }

    /// Hand one daemon-wide health sample to the telemetry sink.
    ///
    /// `relief` is the capacity granted on this sample, which is a per-sample
    /// figure rather than a running total: the sink accumulates it.
    pub(super) fn observe_lanes(&self, snapshot: &LaneSnapshot, relief: usize) {
        // Every `PipelineWorld::new` installs the sink resource (a no-op one
        // unless a host replaced it), so this is a hard invariant rather than a
        // branch - the same reasoning as `set_stream_inference`.
        self.world
            .world()
            .resource::<crate::telemetry::Telemetry>()
            .0
            .observe_lanes(leviath_core::telemetry::LaneHealth {
                agents_active: snapshot.agents.active,
                agents_waiting: snapshot.agents.waiting,
                tools_busy: snapshot.tools_busy,
                tools_queued: snapshot.tools_queued,
                tools_parked: snapshot.tools_parked,
                tools_workers: snapshot.tools_workers,
                dead_cycles: self.dead_cycles,
                relief_granted: relief,
            });
        // Sampled on the same tick, and unconditionally: a collector needs the
        // empty sample to see that a provider came *back*, not just that it
        // went away (issue #201).
        let down: Vec<leviath_core::telemetry::ProviderHealth> = self
            .world
            .open_circuits()
            .into_iter()
            .map(|c| leviath_core::telemetry::ProviderHealth {
                provider: c.provider,
                reason: c.reason.label().to_string(),
                consecutive_failures: c.consecutive_failures,
                retry_in_secs: c.retry_in_secs,
            })
            .collect();
        self.world
            .world()
            .resource::<crate::telemetry::Telemetry>()
            .0
            .observe_providers(&down);
    }

    /// The daemon's own health: lane occupancy plus the dead-cycle count.
    ///
    /// Served alongside every run listing, because "is this run stuck" and "is
    /// the daemon stuck" are answered by different numbers and an operator
    /// looking at one wants the other in the same breath.
    pub(crate) fn health(&self) -> DaemonHealth {
        let snapshot = self.world.lane_snapshot();
        DaemonHealth {
            agents: snapshot.agents,
            inference: snapshot.inference,
            inference_providers: snapshot.inference_providers,
            tools_busy: snapshot.tools_busy,
            tools_queued: snapshot.tools_queued,
            tools_parked: snapshot.tools_parked,
            tools_workers: snapshot.tools_workers,
            dead_cycles: self.dead_cycles,
            relief_granted: self.relief_granted,
            redrive_secs: self.redrive.as_secs(),
            providers_down: self.world.open_circuits(),
        }
    }

    /// A number that changes exactly when some run observably moves.
    ///
    /// Derived from the per-run snapshots `emit_events` already keeps to decide
    /// what to broadcast, so an unchanged fingerprint means "nothing happened
    /// that anyone watching would have been told about" - not merely "no event
    /// was sent", which would also be true of a daemon nobody is subscribed to.
    ///
    /// Summed rather than fed through one hasher because a `HashMap` has no
    /// iteration order to depend on. Every field it covers is either monotonic or
    /// hashed, so two different worlds colliding takes a deliberate effort.
    pub(super) fn progress_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut total = self.emitted.len() as u64;
        for entry in &self.emitted {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            entry.hash(&mut hasher);
            total = total.wrapping_add(hasher.finish());
        }
        total
    }

    /// Report what the lanes are holding.
    ///
    /// The daemon otherwise logs nothing per tick, by design - observation goes
    /// through the telemetry sink. But a wedged daemon emits no telemetry either,
    /// precisely because nothing is happening, so "frozen for hours" left no
    /// trace at all (issue #189). This is the one periodic line that can answer
    /// "is anything running, and what is it queued behind?".
    ///
    /// Quiet by default: `warn` once the daemon has been going nowhere, `info`
    /// while a lane is merely at capacity, `debug` otherwise, so an idle daemon
    /// says nothing above `debug`.
    pub(super) fn log_lane_pressure(&self, snapshot: &LaneSnapshot) {
        let agents = snapshot.agents.to_string();
        let inference = snapshot.inference_summary();
        if self.dead_cycles > 0 {
            tracing::warn!(
                dead_cycles = self.dead_cycles,
                agents = %agents,
                inference = %inference,
                tools_busy = snapshot.tools_busy,
                tools_workers = snapshot.tools_workers,
                tools_queued = snapshot.tools_queued,
                tools_parked = snapshot.tools_parked,
                "no progress while the lanes are full"
            );
        } else if snapshot.is_under_pressure() {
            tracing::info!(
                agents = %agents,
                inference = %inference,
                tools_busy = snapshot.tools_busy,
                tools_workers = snapshot.tools_workers,
                tools_queued = snapshot.tools_queued,
                tools_parked = snapshot.tools_parked,
                "lane heartbeat: at capacity with work queued"
            );
        } else {
            tracing::debug!(
                agents = %agents,
                inference = %inference,
                tools_busy = snapshot.tools_busy,
                tools_workers = snapshot.tools_workers,
                tools_queued = snapshot.tools_queued,
                tools_parked = snapshot.tools_parked,
                "lane heartbeat"
            );
        }
    }

    /// Override how often [`Self::serve`] re-drives the world with no wake.
    ///
    /// Exists so tests don't have to wait out the 30-second default; the daemon
    /// uses it as-is.
    #[cfg(test)]
    pub(crate) fn set_redrive_interval(&mut self, every: Duration) {
        self.redrive = every;
    }
}
