//! Turning world state into the event stream subscribers see.
//!
//! The change-detection pass: compare every run against the snapshot kept from
//! the previous cycle and emit only what actually moved. This is why an idle
//! daemon produces no events rather than a heartbeat of unchanged status.

use super::*;

impl WorldHost {
    /// Subscribe to [`WorldEvent`]s. The HTTP/WS gateway uses this (via the
    /// control transport's `Subscribe`) to push updates instead of polling.
    #[cfg(test)]
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<WorldEvent> {
        self.events.subscribe()
    }

    /// The world-event sender, handed to the control transport so a `Subscribe`
    /// connection can stream events.
    pub fn event_sender(&self) -> broadcast::Sender<WorldEvent> {
        self.events.clone()
    }

    /// Diff every registered run against its last-emitted snapshot and broadcast
    /// what changed (status/tokens/context/completion) plus any new interaction.
    /// Called after each drive to quiescence, so subscribers see every change.
    pub(super) fn emit_events(&mut self) {
        self.adopt_unregistered_runs();
        let pairs: Vec<(String, AgentId)> = self
            .by_run_id
            .iter()
            .map(|(k, &v)| (k.clone(), v))
            .collect();
        // Terminal agents to unload from memory this pass (their disk state is
        // preserved and still viewable). Collected during the loop, reaped after.
        // The listing row travels with each one: it is built here, while the
        // entity is untouched, rather than in the reap loop below, where the
        // daemon's reap hook has already had the world and is free to have taken
        // the components it reads.
        let mut to_reap: Vec<(String, Entity, RunListEntry)> = Vec::new();
        let mut to_park: Vec<(String, Entity, RunListEntry)> = Vec::new();
        let now = chrono::Utc::now().timestamp();
        // Read once for the whole pass: the operator can change these while
        // the daemon runs, and a pass that used two different lists partway
        // through would announce one run's threshold and not another's.
        let spend_notify = self.settings.spend_notify_usd();
        for (run_id, agent) in pairs {
            // Unwrapped once: everything below reaches into this world's ECS,
            // where same-world is true by construction.
            let entity = agent.entity();
            let Some(state) = self.world.world().get::<AgentState>(entity) else {
                continue; // reaped between registration and now
            };
            let agent_id = state.agent_id.clone();
            let status = status_str(&state.status);
            let terminal = crate::pipeline::is_terminal_status(&state.status);
            let cur = {
                let totals = self
                    .world
                    .world()
                    .get::<TokenTotals>(entity)
                    .copied()
                    .unwrap_or_default();
                let (context_tokens, _) = self
                    .world
                    .world()
                    .get::<ContextWindow>(entity)
                    .map(|w| (w.current_tokens, w.max_tokens))
                    .unwrap_or((0, 0));
                Emitted {
                    status,
                    stage: state.current_stage.clone(),
                    iteration: state.iteration,
                    tool_calls: totals.tool_calls,
                    accepts_messages: state.accepts_messages,
                    prompt_tokens: totals.prompt_tokens,
                    completion_tokens: totals.completion_tokens,
                    cached_tokens: totals.cached_tokens,
                    cache_write_tokens: totals.cache_write_tokens,
                    context_tokens,
                    // `priced_usd` rather than `total_usd()`: a run with an
                    // unpriced call has still spent what it could price, and
                    // reporting nothing until every call is priced would stay
                    // silent through exactly the run worth interrupting.
                    cost_micros: super::events::usd_to_micros(totals.cost.priced_usd),
                    cost_complete: totals.cost.total_usd().is_some(),
                    terminal,
                    wait_reason: self.wait_reason(agent),
                    title: self
                        .world
                        .world()
                        .get::<RunMetadata>(entity)
                        .and_then(|m| m.title.clone()),
                }
            };
            let max_tokens = self
                .world
                .world()
                .get::<ContextWindow>(entity)
                .map(|w| w.max_tokens)
                .unwrap_or(0);
            let prev = self.emitted.get(&run_id).cloned();

            if prev.is_none() {
                let metadata = self.world.world().get::<RunMetadata>(entity);
                let blueprint = metadata.map(|m| m.agent_name.clone()).unwrap_or_default();
                let parent_run_id = metadata.and_then(|m| m.parent_run_id.clone());
                let _ = self.events.send(WorldEvent::Spawned {
                    run_id: run_id.clone(),
                    agent_id: agent_id.clone(),
                    blueprint,
                    parent_run_id,
                });
            }

            // A run is created untitled and named a moment later, once the
            // titling call lands. Nothing else on the wire said so, which left
            // every client either polling each new run or showing the prompt's
            // first line until unrelated traffic made it re-read.
            if let Some(title) = cur.title.as_deref()
                && prev.as_ref().and_then(|e| e.title.as_deref()) != Some(title)
            {
                let _ = self.events.send(WorldEvent::Renamed {
                    run_id: run_id.clone(),
                    agent_id: agent_id.clone(),
                    title: title.to_string(),
                });
            }

            let status_key = |e: &Emitted| {
                (
                    e.status,
                    e.stage.clone(),
                    e.iteration,
                    e.tool_calls,
                    e.accepts_messages,
                    e.wait_reason.clone(),
                )
            };
            // The reason is part of the key, so a parent whose worker count
            // falls sends an event: that is progress, and a subscriber that
            // only heard "waiting" once would show a stale count for the rest
            // of the fan-out. Bounded by the number of workers.
            if prev.as_ref().map(status_key) != Some(status_key(&cur)) {
                let _ = self.events.send(WorldEvent::Status {
                    run_id: run_id.clone(),
                    agent_id: agent_id.clone(),
                    status: status.to_string(),
                    stage: cur.stage.clone(),
                    iteration: cur.iteration,
                    tool_calls: cur.tool_calls,
                    accepts_messages: cur.accepts_messages,
                    wait_reason: cur.wait_reason.clone(),
                    title: cur.title.clone(),
                });
            }

            let token_key = |e: &Emitted| {
                (
                    e.prompt_tokens,
                    e.completion_tokens,
                    e.cached_tokens,
                    e.cache_write_tokens,
                )
            };
            if prev.as_ref().map(token_key) != Some(token_key(&cur)) {
                let _ = self.events.send(WorldEvent::Tokens {
                    run_id: run_id.clone(),
                    agent_id: agent_id.clone(),
                    prompt_tokens: cur.prompt_tokens,
                    completion_tokens: cur.completion_tokens,
                    cached_tokens: cur.cached_tokens,
                    cache_write_tokens: cur.cache_write_tokens,
                });
            }

            // Every threshold the total passed since the last pass, in order.
            // Compared against what was emitted before rather than a per-run
            // "highest seen", so a threshold is announced once and a run that
            // jumps several in one pass announces each of them.
            let spent_before = prev.as_ref().map(|e| e.cost_micros).unwrap_or(0);
            for threshold in spend_notify.iter() {
                let crossing = super::events::usd_to_micros(*threshold);
                if spent_before < crossing && cur.cost_micros >= crossing {
                    let _ = self.events.send(WorldEvent::Spend {
                        run_id: run_id.clone(),
                        agent_id: agent_id.clone(),
                        threshold_usd: *threshold,
                        total_usd: cur.cost_micros as f64 / 1_000_000.0,
                        complete: cur.cost_complete,
                        stage: cur.stage.clone(),
                    });
                }
            }

            if prev.as_ref().map(|e| e.context_tokens) != Some(cur.context_tokens) {
                let _ = self.events.send(WorldEvent::Context {
                    run_id: run_id.clone(),
                    agent_id: agent_id.clone(),
                    total_tokens: cur.context_tokens,
                    max_tokens,
                });
            }

            let was_terminal = prev.as_ref().map(|e| e.terminal) == Some(true);
            if cur.terminal && !was_terminal {
                let _ = self.events.send(WorldEvent::Completed {
                    run_id: run_id.clone(),
                    agent_id: agent_id.clone(),
                    status: status.to_string(),
                    // Read off the live entity, not off disk: this fires the
                    // moment the run goes terminal, and the persist tick that
                    // writes `meta.json` has not necessarily run yet.
                    final_output: self
                        .world
                        .world()
                        .get::<crate::persistence::FinalOutput>(entity)
                        .map(|o| o.0.clone()),
                });
            }
            // Unload a terminal agent once its terminal state has been emitted (a
            // prior pass already saw it terminal, so the event went out and the
            // persistence lane captured it) and no live parent still needs it.
            // A run whose title is still on its way stays resident. Unloading
            // it would throw the answer away when it lands - `collect_title`
            // finds no entity and drops the title and the reason together -
            // and a run that finishes in a couple of seconds finishes well
            // inside the time one title call takes. Bounded by
            // `TITLE_HOLD_SECS` from the run's start, and `expire_title_hold`
            // ends the wait out loud rather than letting it lapse in silence.
            let title_owed = crate::title::title_outstanding(self.world.world(), entity, now);
            if cur.terminal && was_terminal && !title_owed && self.no_live_parent(entity) {
                let entry = self.entry_for(&run_id, entity, state);
                to_reap.push((run_id.clone(), entity, entry));
            }
            // Page a paused run out of the world once its paused state is on
            // its way to disk. Unlike `Waiting` (see the NOTE below), `Paused`
            // carries no live continuation - it is the one non-terminal state
            // whose whole meaning is "nothing is driving this" - and Resume,
            // Message and Cancel all page an unloaded run back in through
            // `resolve_or_reload`, exactly as a daemon restart would. Scoped
            // to standalone roots: a run with tree links or an open prompt
            // keeps the restart-equivalence question open and stays resident.
            if self.parkable(entity, &state.status) {
                let entry = self.entry_for(&run_id, entity, state);
                to_park.push((run_id.clone(), entity, entry));
            }
            // NOTE: non-terminal `Waiting` agents are intentionally NOT unloaded.
            // Every `Waiting` state carries a live, unpersisted continuation - a
            // blocked `ask` future (`AwaitingInteraction`), running fan-out workers
            // (`FanOutWaiting`), or pending children (`WaitingForChildren`) - so
            // flushing one to disk and paging it back cannot resume it (in-flight
            // interactions aren't persisted; the blocked future is gone). Only
            // terminal agents (fully on disk) are reaped, and paused ones parked.

            self.emitted.insert(run_id, cur);
        }

        // Reap: run the daemon's reap hook (sandbox teardown + tool-state drop)
        // while the entity is still valid, then despawn it and erase its host-map
        // entries. Iterating a snapshot of `by_run_id` above means removing here
        // is safe. The reaper is moved out for the loop to avoid borrowing `self`
        // twice, then restored.
        let mut reaper = self.reaper.take();
        let reaped_any = !to_reap.is_empty();
        for (run_id, entity, entry) in to_reap {
            if let Some(reaper) = reaper.as_mut() {
                reaper(&mut self.world, entity);
            }
            self.world.world_mut().despawn(entity);
            self.by_run_id.remove(&run_id);
            self.emitted.remove(&run_id);
            // The run leaves memory but not the listing: for a while yet it can
            // still say how it ended, which is the whole of issue #205.
            self.record_finished(entry, now);
        }
        // Park paused runs: same teardown as a reap (the reap hook drops the
        // agent's tool state and sandbox, which a page-in rebuilds the way a
        // daemon restart does), but the listing row moves to `parked` rather
        // than `finished` - the run is not over, it is just not resident.
        for (run_id, entity, entry) in to_park {
            if let Some(reaper) = reaper.as_mut() {
                reaper(&mut self.world, entity);
            }
            self.world.world_mut().despawn(entity);
            self.by_run_id.remove(&run_id);
            self.emitted.remove(&run_id);
            self.parked.insert(run_id, entry);
        }
        self.reaper = reaper;
        self.prune_finished(now);
        // Reaped runs answer no further prompts: drop their request ids from
        // the emitted-interaction set, which otherwise grows for the daemon's
        // life (the set is keyed by request id, so prune by what is still
        // pending - the same shape `cancel_tree` uses).
        if reaped_any {
            self.prune_emitted_interactions();
        }

        for (agent_id, request) in self.interactions.pending() {
            if self.emitted_interactions.insert(request.id.clone()) {
                let _ = self.events.send(WorldEvent::Interaction {
                    run_id: agent_id.clone(),
                    agent_id,
                    request,
                });
            }
        }
    }
}
