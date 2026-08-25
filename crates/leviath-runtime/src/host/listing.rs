//! What `lev ps` and the control socket see: one row per run, live from the
//! world rather than re-read from disk.
//!
//! [`wait_reason`](WorldHost::wait_reason) is the interesting part. A status of
//! `Waiting` says nothing about whether a person is needed, which is what issue
//! #184 was about, so the row carries why.
//!
//! Retention lives here too, and deliberately: a finished run stays listable for
//! a while after it ends, so how long the list keeps one is a property of the
//! list rather than of the health bookkeeping it used to sit beside.

use super::*;

impl WorldHost {
    /// Why `entity` is [`AgentStatus::Waiting`], read off the markers the engine
    /// already maintains. `None` when the agent is not waiting, or when it is
    /// waiting for a reason nothing has claimed.
    ///
    /// Order matters. A taint-gate block and a stage checkpoint each open a hub
    /// request of their own, so both also carry [`AwaitingInteraction`]; asking
    /// the specific markers first is what keeps them from all reporting as a
    /// generic prompt.
    pub fn wait_reason(&self, agent: crate::world::AgentId) -> Option<WaitReason> {
        let world = self.world.world();
        // An id from another world names a different agent here, which would
        // report that one's wait reason as this run's.
        let entity = agent.resolve_in(world)?;
        let state = world.get::<AgentState>(entity)?;
        // The precedence itself lives in `leviath_core`, shared with the
        // persistence system that writes the same answer to `meta.json`: two
        // copies of it would disagree the first time either was edited.
        leviath_core::run_meta::wait_reason_from(
            matches!(state.status, AgentStatus::Waiting | AgentStatus::Paused),
            &leviath_core::run_meta::WaitMarkers {
                gate_prompt: world
                    .get::<crate::gate_prompt::AwaitingGatePrompt>(entity)
                    .is_some(),
                interaction_point: world
                    .get::<crate::interaction_points::AwaitingInteractionPoint>(entity)
                    .is_some(),
                fan_out_outstanding: world
                    .get::<crate::fanout::FanOutWaiting>(entity)
                    .map(|f| f.outstanding()),
                children_outstanding: world
                    .get::<crate::pipeline::WaitingForChildren>(entity)
                    .map(|_| {
                        world
                            .get::<SubAgentChildren>(entity)
                            .map(|c| {
                                c.children
                                    .iter()
                                    .filter(|&&child| {
                                        world.get::<AgentState>(child).is_some_and(|s| {
                                            !crate::pipeline::is_terminal_status(&s.status)
                                        })
                                    })
                                    .count()
                            })
                            .unwrap_or(0)
                    }),
                // The hub is keyed by agent id, and one agent can only be
                // parked on one prompt at a time, so the first match is the
                // one blocking it.
                interaction: self
                    .interactions
                    .pending()
                    .into_iter()
                    .find(|(agent_id, _)| *agent_id == state.agent_id)
                    .map(|(_, req)| req.kind),
                awaiting_interaction: world.get::<AwaitingInteraction>(entity).is_some(),
                needs_setup: world
                    .get::<crate::pipeline::PausedForSetup>(entity)
                    .map(|p| leviath_core::run_meta::SetupNeeded {
                        blocker: p.blocker,
                        remedy: p.remedy.clone(),
                    }),
            },
        )
    }

