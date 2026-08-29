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

    /// Sync agent list from on-disk run-state dir (background workers).
    pub(in crate::commands::dashboard) fn sync_from_run_state(&mut self) {
        // Cached listing: metas re-parse only when their files change, and a
        // finished run's is not even stat'ed every tick. Kept as the cache's
        // own `Arc`s: the loop below reads fields and clones the few strings
        // it keeps, and cloning 750 records ten times a second was a measurable
        // share of an idle dashboard's work.
        let runs: Vec<std::sync::Arc<runstate::RunMeta>> =
            runstate::list_runs_cached(&mut self.meta_cache);
        // Prune the per-run caches down to runs that still exist.
        let live_dirs: std::collections::HashSet<std::path::PathBuf> = runs
            .iter()
            .map(|run| runstate::run_dir(&run.run_id))
            .collect();
        self.stages_cache.retain_under(&live_dirs);
        self.context_cache.retain_under(&live_dirs);
        // The run whose context is worth reading this tick: the one the cursor
        // is on, which is the one the detail view draws. Taken before the loop
        // because the loop borrows `self.agents` mutably.
        //
        // Last frame's selection, which is this frame's: a keypress that moves
        // the cursor is handled before the tick that follows it, so opening a
        // run always finds its context already read.
        let showing = self
            .display_indices
            .get(self.selected)
            .and_then(|&i| self.agents.get(i))
            .map(|agent| agent.id.clone());
        // Where each known run sits, so the loop is not a search per run: 750
        // runs was 280,000 string compares a tick.
        let positions: std::collections::HashMap<String, usize> = self
            .agents
            .iter()
            .enumerate()
            .map(|(i, agent)| (agent.id.clone(), i))
            .collect();
        for run in runs {
            // A live open prompt from the daemon's hub (populated each tick by
            // `sync_interactions`) is the authoritative signal that this agent is
            // blocked on us - surface it regardless of the persisted status,
            // which can lag a tick behind the hub or (for tool-approval prompts)
            // never flips on its own.
            let pending_request = self.pending_interactions.get(&run.run_id).cloned();

            let stale = self.looks_stale(&run);
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

            // Read stages index + context snapshot through the poll caches,
            // hoisted out of the per-agent branches below so the cache borrow
            // does not overlap the `self.agents` borrow.
            let stages = runstate::read_stages_index_settled(
                &run.run_id,
                &mut self.stages_cache,
                runstate::settle_window(&run),
            );
            // The context window, only for the run on screen. It is the largest
            // file in a run directory by a wide margin, and the only thing that
            // reads it is the detail view's context card, which draws one run.
            //
            // Reading every run's cost what the history cost: a machine with
            // 750 runs behind it held 194 MB of context.json, and the dashboard
            // parsed all of it before its first frame and then kept every
            // snapshot alive in the cache. 1.3s to draw a list of runs, and
            // 267 MB resident to show one of them.
            let context_snapshot = if showing.as_deref() == Some(run.run_id.as_str()) {
                runstate::read_context_snapshot_cached(&run.run_id, &mut self.context_cache)
            } else {
                None
            };

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
                    depth: 0,
                    started_at: run.started_at,
                    last_progress_at: run.last_progress_at,
                    runtime_secs: run.active_runtime_secs(clock_now),
                    clock_now,
                    graph: load_stage_graph(&run.agent_path),
                    accepts_messages: true,
                    taint_summary: vec![], // default; stage-level control via agent state
                });
            }
        }
        self.update_display_indices();
        self.initial_sync_done = true;
    }
}
