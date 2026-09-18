//! One tick's reconciliation of the agent list with the run directory.
//!
//! Split out of `state.rs` for size; a child module of `state` so it reads
//! the [`Dashboard`] fields directly. Tests live beside the struct.

use super::*;

impl Dashboard {
    /// The current wall-clock time in Unix seconds, via the injected clock.
    pub(super) fn now_secs(&self) -> i64 {
        (self.clock)()
    }

    /// Whether a run that claims to be live on disk actually has nothing driving
    /// it, and so should read STALE rather than ACTIVE.
    ///
    /// The rule itself lives in [`runstate::looks_abandoned`], because `lev ps
    /// --all` has to answer the same question for an external harness and the
    /// two must not drift apart.
    pub(super) fn looks_stale(&self, run: &runstate::RunMeta) -> bool {
        runstate::looks_abandoned(run, self.daemon_run_ids.as_ref(), self.now_secs())
    }

    /// Sync the agent list with the runs directory.
    ///
    /// With a [`RunFeed`](super::super::run_loader::RunFeed) attached (the
    /// real dashboard), this takes the loader thread's newest snapshot and
    /// never touches the disk itself; until the first snapshot lands there is
    /// nothing to show and the list says it is loading. Without one (tests),
    /// it reads the directory there and then, the same way.
    pub(in crate::commands::dashboard) fn sync_from_run_state(&mut self) {
        // The run whose context is worth reading: the one the cursor is on,
        // which is the one the detail view draws.
        //
        // Last frame's selection, which is this frame's: a keypress that moves
        // the cursor is handled before the tick that follows it.
        let showing = self
            .display_indices
            .get(self.selected)
            .and_then(|&i| self.agents.get(i))
            .map(|agent| agent.id.clone());
        let snapshot = match self.run_feed.as_mut() {
            Some(feed) => {
                feed.show(showing.as_deref());
                if let Some(latest) = feed.take() {
                    self.run_snapshot = Some(latest);
                }
                let Some(snapshot) = self.run_snapshot.clone() else {
                    return;
                };
                snapshot
            }
            None => std::sync::Arc::new(self.run_loader.collect(showing.as_deref(), true)),
        };
        self.runs_loading = false;
        self.apply_run_snapshot(&snapshot);
    }