    /// One listing row for a run, read off the live world.
    ///
    /// Shared by [`Self::list`] and by the unload path in [`Self::emit_events`],
    /// so a run's last row is built exactly the way every row before it was.
    /// Takes the state rather than looking it up because the unload path already
    /// holds one, and a `None` it could never return would be a branch nothing
    /// can reach.
    pub(super) fn entry_for(
        &self,
        run_id: &str,
        entity: Entity,
        state: &AgentState,
    ) -> RunListEntry {
        let world = self.world.world();
        let metadata = world.get::<RunMetadata>(entity);
        let has_output = world
            .get::<crate::persistence::FinalOutput>(entity)
            .is_some();
        RunListEntry {
            run_id: run_id.to_string(),
            title: metadata.and_then(|m| m.title.clone()),
            status: state.status.clone(),
            wait_reason: self.wait_reason(crate::world::AgentId::in_world(world, entity)),
            stage: state.current_stage.clone(),
            stage_index: world
                .get::<crate::pipeline::StageCursor>(entity)
                .map(|c| c.index),
            num_stages: metadata.map(|m| m.num_stages),
            iteration: state.iteration,
            tool_calls: world.get::<TokenTotals>(entity).map_or(0, |t| t.tool_calls),
            last_progress_at: world
                .get::<crate::pipeline::PersistWatermark>(entity)
                .and_then(|w| w.last_progress_at()),
            unattended: metadata.is_some_and(|m| m.unattended),
            splits_degraded: world
                .get::<crate::persistence::RunOutcomeFlags>(entity)
                .map_or(0, |f| f.0.splits_degraded),
            broken_scripts: world
                .get::<crate::persistence::RunOutcomeFlags>(entity)
                .map(|f| f.0.broken_scripts.clone())
                .unwrap_or_default(),
            empty_output: world
                .get::<crate::persistence::RunOutcomeFlags>(entity)
                .is_some_and(|f| {
                    // `produced_output` lives on the component only after a
                    // persist tick fills it, so it is answered from the live
                    // entity here. Without this, a researcher that submitted a
                    // perfectly good answer still read `complete (no output)`
                    // in `lev ps` while `meta.json` said otherwise - the exact
                    // drift between the two surfaces that one shared
                    // `is_empty_output` exists to prevent.
                    let mut flags = f.0.clone();
                    flags.produced_output = has_output;
                    crate::persistence::is_empty_output(&state.status, &flags)
                }),
            read_paths: metadata.and_then(|m| m.read_paths),
            has_final_output: has_output,
        }
    }

    /// List every known live run with the context an operator needs to read its
    /// status: why it is waiting, where it is, and when it last moved.
    pub(super) fn list(&self) -> Vec<RunListEntry> {
        let world = self.world.world();
        self.by_run_id
            .iter()
            .filter_map(|(run_id, &agent)| {
                let state = world.get::<AgentState>(agent.entity())?;
                Some(self.entry_for(run_id, agent.entity(), state))
            })
            // Parked (paused, paged-out) runs are still the daemon's runs; an
            // operator must not lose sight of one just because it left memory.
            .chain(self.parked.values().cloned())
            .collect()
    }

    /// The runs unloaded recently enough to still be reported, oldest first.
    ///
    /// Kept apart from [`Self::list`] rather than folded into it because
    /// "running now" and "finished a moment ago" are different questions, and
    /// two callers already depend on the first one: `lev daemon status` counts
    /// the hosted agents, and the dashboard uses the listing to decide which
    /// runs the daemon still holds.
    pub(super) fn finished(&self) -> Vec<RunListEntry> {
        self.finished
            .iter()
            .map(|(_, entry)| entry.clone())
            .collect()
    }

    /// How long a run stays in the listing after the daemon unloads it. `0`
    /// keeps none, which is how the listing behaved before issue #205. Served
    /// from `[limits] finished_retention_secs`.
    pub fn set_finished_retention_secs(&mut self, secs: u64) {
        self.finished_retention_secs = secs;
    }

    /// Keep `entry` in the listing as a run that finished at `at`.
    ///
    /// One row per run: an id already held is replaced rather than duplicated,
    /// so however often a run is unloaded it is reported once.
    ///
    /// `last_progress_at` is filled in from `at` when the run never persisted a
    /// snapshot. That is not a guess. A run that died on its first inference has
    /// no watermark to read, and the listing would show its age as `-` - which
    /// is the one thing an operator or a scheduler most wants to know about a
    /// run that failed instantly. For a run being unloaded, the unload is the
    /// last thing that happened to it.
    pub(super) fn record_finished(&mut self, mut entry: RunListEntry, at: i64) {
        if self.finished_retention_secs == 0 {
            return;
        }
        entry.last_progress_at.get_or_insert(at);
        self.finished
            .retain(|(_, held)| held.run_id != entry.run_id);
        self.finished.push_back((at, entry));
        while self.finished.len() > MAX_RETAINED_FINISHED {
            self.finished.pop_front();
        }
    }

    /// Drop unloaded runs that have outlived the retention window.
    ///
    /// `now` is passed in rather than read here so a test can age the buffer
    /// without sleeping through the window, the same reason `lev ps`'s
    /// `format_runs` takes it. Called once per [`Self::emit_events`], which the
    /// serve loop runs before it handles any control op, so a listing never has
    /// to prune on the way out.
    pub(super) fn prune_finished(&mut self, now: i64) {
        let window = self.finished_retention_secs as i64;
        while let Some(&(at, _)) = self.finished.front() {
            if now.saturating_sub(at) <= window {
                break;
            }
            self.finished.pop_front();
        }
    }
}