    /// Reconcile the agent list with one [`RunSnapshot`]: new runs become
    /// rows, known ones take the snapshot's fields, and the clocks and
    /// staleness that depend on "now" are worked out afresh.
    ///
    /// Runs every tick, with the same snapshot until a newer one lands, so it
    /// reads memory only.
    ///
    /// [`RunSnapshot`]: super::super::run_loader::RunSnapshot
    pub(super) fn apply_run_snapshot(&mut self, snapshot: &super::super::run_loader::RunSnapshot) {
        // A run deleted here after this snapshot was read is still in it;
        // adding it back would have the row the user just deleted reappear
        // until the next snapshot. Once a snapshot read after the delete
        // arrives, the disk has the final word again.
        self.deleted_runs
            .retain(|_, deleted_at| *deleted_at > snapshot.taken_at);
        // Where each known run sits, so the loop is not a search per run.
        let positions: std::collections::HashMap<String, usize> = self
            .agents
            .iter()
            .enumerate()
            .map(|(i, agent)| (agent.id.clone(), i))
            .collect();
        let mut order_changed = !self.initial_sync_done;
        for entry in &snapshot.runs {
            let run = &entry.meta;
            if self.deleted_runs.contains_key(&run.run_id) {
                continue;
            }
            // A live open prompt from the daemon's hub (populated each tick by
            // `sync_interactions`) is the authoritative signal that this agent is
            // blocked on us - surface it regardless of the persisted status,
            // which can lag a tick behind the hub or (for tool-approval prompts)
            // never flips on its own.
            let pending_request = self.pending_interactions.get(&run.run_id).cloned();

            let stale = self.looks_stale(run);
            // The moment to read the run's working clock at. Nothing is driving
            // an abandoned run, so its clock stopped when its record was last
            // written; reading that one against the wall clock would have its
            // timer climb forever.
            let clock_now = match stale {
                true => run.updated_at,
                false => self.now_secs(),
            };

            let status = match run.status {
                RunStatus::Starting | RunStatus::Running => {
                    if pending_request.is_some() {
                        AgentDisplayStatus::Waiting
                    } else if stale {
                        AgentDisplayStatus::Stale
                    } else {
                        AgentDisplayStatus::Active
                    }
                }
                RunStatus::WaitingInput => AgentDisplayStatus::Waiting,
                RunStatus::Paused => AgentDisplayStatus::Paused,
                RunStatus::Complete => AgentDisplayStatus::Complete,
                RunStatus::CompleteInteractive => AgentDisplayStatus::CompleteInteractive,
                RunStatus::Error => {
                    AgentDisplayStatus::Error(run.error.clone().unwrap_or_default())
                }
                RunStatus::Cancelled => AgentDisplayStatus::Cancelled,
            };

            // Attach the pending interaction whenever the hub holds one, or for
            // a CompleteInteractive agent (which accepts a follow-up message even
            // with no open request).
            let needs_input =
                pending_request.is_some() || matches!(run.status, RunStatus::CompleteInteractive);
            let (waiting_prompt, pending_request) = if needs_input {
                (
                    pending_request.as_ref().map(|r| r.prompt.clone()),
                    pending_request,
                )
            } else {
                (None, None)
            };

            let stages = entry.stages.clone();
            // The context window is read for the run on screen only: it is the
            // largest file in a run directory by a wide margin, and the only
            // thing that reads it is the detail view's context card.
            let context_snapshot = snapshot
                .context
                .as_ref()
                .filter(|(id, _)| *id == run.run_id)
                .map(|(_, context)| context.clone());

            if let Some(agent) = positions.get(&run.run_id).map(|&i| &mut self.agents[i]) {
                let prev_status_was_active = matches!(
                    agent.status,
                    AgentDisplayStatus::Active | AgentDisplayStatus::Waiting
                );
                let now_needs_input = needs_input;

                // Toast on terminal state transitions
                let name = agent
                    .title
                    .clone()
                    .unwrap_or(truncate(&agent.blueprint_name, 20));
                if prev_status_was_active {
                    if let AgentDisplayStatus::Error(msg) = &status {
                        let preview = if msg.is_empty() {
                            String::new()
                        } else {
                            format!(": {}", truncate(msg, 40))
                        };
                        Self::push_toast(
                            &mut self.toasts,
                            format!("Agent run '{}' failed{}", name, preview),
                            ToastLevel::Error,
                            50,
                        );
                    } else if matches!(
                        status,
                        AgentDisplayStatus::Complete | AgentDisplayStatus::CompleteInteractive
                    ) {
                        Self::push_toast(
                            &mut self.toasts,
                            format!("Agent run '{}' completed", name),
                            ToastLevel::Info,
                            35,
                        );
                    }
                }

                // Only these decide where the row sits and whether a filter
                // matches it; anything else changing leaves the order alone.
                if agent.status != status
                    || agent.last_progress_at != run.last_progress_at
                    || agent.title != run.title
                {
                    order_changed = true;
                }
                agent.stage = run.current_stage.clone();
                agent.stage_index = run.stage_index;
                agent.num_stages = run.num_stages;
                agent.iteration = run.iteration;
                agent.tokens_in = run.prompt_tokens;
                agent.tokens_out = run.completion_tokens;
                agent.cached_tokens = run.cached_tokens;
                agent.title = run.title.clone();
                agent.clock_now = clock_now;
                agent.runtime_secs = run.active_runtime_secs(clock_now);
                agent.status = status;
                agent.workdir = run.workdir.clone();
                agent.context_snapshot = context_snapshot.clone();
                agent.stages = stages;
                agent.last_progress_at = run.last_progress_at;

                if now_needs_input {
                    if waiting_prompt.is_some() {
                        let pending_id = pending_request
                            .as_ref()
                            .map(|r| r.id.as_str())
                            .unwrap_or("");
                        let already_answered = agent
                            .last_answered_request_id
                            .as_deref()
                            .map(|a| !a.is_empty() && a == pending_id)
                            .unwrap_or(false);
                        if !already_answered {
                            if agent.waiting_prompt.is_none()
                                && waiting_prompt.is_some()
                                && matches!(run.status, RunStatus::WaitingInput)
                            {
                                // Newly needs input - toast (not for CompleteInteractive which is optional)
                                let name = agent
                                    .title
                                    .clone()
                                    .unwrap_or(truncate(&agent.blueprint_name, 20));
                                Self::push_toast(
                                    &mut self.toasts,
                                    format!("Agent run '{}' needs input", name),
                                    ToastLevel::Warning,
                                    35,
                                );
                            }
                            agent.waiting_prompt = waiting_prompt;
                            agent.pending_request = pending_request;
                            agent.wait_reason = run.waiting_on.clone();
                        }
                    }
                } else {
                    agent.waiting_prompt = None;
                    agent.pending_request = None;
                    agent.last_answered_request_id = None;
                }
                // Read every tick, not only while waiting: a run that stops
                // being parked must stop claiming a reason.
                agent.wait_reason = run.waiting_on.clone();
            } else {
                // New agent - toasts only after the initial sync (avoid flooding on startup)
                if self.initial_sync_done {
                    if needs_input
                        && waiting_prompt.is_some()
                        && matches!(run.status, RunStatus::WaitingInput)
                    {
                        let name = run.title.clone().unwrap_or(truncate(&run.agent_name, 20));
                        Self::push_toast(
                            &mut self.toasts,
                            format!("Agent run '{}' needs input", name),
                            ToastLevel::Warning,
                            35,
                        );
                    }
                    if matches!(
                        run.status,
                        RunStatus::Complete | RunStatus::CompleteInteractive
                    ) {
                        let name = run.title.clone().unwrap_or(truncate(&run.agent_name, 20));
                        Self::push_toast(
                            &mut self.toasts,
                            format!("Agent run '{}' completed", name),
                            ToastLevel::Info,
                            35,
                        );
                    }
                }
                order_changed = true;
                self.agents.push(DashboardAgent {
                    id: run.run_id.clone(),
                    blueprint_name: run.agent_name.clone(),
                    stage: run.current_stage.clone(),
                    stage_index: run.stage_index,
                    num_stages: run.num_stages,
                    status,
                    tokens_in: run.prompt_tokens,
                    tokens_out: run.completion_tokens,
                    cached_tokens: run.cached_tokens,
                    iteration: run.iteration,
                    broken_scripts: run.flags.broken_scripts.clone(),
                    waiting_prompt,
                    wait_reason: run.waiting_on.clone(),
                    pending_request,
                    last_answered_request_id: None,
                    context_snapshot,
                    stages,
                    workdir: run.workdir.clone(),
                    task: run.task.clone(),
                    title: run.title.clone(),
                    model: run.model.clone(),
                    parent_id: run.parent_run_id.clone(),
                    started_at: run.started_at,
                    last_progress_at: run.last_progress_at,
                    runtime_secs: run.active_runtime_secs(clock_now),
                    clock_now,
                    graph: entry.graph.clone(),
                    accepts_messages: true,
                });
            }
        }
        // Re-sorting and re-nesting thousands of rows is wasted on the ticks
        // where only clocks move.
        if order_changed {
            self.update_display_indices();
        }
        self.initial_sync_done = true;
    }
}
