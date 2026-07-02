//! Dashboard state struct and core state-management methods.

use leviath_runtime::{AgentEngine, AgentState, AgentStatus, ContextWindow};
use ratatui::widgets::TableState;
use tokio::sync::mpsc;
use tui_textarea::TextArea;

use super::graph::load_graph_info;
use super::helpers::truncate;
use super::types::*;
use crate::interaction;
use crate::runstate::{self, RunStatus};

/// The interactive dashboard state.
pub(crate) struct Dashboard {
    pub(super) agents: Vec<DashboardAgent>,
    pub(super) selected: usize,
    pub(super) log: Vec<LogEntry>,
    /// Multi-line input textarea (active when input_mode = true and kind = FreeText).
    pub(super) input_textarea: TextArea<'static>,
    pub(super) input_mode: bool,
    /// True when the full-screen detail view is open for the selected agent
    pub(super) detail_view: bool,
    pub(super) event_rx: mpsc::UnboundedReceiver<AgentEvent>,
    pub(super) event_tx: mpsc::UnboundedSender<AgentEvent>,
    pub(super) cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    pub(super) table_state: TableState,
    pub(super) should_quit: bool,
    /// True when the delete-agent confirmation popup is open
    pub(super) confirm_delete: bool,
    /// Scroll offset for detail view content: 0 = bottom (auto-scroll), >0 = scrolled up
    pub(super) detail_scroll: usize,
    /// Selected option index for MultipleChoice/ToolApproval/Confirm input
    pub(super) choice_selected: usize,
    /// Which stage tab is currently focused in the detail view
    pub(super) selected_stage: usize,
    /// Whether the content pane shows Output or Logs — global across all stage tabs.
    pub(super) stage_content_mode: StageContentMode,
    /// True after the first sync completes; suppresses startup toasts for pre-existing state.
    pub(super) initial_sync_done: bool,
    /// Monotonic tick counter for animations (spinner, toast timeouts)
    pub(super) tick_count: u64,
    /// Active toast notifications
    pub(super) toasts: Vec<Toast>,
    /// True when the help overlay (?) is shown
    pub(super) show_help: bool,
    /// Scroll offset within the review body pane (present_for_review).
    pub(super) review_scroll: usize,
    // ── Search ────────────────────────────────────────────────────────────────
    /// True when the user is typing a search query (entered with `/`).
    pub(super) search_mode: bool,
    /// Current search query (empty = no active search).
    pub(super) search_query: String,
    /// Index into the matched-lines list of the currently highlighted match.
    pub(super) search_match_idx: usize,
    // ── Main list filter ──────────────────────────────────────────────────────
    /// True when the user is typing a filter query in the main agent list.
    pub(super) list_search_mode: bool,
    /// Current filter query for the main list.
    pub(super) list_search_query: String,
    /// Sorted + filtered indices into self.agents (drives both display and selection).
    pub(super) display_indices: Vec<usize>,
}

impl Dashboard {
    pub(super) fn new(cmd_tx: mpsc::UnboundedSender<EngineCommand>) -> Self {
        let (event_tx, event_rx) = create_event_channel();
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        // Seed the in-memory log buffer from the tail of the persistent log so
        // the panel shows recent history immediately on launch (not a blank panel).
        let log = Self::load_log_seed();

        Self {
            agents: Vec::new(),
            selected: 0,
            log,
            input_textarea: TextArea::default(),
            input_mode: false,
            detail_view: false,
            event_rx,
            event_tx,
            cmd_tx,
            table_state,
            should_quit: false,
            confirm_delete: false,
            detail_scroll: 0,
            choice_selected: 0,
            selected_stage: 0,
            stage_content_mode: StageContentMode::Output,
            initial_sync_done: false,
            tick_count: 0,
            toasts: Vec::new(),
            show_help: false,
            review_scroll: 0,
            search_mode: false,
            search_query: String::new(),
            search_match_idx: 0,
            list_search_mode: false,
            list_search_query: String::new(),
            display_indices: Vec::new(),
        }
    }

    /// Read the last 32 KB of dashboard.log and convert each line into a
    /// `LogEntry` for the initial in-memory buffer.
    fn load_log_seed() -> Vec<LogEntry> {
        let tail = runstate::tail_file(&runstate::dashboard_log_path(), 32_768);
        Self::parse_log_lines(&tail)
    }

    /// Core parsing logic of [`load_log_seed`], split out so it can be
    /// exercised in tests against controlled input -- `dashboard_log_path()`
    /// always points at the real, shared `~/.leviath/dashboard.log`, with no
    /// injectable override, so tests can't safely control its content.
    fn parse_log_lines(tail: &str) -> Vec<LogEntry> {
        let mut entries = Vec::new();
        for line in tail.lines() {
            // Lines are written as "YYYY-MM-DD HH:MM:SS <message>"
            // Split off the first two space-separated tokens as the timestamp.
            let mut parts = line.splitn(3, ' ');
            let date = parts.next().unwrap_or("");
            let time = parts.next().unwrap_or("");
            let message = parts.next().unwrap_or(line).to_string();
            if message.is_empty() {
                continue;
            }
            // In the panel we show only the time portion for compactness.
            let timestamp = if time.is_empty() {
                date.to_string()
            } else {
                time.to_string()
            };
            entries.push(LogEntry { timestamp, message });
        }
        entries
    }

    /// Recompute display_indices: sorted by status priority then recency, filtered by list_search_query.
    pub(super) fn update_display_indices(&mut self) {
        let query = self.list_search_query.to_lowercase();
        let status_priority = |s: &AgentDisplayStatus| -> u8 {
            match s {
                AgentDisplayStatus::Active => 0,
                AgentDisplayStatus::Waiting => 1,
                AgentDisplayStatus::CompleteInteractive => 2,
                AgentDisplayStatus::Complete => 3,
                AgentDisplayStatus::Error(_) => 4,
                AgentDisplayStatus::Idle => 5,
                AgentDisplayStatus::Cancelled => 6,
            }
        };
        let mut indices: Vec<usize> = (0..self.agents.len())
            .filter(|&i| {
                if query.is_empty() {
                    return true;
                }
                let a = &self.agents[i];
                a.blueprint_name.to_lowercase().contains(&query)
                    || a.title
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&query)
                    || a.task.to_lowercase().contains(&query)
                    || a.status.to_string().to_lowercase().contains(&query)
            })
            .collect();
        indices.sort_by(|&a, &b| {
            let pa = status_priority(&self.agents[a].status);
            let pb = status_priority(&self.agents[b].status);
            pa.cmp(&pb)
                .then(self.agents[b].started_at.cmp(&self.agents[a].started_at))
        });
        // Preserve selection: try to keep the same agent highlighted after recompute
        let prev_id = self
            .display_indices
            .get(self.selected)
            .and_then(|&i| self.agents.get(i))
            .map(|a| a.id.clone());
        self.display_indices = indices;
        if let Some(id) = prev_id {
            if let Some(pos) = self
                .display_indices
                .iter()
                .position(|&i| self.agents.get(i).map(|a| a.id == id).unwrap_or(false))
            {
                self.selected = pos;
            } else {
                self.selected = 0;
            }
        }
        if self.display_indices.is_empty() {
            self.selected = 0;
            self.table_state.select(None);
        } else {
            self.selected = self.selected.min(self.display_indices.len() - 1);
            self.table_state.select(Some(self.selected));
        }
    }

    pub(super) fn selected_agent(&self) -> Option<&DashboardAgent> {
        self.display_indices
            .get(self.selected)
            .and_then(|&i| self.agents.get(i))
    }

    pub(super) fn selected_agent_mut(&mut self) -> Option<&mut DashboardAgent> {
        let idx = self.display_indices.get(self.selected).copied()?;
        self.agents.get_mut(idx)
    }

    pub(super) fn selected_agent_raw_idx(&self) -> Option<usize> {
        self.display_indices.get(self.selected).copied()
    }

    pub(super) fn add_log(&mut self, msg: String) {
        let now = chrono::Local::now();
        let timestamp = now.format("%H:%M:%S").to_string();
        self.log.push(LogEntry {
            timestamp,
            message: msg.clone(),
        });
        if self.log.len() > 200 {
            self.log.remove(0);
        }
        // Persist to the append-only dashboard log.
        runstate::append_dashboard_log(&msg);
    }

    #[allow(dead_code)]
    pub(super) fn push_toast(&mut self, msg: impl Into<String>, level: ToastLevel) {
        // 25 ticks ≈ 2.5 seconds at 100ms/tick
        self.toasts.push(Toast {
            message: msg.into(),
            remaining_ticks: 25,
            level,
        });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
    }

    pub(super) fn tick_toasts(&mut self) {
        self.toasts.retain_mut(|t| {
            if t.remaining_ticks > 0 {
                t.remaining_ticks -= 1;
            }
            t.remaining_ticks > 0
        });
    }

    /// Sync agent list from on-disk run-state dir (background workers).
    pub(super) fn sync_from_run_state(&mut self) {
        let runs = runstate::list_runs();
        let dummy = bevy_ecs::prelude::Entity::from_raw(u32::MAX);
        for run in runs {
            let status = match run.status {
                RunStatus::Starting | RunStatus::Running => AgentDisplayStatus::Active,
                RunStatus::WaitingInput => AgentDisplayStatus::Waiting,
                RunStatus::Complete => AgentDisplayStatus::Complete,
                RunStatus::CompleteInteractive => AgentDisplayStatus::CompleteInteractive,
                RunStatus::Error => {
                    AgentDisplayStatus::Error(run.error.clone().unwrap_or_default())
                }
                RunStatus::Cancelled => AgentDisplayStatus::Cancelled,
            };

            // For WaitingInput and CompleteInteractive agents, read the pending interaction from disk once
            let needs_input = matches!(
                run.status,
                RunStatus::WaitingInput | RunStatus::CompleteInteractive
            );
            let (waiting_prompt, pending_request) = if needs_input {
                let req = interaction::read_request(&run.run_id);
                (req.as_ref().map(|r| r.prompt.clone()), req)
            } else {
                (None, None)
            };

            // Read stages index
            let stages = runstate::read_stages_index(&run.run_id);

            if let Some(agent) = self.agents.iter_mut().find(|a| a.id == run.run_id) {
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
                        self.toasts.push(Toast {
                            message: format!("Agent '{}' failed{}", name, preview),
                            remaining_ticks: 50,
                            level: ToastLevel::Error,
                        });
                    } else if matches!(
                        status,
                        AgentDisplayStatus::Complete | AgentDisplayStatus::CompleteInteractive
                    ) {
                        self.toasts.push(Toast {
                            message: format!("Agent '{}' completed", name),
                            remaining_ticks: 35,
                            level: ToastLevel::Info,
                        });
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
                agent.pid = run.pid;
                let now_is_waiting = matches!(
                    status,
                    AgentDisplayStatus::Waiting | AgentDisplayStatus::CompleteInteractive
                );
                if now_is_waiting {
                    // Entering or staying in a wait — freeze timer at entry point
                    if agent.active_until.is_none() {
                        agent.active_until = Some(run.updated_at);
                    }
                } else {
                    // Leaving a wait — accumulate how long we were waiting
                    if let Some(wait_start) = agent.active_until.take() {
                        agent.waiting_secs += (run.updated_at - wait_start).max(0) as u64;
                    }
                }
                agent.status = status;
                agent.workdir = run.workdir.clone();
                agent.context_snapshot = runstate::read_context_snapshot(&run.run_id);
                agent.stages = stages;

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
                                // Newly needs input — toast (not for CompleteInteractive which is optional)
                                let name = agent
                                    .title
                                    .clone()
                                    .unwrap_or(truncate(&agent.blueprint_name, 20));
                                self.toasts.push(Toast {
                                    message: format!("Agent '{}' needs input", name),
                                    remaining_ticks: 35,
                                    level: ToastLevel::Warning,
                                });
                            }
                            agent.waiting_prompt = waiting_prompt;
                            agent.pending_request = pending_request;
                        }
                    }
                } else {
                    agent.waiting_prompt = None;
                    agent.pending_request = None;
                    agent.last_answered_request_id = None;
                }
            } else {
                // New agent — toasts only after the initial sync (avoid flooding on startup)
                if self.initial_sync_done {
                    if needs_input
                        && waiting_prompt.is_some()
                        && matches!(run.status, RunStatus::WaitingInput)
                    {
                        let name = run.title.clone().unwrap_or(truncate(&run.agent_name, 20));
                        self.toasts.push(Toast {
                            message: format!("Agent '{}' needs input", name),
                            remaining_ticks: 35,
                            level: ToastLevel::Warning,
                        });
                    }
                    if matches!(
                        run.status,
                        RunStatus::Complete | RunStatus::CompleteInteractive
                    ) {
                        let name = run.title.clone().unwrap_or(truncate(&run.agent_name, 20));
                        self.toasts.push(Toast {
                            message: format!("Agent '{}' completed", name),
                            remaining_ticks: 35,
                            level: ToastLevel::Info,
                        });
                    }
                }
                self.agents.push(DashboardAgent {
                    id: run.run_id.clone(),
                    blueprint_name: run.agent_name.clone(),
                    agent_path: run.agent_path.clone(),
                    stage: run.current_stage.clone(),
                    stage_index: run.stage_index,
                    num_stages: run.num_stages,
                    status,
                    tokens_in: run.prompt_tokens,
                    tokens_out: run.completion_tokens,
                    cached_tokens: run.cached_tokens,
                    context_tokens: (0, 0),
                    iteration: run.iteration,
                    waiting_prompt,
                    pending_request,
                    last_answered_request_id: None,
                    context_snapshot: runstate::read_context_snapshot(&run.run_id),
                    stages,
                    entity: dummy,
                    is_run_state: true,
                    pid: run.pid,
                    workdir: run.workdir.clone(),
                    task: run.task.clone(),
                    title: run.title.clone(),
                    model: run.model.clone(),
                    parent_id: None,
                    depth: 0,
                    started_at: run.started_at,
                    active_until: if matches!(
                        run.status,
                        RunStatus::WaitingInput | RunStatus::CompleteInteractive
                    ) {
                        Some(run.updated_at)
                    } else {
                        None
                    },
                    waiting_secs: 0,
                    graph_info: load_graph_info(&run.agent_path),
                    accepts_messages: true, // default; stage-level control via agent state
                });
            }
        }
        self.update_display_indices();
        self.initial_sync_done = true;
    }

    /// Sync agent state from the ECS world (in-process agents only).
    pub(super) fn sync_agent_state_from_world(&mut self, engine: &AgentEngine) {
        for agent in &mut self.agents {
            if agent.is_run_state {
                continue;
            }
            if let Some(state) = engine.world().get::<AgentState>(agent.entity) {
                agent.iteration = state.iteration;
                agent.stage = state.current_stage.clone();
                match &state.status {
                    AgentStatus::Active => agent.status = AgentDisplayStatus::Active,
                    AgentStatus::Waiting => {
                        agent.status = AgentDisplayStatus::Waiting;
                    }
                    AgentStatus::Complete => agent.status = AgentDisplayStatus::Complete,
                    AgentStatus::Error { message } => {
                        agent.status = AgentDisplayStatus::Error(message.clone());
                    }
                    AgentStatus::Cancelled => agent.status = AgentDisplayStatus::Cancelled,
                    AgentStatus::Idle => agent.status = AgentDisplayStatus::Idle,
                }
                agent.accepts_messages = state.accepts_messages;
            }
            if let Some(window) = engine.world().get::<ContextWindow>(agent.entity) {
                agent.context_tokens = (window.current_tokens, window.max_tokens);
            }
            // Populate parent info from ParentRef component
            if let Some(parent_ref) = engine
                .world()
                .get::<leviath_runtime::ParentRef>(agent.entity)
            {
                agent.parent_id = Some(parent_ref.parent_agent_id.clone());
                agent.depth = parent_ref.depth;
            }
        }
        self.update_display_indices();
    }

    /// Kill + delete all on-disk state for the selected agent.
    pub(super) fn delete_selected_agent(&mut self) {
        let (id, _pid, is_run_state) = match self.selected_agent() {
            Some(a) => (a.id.clone(), a.pid, a.is_run_state),
            None => return,
        };
        if !is_run_state {
            self.add_log("Can only delete background run-state agents".to_string());
            return;
        }
        // Kill the worker process first if it is still running
        #[cfg(unix)]
        if _pid > 0 {
            unsafe {
                libc::kill(_pid as libc::pid_t, libc::SIGTERM);
            }
        }
        // Remove run directory
        let run_dir = runstate::run_dir(&id);
        if let Err(e) = std::fs::remove_dir_all(&run_dir) {
            self.add_log(format!("Delete failed: {}", e));
        } else {
            self.add_log(format!("Deleted run {}", id));
        }
        // Remove saved context state if present
        if let Some(home) = dirs::home_dir() {
            let state_dir = home.join(".leviath").join("state").join(&id);
            let _ = std::fs::remove_dir_all(state_dir);
        }
        // Remove agent from self.agents using the raw index
        if let Some(raw_idx) = self.selected_agent_raw_idx() {
            self.agents.remove(raw_idx);
        }
        self.update_display_indices();
    }

    /// True if the currently selected stage tab is the one actively accepting input.
    /// Check if the selected agent is active and accepts mid-run messages.
    pub(super) fn selected_agent_accepts_messages(&self) -> bool {
        let agent = match self.selected_agent() {
            Some(a) => a,
            None => return false,
        };
        matches!(agent.status, AgentDisplayStatus::Active) && agent.accepts_messages
    }

    pub(super) fn selected_stage_can_respond(&self) -> bool {
        let agent = match self.selected_agent() {
            Some(a) => a,
            None => return false,
        };
        if matches!(agent.status, AgentDisplayStatus::Cancelled) {
            return false;
        }
        // Check if there's actually a prompt to respond to
        if agent.waiting_prompt.is_none() && agent.pending_request.is_none() {
            return false;
        }
        // Gate on the selected stage matching the stage currently requiring input.
        // Use pending_request.stage_name if available, otherwise fall back to current stage_index.
        let input_stage_idx = if let Some(req) = &agent.pending_request {
            if !req.stage_name.is_empty() {
                // Find stage by name
                agent
                    .stages
                    .iter()
                    .position(|s| s.name == req.stage_name)
                    .unwrap_or(agent.stage_index)
            } else {
                agent.stage_index
            }
        } else {
            agent.stage_index
        };
        self.selected_stage == input_stage_idx
    }

    /// Build a tree-ordered list of agent indices with tree connector prefixes.
    ///
    /// Returns Vec<(original_index, tree_prefix)> in depth-first tree order.
    #[allow(dead_code)]
    pub(super) fn build_tree_order(&self) -> Vec<(usize, String)> {
        let mut result = Vec::new();

        // Find root agents (no parent_id)
        let root_indices: Vec<usize> = self
            .agents
            .iter()
            .enumerate()
            .filter(|(_, a)| a.parent_id.is_none())
            .map(|(i, _)| i)
            .collect();

        for &idx in &root_indices {
            self.collect_tree_children(idx, "", &mut result, true);
        }

        result
    }

    #[allow(dead_code)]
    fn collect_tree_children(
        &self,
        idx: usize,
        prefix: &str,
        result: &mut Vec<(usize, String)>,
        is_root: bool,
    ) {
        let display_prefix = if is_root {
            String::new()
        } else {
            prefix.to_string()
        };
        result.push((idx, display_prefix));

        let agent_id = &self.agents[idx].id;
        let children: Vec<usize> = self
            .agents
            .iter()
            .enumerate()
            .filter(|(_, a)| a.parent_id.as_deref() == Some(agent_id))
            .map(|(i, _)| i)
            .collect();

        for (ci, &child_idx) in children.iter().enumerate() {
            let is_last = ci == children.len() - 1;
            let connector = if is_last { "└─ " } else { "├─ " };
            let child_prefix = if is_root {
                connector.to_string()
            } else {
                let base = prefix.replace("├─ ", "│  ").replace("└─ ", "   ");
                format!("{}{}", base, connector)
            };

            self.collect_tree_children(child_idx, &child_prefix, result, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_dashboard() -> Dashboard {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        Dashboard::new(cmd_tx)
    }

    fn make_test_agent(id: &str, status: AgentDisplayStatus) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "test-agent".to_string(),
            agent_path: "/path".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 1,
            status,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            context_tokens: (0, 0),
            iteration: 0,
            waiting_prompt: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            entity: bevy_ecs::prelude::Entity::from_raw(0),
            is_run_state: true,
            pid: 0,
            workdir: "/tmp".to_string(),
            task: "test task".to_string(),
            title: None,
            model: None,
            parent_id: None,
            depth: 0,
            started_at: 1000,
            active_until: None,
            waiting_secs: 0,
            graph_info: None,
            accepts_messages: true,
        }
    }

    #[test]
    fn dashboard_initial_state() {
        let dash = make_test_dashboard();
        assert!(dash.agents.is_empty());
        assert_eq!(dash.selected, 0);
        assert!(!dash.input_mode);
        assert!(!dash.detail_view);
        assert!(!dash.should_quit);
        assert!(!dash.confirm_delete);
        assert_eq!(dash.detail_scroll, 0);
        assert_eq!(dash.choice_selected, 0);
        assert_eq!(dash.selected_stage, 0);
        assert_eq!(dash.stage_content_mode, StageContentMode::Output);
        assert!(!dash.initial_sync_done);
        assert_eq!(dash.tick_count, 0);
        assert!(dash.toasts.is_empty());
        assert!(!dash.show_help);
        assert_eq!(dash.review_scroll, 0);
        assert!(!dash.search_mode);
        assert!(dash.search_query.is_empty());
        assert_eq!(dash.search_match_idx, 0);
        assert!(!dash.list_search_mode);
        assert!(dash.list_search_query.is_empty());
        assert!(dash.display_indices.is_empty());
    }

    #[test]
    fn selected_agent_empty() {
        let dash = make_test_dashboard();
        assert!(dash.selected_agent().is_none());
    }

    #[test]
    fn update_display_indices_empty() {
        let mut dash = make_test_dashboard();
        dash.update_display_indices();
        assert!(dash.display_indices.is_empty());
        assert_eq!(dash.selected, 0);
    }

    #[test]
    fn update_display_indices_with_agents() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Complete));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-3", AgentDisplayStatus::Waiting));
        dash.update_display_indices();

        // Active first, then Waiting, then Complete
        assert_eq!(dash.display_indices.len(), 3);
        assert_eq!(dash.agents[dash.display_indices[0]].id, "run-2"); // Active
        assert_eq!(dash.agents[dash.display_indices[1]].id, "run-3"); // Waiting
        assert_eq!(dash.agents[dash.display_indices[2]].id, "run-1"); // Complete
    }

    #[test]
    fn update_display_indices_filter() {
        let mut dash = make_test_dashboard();
        let mut a1 = make_test_agent("run-1", AgentDisplayStatus::Active);
        a1.blueprint_name = "coder".to_string();
        let mut a2 = make_test_agent("run-2", AgentDisplayStatus::Active);
        a2.blueprint_name = "reviewer".to_string();
        dash.agents.push(a1);
        dash.agents.push(a2);
        dash.list_search_query = "coder".to_string();
        dash.update_display_indices();

        assert_eq!(dash.display_indices.len(), 1);
        assert_eq!(dash.agents[dash.display_indices[0]].blueprint_name, "coder");
    }

    #[test]
    fn selected_agent_with_agents() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        assert!(dash.selected_agent().is_some());
        assert_eq!(dash.selected_agent().unwrap().id, "run-1");
    }

    #[test]
    fn selected_agent_mut_works() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        if let Some(agent) = dash.selected_agent_mut() {
            agent.tokens_in = 999;
        }
        assert_eq!(dash.agents[0].tokens_in, 999);
    }

    #[test]
    fn selected_agent_raw_idx() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        assert_eq!(dash.selected_agent_raw_idx(), Some(0));
    }

    #[test]
    fn tick_toasts_decrements() {
        let mut dash = make_test_dashboard();
        dash.toasts.push(Toast {
            message: "hello".to_string(),
            remaining_ticks: 2,
            level: ToastLevel::Info,
        });
        dash.tick_toasts();
        assert_eq!(dash.toasts.len(), 1);
        assert_eq!(dash.toasts[0].remaining_ticks, 1);
        dash.tick_toasts();
        assert!(dash.toasts.is_empty());
    }

    #[test]
    fn push_toast_limits_to_four() {
        let mut dash = make_test_dashboard();
        for i in 0..6 {
            dash.push_toast(format!("toast {}", i), ToastLevel::Info);
        }
        assert_eq!(dash.toasts.len(), 4);
    }

    #[test]
    fn selected_agent_accepts_messages_no_agents() {
        let dash = make_test_dashboard();
        assert!(!dash.selected_agent_accepts_messages());
    }

    #[test]
    fn selected_agent_accepts_messages_active() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.accepts_messages = true;
        dash.agents.push(agent);
        dash.update_display_indices();
        assert!(dash.selected_agent_accepts_messages());
    }

    #[test]
    fn selected_agent_accepts_messages_waiting() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.accepts_messages = true;
        dash.agents.push(agent);
        dash.update_display_indices();
        // Waiting agents don't accept messages (only Active do)
        assert!(!dash.selected_agent_accepts_messages());
    }

    #[test]
    fn selected_stage_can_respond_no_agents() {
        let dash = make_test_dashboard();
        assert!(!dash.selected_stage_can_respond());
    }

    #[test]
    fn selected_stage_can_respond_cancelled() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Cancelled);
        agent.waiting_prompt = Some("prompt".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        assert!(!dash.selected_stage_can_respond());
    }

    #[test]
    fn selected_stage_can_respond_no_prompt() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Waiting));
        dash.update_display_indices();
        assert!(!dash.selected_stage_can_respond());
    }

    #[test]
    fn build_tree_order_empty() {
        let dash = make_test_dashboard();
        assert!(dash.build_tree_order().is_empty());
    }

    #[test]
    fn build_tree_order_single_root() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("root", AgentDisplayStatus::Active));
        let tree = dash.build_tree_order();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].0, 0);
        assert!(tree[0].1.is_empty()); // root has no prefix
    }

    #[test]
    fn build_tree_order_parent_child() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("parent", AgentDisplayStatus::Active));
        let mut child = make_test_agent("child", AgentDisplayStatus::Active);
        child.parent_id = Some("parent".to_string());
        dash.agents.push(child);
        let tree = dash.build_tree_order();
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].0, 0); // parent
        assert_eq!(tree[1].0, 1); // child
        assert!(tree[1].1.contains("└─")); // last child connector
    }

    #[test]
    fn update_display_indices_preserves_selection() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.selected = 1;
        dash.table_state.select(Some(1));
        // Re-sort without changing agents — selection should be preserved
        dash.update_display_indices();
        assert_eq!(dash.selected, 1);
    }

    #[test]
    fn update_display_indices_resets_selection_when_previously_selected_agent_disappears() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.selected = 1;
        dash.table_state.select(Some(1));

        // The previously-selected agent ("run-2") is filtered out entirely,
        // so its id can't be found in the recomputed display_indices --
        // exercising the `else { self.selected = 0; }` reset branch, as
        // opposed to `update_display_indices_preserves_selection`'s
        // find-and-restore-position path above.
        dash.list_search_query = "run-1".to_string();
        dash.update_display_indices();
        assert_eq!(dash.selected, 0);
    }

    #[test]
    fn process_events_stage_changed() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::StageChanged {
            agent_id: "run-1".to_string(),
            stage: "implement".to_string(),
        })
        .unwrap();
        dash.process_events();
        assert_eq!(dash.agents[0].stage, "implement");
    }

    #[test]
    fn process_events_agent_done() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::AgentDone {
            agent_id: "run-1".to_string(),
        })
        .unwrap();
        dash.process_events();
        assert!(matches!(
            dash.agents[0].status,
            AgentDisplayStatus::Complete
        ));
    }

    #[test]
    fn process_events_error() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::Error {
            agent_id: "run-1".to_string(),
            error: "something broke".to_string(),
        })
        .unwrap();
        dash.process_events();
        assert!(matches!(
            dash.agents[0].status,
            AgentDisplayStatus::Error(_)
        ));
    }

    #[test]
    fn process_events_log() {
        let mut dash = make_test_dashboard();
        dash.log.clear(); // Clear seeded log entries
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::Log("test log message".to_string()))
            .unwrap();
        dash.process_events();
        assert!(!dash.log.is_empty());
        assert!(dash
            .log
            .iter()
            .any(|e| e.message.contains("test log message")));
    }

    #[test]
    fn process_events_needs_input() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::NeedsInput {
            agent_id: "run-1".to_string(),
            prompt: "What should I do?".to_string(),
        })
        .unwrap();
        dash.process_events();
        assert!(matches!(dash.agents[0].status, AgentDisplayStatus::Waiting));
        assert_eq!(
            dash.agents[0].waiting_prompt.as_deref(),
            Some("What should I do?")
        );
    }

    #[test]
    fn process_events_tool_called() {
        let mut dash = make_test_dashboard();
        dash.log.clear(); // Clear seeded log entries
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::ToolCalled {
            agent_id: "run-1".to_string(),
            tool: "bash".to_string(),
            args: r#"{"cmd": "ls"}"#.to_string(),
        })
        .unwrap();
        dash.process_events();
        assert!(dash.log.iter().any(|e| e.message.contains("bash")));
    }

    #[test]
    fn process_events_inference_complete() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::InferenceComplete {
            agent_id: "run-1".to_string(),
            content: "done".to_string(),
            tokens_used: 100,
            tokens_prompt: 50,
        })
        .unwrap();
        dash.process_events();
        assert_eq!(dash.agents[0].iteration, 1);
    }

    #[test]
    fn process_events_status_changed() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::StatusChanged {
            agent_id: "run-1".to_string(),
            status: AgentDisplayStatus::Complete,
        })
        .unwrap();
        dash.process_events();
        assert!(matches!(
            dash.agents[0].status,
            AgentDisplayStatus::Complete
        ));
    }

    #[test]
    fn selected_stage_can_respond_with_prompt_matching_stage() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("Review this".to_string());
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "req-1",
            "Review this",
            "main",
            true,
        ));
        agent.stage_index = 0;
        agent.stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.selected_stage = 0;
        assert!(dash.selected_stage_can_respond());
    }

    #[test]
    fn selected_stage_can_respond_wrong_stage_selected() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("Review this".to_string());
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "req-1",
            "Review this",
            "main",
            true,
        ));
        agent.stage_index = 0;
        agent.num_stages = 2;
        agent.stages = vec![
            crate::runstate::StageRecord::new("main".to_string(), 0),
            crate::runstate::StageRecord::new("code".to_string(), 1),
        ];
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.selected_stage = 1; // Wrong stage
        assert!(!dash.selected_stage_can_respond());
    }

    #[test]
    fn build_tree_order_multiple_children() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("parent", AgentDisplayStatus::Active));
        let mut child1 = make_test_agent("child1", AgentDisplayStatus::Active);
        child1.parent_id = Some("parent".to_string());
        let mut child2 = make_test_agent("child2", AgentDisplayStatus::Active);
        child2.parent_id = Some("parent".to_string());
        dash.agents.push(child1);
        dash.agents.push(child2);

        let tree = dash.build_tree_order();
        assert_eq!(tree.len(), 3);
        assert_eq!(tree[0].0, 0); // parent
        assert!(tree[0].1.is_empty()); // root has no prefix
                                       // First child
        assert_eq!(tree[1].0, 1);
        assert!(tree[1].1.contains("├─")); // not last child
                                           // Second child (last)
        assert_eq!(tree[2].0, 2);
        assert!(tree[2].1.contains("└─")); // last child
    }

    #[test]
    fn build_tree_order_grandchildren() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("root", AgentDisplayStatus::Active));
        let mut child = make_test_agent("child", AgentDisplayStatus::Active);
        child.parent_id = Some("root".to_string());
        dash.agents.push(child);
        let mut grandchild = make_test_agent("grandchild", AgentDisplayStatus::Active);
        grandchild.parent_id = Some("child".to_string());
        dash.agents.push(grandchild);

        let tree = dash.build_tree_order();
        assert_eq!(tree.len(), 3);
        assert_eq!(tree[0].0, 0); // root
        assert_eq!(tree[1].0, 1); // child
        assert_eq!(tree[2].0, 2); // grandchild
    }

    #[test]
    fn update_display_indices_filter_by_status() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.agents.push(make_test_agent(
            "run-2",
            AgentDisplayStatus::Error("err".to_string()),
        ));
        dash.list_search_query = "error".to_string();
        dash.update_display_indices();
        // Only the error agent should match (status contains "error")
        assert_eq!(dash.display_indices.len(), 1);
    }

    #[test]
    fn update_display_indices_filter_by_title() {
        let mut dash = make_test_dashboard();
        let mut a1 = make_test_agent("run-1", AgentDisplayStatus::Active);
        a1.title = Some("Deploy pipeline".to_string());
        let mut a2 = make_test_agent("run-2", AgentDisplayStatus::Active);
        a2.title = Some("Code review".to_string());
        dash.agents.push(a1);
        dash.agents.push(a2);
        dash.list_search_query = "deploy".to_string();
        dash.update_display_indices();
        assert_eq!(dash.display_indices.len(), 1);
    }

    #[test]
    fn update_display_indices_filter_by_task() {
        let mut dash = make_test_dashboard();
        let mut a1 = make_test_agent("run-1", AgentDisplayStatus::Active);
        a1.task = "Write unit tests".to_string();
        let mut a2 = make_test_agent("run-2", AgentDisplayStatus::Active);
        a2.task = "Fix bug in parser".to_string();
        dash.agents.push(a1);
        dash.agents.push(a2);
        dash.list_search_query = "unit test".to_string();
        dash.update_display_indices();
        assert_eq!(dash.display_indices.len(), 1);
    }

    #[test]
    fn selected_agent_accepts_messages_not_accepting() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.accepts_messages = false;
        dash.agents.push(agent);
        dash.update_display_indices();
        assert!(!dash.selected_agent_accepts_messages());
    }

    #[test]
    fn add_log_trims_to_200() {
        let mut dash = make_test_dashboard();
        dash.log.clear(); // Clear any seeded log entries
        for i in 0..250 {
            dash.add_log(format!("msg {}", i));
        }
        assert!(dash.log.len() <= 200);
    }

    #[test]
    fn selected_agent_raw_idx_empty() {
        let dash = make_test_dashboard();
        assert_eq!(dash.selected_agent_raw_idx(), None);
    }

    // ─── sync_agent_state_from_world ──────────────────────────────────────

    #[test]
    fn sync_agent_state_from_world_updates_in_process_agents() {
        let mut dash = make_test_dashboard();
        let mut engine = AgentEngine::new();

        // Spawn an entity with known state
        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "in-proc-1".to_string(),
                    current_stage: "analyze".to_string(),
                    iteration: 5,
                    status: AgentStatus::Active,
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: true,
                },
                ContextWindow::new(10000),
            ))
            .id();

        // Add an in-process agent with this entity
        let mut agent = make_test_agent("in-proc-1", AgentDisplayStatus::Idle);
        agent.is_run_state = false;
        agent.entity = entity;
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.sync_agent_state_from_world(&engine);

        assert_eq!(dash.agents[0].iteration, 5);
        assert_eq!(dash.agents[0].stage, "analyze");
        assert!(matches!(dash.agents[0].status, AgentDisplayStatus::Active));
        assert!(dash.agents[0].accepts_messages);
    }

    #[test]
    fn sync_agent_state_from_world_skips_run_state_agents() {
        let mut dash = make_test_dashboard();
        let engine = AgentEngine::new();

        let mut agent = make_test_agent("run-state-1", AgentDisplayStatus::Active);
        agent.is_run_state = true;
        agent.iteration = 99;
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.sync_agent_state_from_world(&engine);

        // Should not change because it's run-state
        assert_eq!(dash.agents[0].iteration, 99);
    }

    #[test]
    fn sync_agent_state_from_world_maps_all_statuses() {
        let mut dash = make_test_dashboard();
        let mut engine = AgentEngine::new();

        // Test each AgentStatus variant
        let statuses = [
            (AgentStatus::Active, AgentDisplayStatus::Active),
            (AgentStatus::Waiting, AgentDisplayStatus::Waiting),
            (AgentStatus::Complete, AgentDisplayStatus::Complete),
            (AgentStatus::Cancelled, AgentDisplayStatus::Cancelled),
            (AgentStatus::Idle, AgentDisplayStatus::Idle),
            (
                AgentStatus::Error {
                    message: "oops".to_string(),
                },
                AgentDisplayStatus::Error("oops".to_string()),
            ),
        ];

        for (i, (ecs_status, _expected_display)) in statuses.iter().enumerate() {
            let entity = engine
                .world_mut()
                .spawn(AgentState {
                    agent_id: format!("agent-{}", i),
                    current_stage: "main".to_string(),
                    iteration: 0,
                    status: ecs_status.clone(),
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: false,
                })
                .id();

            let mut agent = make_test_agent(&format!("agent-{}", i), AgentDisplayStatus::Idle);
            agent.is_run_state = false;
            agent.entity = entity;
            dash.agents.push(agent);
        }
        dash.update_display_indices();

        dash.sync_agent_state_from_world(&engine);

        // Verify each status was mapped correctly
        assert!(matches!(dash.agents[0].status, AgentDisplayStatus::Active));
        assert!(matches!(dash.agents[1].status, AgentDisplayStatus::Waiting));
        assert!(matches!(
            dash.agents[2].status,
            AgentDisplayStatus::Complete
        ));
        assert!(matches!(
            dash.agents[3].status,
            AgentDisplayStatus::Cancelled
        ));
        assert!(matches!(dash.agents[4].status, AgentDisplayStatus::Idle));
        assert!(matches!(
            dash.agents[5].status,
            AgentDisplayStatus::Error(_)
        ));
    }

    #[test]
    fn sync_agent_state_from_world_reads_context_tokens() {
        let mut dash = make_test_dashboard();
        let mut engine = AgentEngine::new();

        let mut window = ContextWindow::new(50000);
        window.current_tokens = 12345;
        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "ctx-agent".to_string(),
                    current_stage: "main".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: true,
                },
                window,
            ))
            .id();

        let mut agent = make_test_agent("ctx-agent", AgentDisplayStatus::Idle);
        agent.is_run_state = false;
        agent.entity = entity;
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.sync_agent_state_from_world(&engine);

        assert_eq!(dash.agents[0].context_tokens, (12345, 50000));
    }

    #[test]
    fn sync_agent_state_from_world_reads_parent_ref() {
        let mut dash = make_test_dashboard();
        let mut engine = AgentEngine::new();

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "child-agent".to_string(),
                    current_stage: "main".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: true,
                },
                leviath_runtime::ParentRef {
                    parent_entity: bevy_ecs::prelude::Entity::from_raw(0),
                    parent_agent_id: "parent-agent".to_string(),
                    depth: 2,
                },
            ))
            .id();

        let mut agent = make_test_agent("child-agent", AgentDisplayStatus::Idle);
        agent.is_run_state = false;
        agent.entity = entity;
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.sync_agent_state_from_world(&engine);

        assert_eq!(dash.agents[0].parent_id.as_deref(), Some("parent-agent"));
        assert_eq!(dash.agents[0].depth, 2);
    }

    // ─── build_tree_order with deeper nesting ─────────────────────────────

    #[test]
    fn build_tree_order_deep_nesting_connectors() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("root", AgentDisplayStatus::Active));
        let mut child1 = make_test_agent("child1", AgentDisplayStatus::Active);
        child1.parent_id = Some("root".to_string());
        dash.agents.push(child1);
        let mut child2 = make_test_agent("child2", AgentDisplayStatus::Active);
        child2.parent_id = Some("root".to_string());
        dash.agents.push(child2);
        let mut gc = make_test_agent("grandchild", AgentDisplayStatus::Active);
        gc.parent_id = Some("child1".to_string());
        dash.agents.push(gc);

        let tree = dash.build_tree_order();
        assert_eq!(tree.len(), 4);
        assert_eq!(tree[0].0, 0); // root
        assert!(tree[0].1.is_empty()); // root has no prefix
        assert_eq!(tree[1].0, 1); // child1
        assert!(tree[1].1.contains("├─")); // not last child
        assert_eq!(tree[2].0, 3); // grandchild (under child1)
                                  // grandchild should have deeper connector
        assert!(tree[2].1.len() > tree[1].1.len());
        assert_eq!(tree[3].0, 2); // child2
        assert!(tree[3].1.contains("└─")); // last child
    }

    // ─── build_tree_order with multiple roots ─────────────────────────────

    #[test]
    fn build_tree_order_multiple_roots() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("root1", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("root2", AgentDisplayStatus::Active));
        let mut child = make_test_agent("child-of-1", AgentDisplayStatus::Active);
        child.parent_id = Some("root1".to_string());
        dash.agents.push(child);

        let tree = dash.build_tree_order();
        assert_eq!(tree.len(), 3);
        // root1 and child-of-1 should be adjacent
        assert_eq!(tree[0].0, 0); // root1
        assert_eq!(tree[1].0, 2); // child-of-1
        assert_eq!(tree[2].0, 1); // root2
    }

    // ─── delete_selected_agent: non-run-state agent ───────────────────────

    #[test]
    fn delete_selected_agent_non_run_state_logs_error() {
        let mut dash = make_test_dashboard();
        dash.log.clear();
        let mut agent = make_test_agent("in-proc-1", AgentDisplayStatus::Active);
        agent.is_run_state = false;
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.delete_selected_agent();

        // Should not have removed the agent
        assert_eq!(dash.agents.len(), 1);
        assert!(dash
            .log
            .iter()
            .any(|e| e.message.contains("Can only delete")));
    }

    // ─── delete_selected_agent: no agent selected ─────────────────────────

    #[test]
    fn delete_selected_agent_empty_list_is_noop() {
        let mut dash = make_test_dashboard();
        dash.delete_selected_agent();
        // Should not panic, just no-op
    }

    // ─── update_display_indices: sort priority order ──────────────────────

    #[test]
    fn update_display_indices_sort_order_comprehensive() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("cancelled", AgentDisplayStatus::Cancelled));
        dash.agents
            .push(make_test_agent("idle", AgentDisplayStatus::Idle));
        dash.agents
            .push(make_test_agent("active", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("waiting", AgentDisplayStatus::Waiting));
        dash.agents.push(make_test_agent(
            "error",
            AgentDisplayStatus::Error("err".to_string()),
        ));
        dash.agents
            .push(make_test_agent("complete", AgentDisplayStatus::Complete));
        dash.update_display_indices();

        // Expected priority: Active(0) < Waiting(1) < Complete(3) < Error(4) < Idle(5) < Cancelled(6)
        let ids: Vec<&str> = dash
            .display_indices
            .iter()
            .map(|&i| dash.agents[i].id.as_str())
            .collect();
        assert_eq!(ids[0], "active");
        assert_eq!(ids[1], "waiting");
        assert_eq!(ids[2], "complete");
        assert_eq!(ids[3], "error");
        assert_eq!(ids[4], "idle");
        assert_eq!(ids[5], "cancelled");
    }

    // ─── selected_stage_can_respond: stage_name matching ──────────────────

    #[test]
    fn selected_stage_can_respond_matches_by_stage_name() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("Review this".to_string());
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "req-1",
            "Review this",
            "code",
            true,
        ));
        agent.stage_index = 0;
        agent.num_stages = 3;
        agent.stages = vec![
            crate::runstate::StageRecord::new("plan".to_string(), 0),
            crate::runstate::StageRecord::new("code".to_string(), 1),
            crate::runstate::StageRecord::new("review".to_string(), 2),
        ];
        dash.agents.push(agent);
        dash.update_display_indices();

        // stage_name is "code" which is at index 1
        dash.selected_stage = 1;
        assert!(dash.selected_stage_can_respond());

        // Wrong stage selected
        dash.selected_stage = 0;
        assert!(!dash.selected_stage_can_respond());
    }

    // ─── add_log: timestamp format and persistence ────────────────────────

    #[test]
    fn add_log_appends_entry() {
        let mut dash = make_test_dashboard();
        dash.log.clear();
        dash.add_log("hello from test".to_string());
        assert!(!dash.log.is_empty());
        assert!(dash.log.last().unwrap().message == "hello from test");
        // Timestamp should look like HH:MM:SS
        let ts = &dash.log.last().unwrap().timestamp;
        assert!(
            ts.contains(':'),
            "timestamp should contain ':', got '{}'",
            ts
        );
    }

    #[test]
    fn add_log_trims_when_over_200() {
        let mut dash = make_test_dashboard();
        dash.log.clear();
        // Fill to exactly 200
        for i in 0..200 {
            dash.log.push(crate::commands::dashboard::types::LogEntry {
                timestamp: "00:00:00".to_string(),
                message: format!("seed {}", i),
            });
        }
        // Adding one more should remove oldest
        dash.add_log("newest".to_string());
        assert!(dash.log.len() <= 200);
        assert_eq!(dash.log.last().unwrap().message, "newest");
    }

    // ─── load_log_seed: exercises the parsing code ───────────────────────

    #[test]
    fn load_log_seed_returns_vec() {
        // We cannot control the on-disk file in tests, but we can confirm
        // the method returns a Vec (even if empty) without panicking.
        let entries = Dashboard::load_log_seed();
        // Just assert it's a valid Vec
        let _ = entries.len();
    }

    // ─── parse_log_lines: the pure parsing core of load_log_seed ──────────

    #[test]
    fn parse_log_lines_normal_line_uses_time_portion() {
        let entries = Dashboard::parse_log_lines("2026-01-01 12:00:00 something happened");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].timestamp, "12:00:00");
        assert_eq!(entries[0].message, "something happened");
    }

    #[test]
    fn parse_log_lines_missing_time_token_falls_back_to_date() {
        // A double space between the first token and the message leaves the
        // "time" slot empty, exercising the `if time.is_empty() { date }`
        // fallback branch.
        let entries = Dashboard::parse_log_lines("2026-01-01  something happened");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].timestamp, "2026-01-01");
        assert_eq!(entries[0].message, "something happened");
    }

    #[test]
    fn parse_log_lines_skips_lines_with_empty_message() {
        let entries = Dashboard::parse_log_lines("2026-01-01 12:00:00 \n");
        assert!(entries.is_empty());
    }

    // ─── push_toast: level variants ──────────────────────────────────────

    #[test]
    fn push_toast_warning_level() {
        let mut dash = make_test_dashboard();
        dash.push_toast("warning!", ToastLevel::Warning);
        assert_eq!(dash.toasts.len(), 1);
        assert!(matches!(dash.toasts[0].level, ToastLevel::Warning));
    }

    #[test]
    fn push_toast_error_level() {
        let mut dash = make_test_dashboard();
        dash.push_toast("error!", ToastLevel::Error);
        assert_eq!(dash.toasts.len(), 1);
        assert!(matches!(dash.toasts[0].level, ToastLevel::Error));
    }

    // ─── tick_toasts: already at zero stays zero ──────────────────────────

    #[test]
    fn tick_toasts_already_expired_removed() {
        let mut dash = make_test_dashboard();
        dash.toasts.push(Toast {
            message: "gone".to_string(),
            remaining_ticks: 1,
            level: ToastLevel::Info,
        });
        dash.tick_toasts(); // goes to 0 and is removed
        assert!(dash.toasts.is_empty());
    }

    // ─── update_display_indices: CompleteInteractive priority ────────────

    #[test]
    fn update_display_indices_complete_interactive_priority() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("active", AgentDisplayStatus::Active));
        dash.agents.push(make_test_agent(
            "ci",
            AgentDisplayStatus::CompleteInteractive,
        ));
        dash.update_display_indices();
        // Active(0) should come before CompleteInteractive(2)
        let ids: Vec<&str> = dash
            .display_indices
            .iter()
            .map(|&i| dash.agents[i].id.as_str())
            .collect();
        assert_eq!(ids[0], "active");
        assert_eq!(ids[1], "ci");
    }

    // ─── selected_stage_can_respond: only pending_request path ───────────

    #[test]
    fn selected_stage_can_respond_with_only_pending_request_no_waiting_prompt() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        // Only pending_request, no waiting_prompt
        agent.waiting_prompt = None;
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "req-1",
            "Prompt text",
            "main",
            true,
        ));
        agent.stage_index = 0;
        agent.stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.selected_stage = 0;
        // pending_request is Some so should be able to respond
        assert!(dash.selected_stage_can_respond());
    }

    // ─── delete_selected_agent: run state agent removal ──────────────────

    #[test]
    fn delete_selected_agent_removes_run_state_agent() {
        let mut dash = make_test_dashboard();
        dash.log.clear();
        // Use a real temp dir that exists to avoid "delete failed" log
        let tmp_id = format!("test-run-{}", std::process::id());
        let run_dir = crate::runstate::run_dir(&tmp_id);
        let _ = std::fs::create_dir_all(&run_dir);

        let mut agent = make_test_agent(&tmp_id, AgentDisplayStatus::Complete);
        agent.is_run_state = true;
        agent.pid = 0; // no actual process to kill
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.delete_selected_agent();

        // Agent should have been removed from the list
        assert!(
            dash.agents.is_empty() || dash.agents[0].id != tmp_id,
            "agent should have been removed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn delete_selected_agent_sends_sigterm_to_real_pid() {
        // Spawns a real, throwaway child process we fully own (so sending it
        // SIGTERM is safe, unlike an arbitrary PID) to exercise the
        // `#[cfg(unix)] if _pid > 0 { libc::kill(...) }` branch, which every
        // other `delete_selected_agent` test avoids via `pid = 0`.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("failed to spawn a throwaway child process");
        let child_pid = child.id();

        let mut dash = make_test_dashboard();
        dash.log.clear();
        let tmp_id = format!("test-run-kill-{}", std::process::id());
        let run_dir = crate::runstate::run_dir(&tmp_id);
        let _ = std::fs::create_dir_all(&run_dir);

        let mut agent = make_test_agent(&tmp_id, AgentDisplayStatus::Active);
        agent.is_run_state = true;
        agent.pid = child_pid;
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.delete_selected_agent();

        let status = child
            .wait()
            .expect("failed to wait on the throwaway child process");
        assert!(
            !status.success(),
            "expected the child to be terminated by SIGTERM, not exit successfully"
        );
    }

    #[test]
    fn delete_selected_agent_missing_run_dir_logs_delete_failed() {
        // Unlike `delete_selected_agent_removes_run_state_agent` (which
        // pre-creates the run dir so removal succeeds), this never creates
        // it -- `std::fs::remove_dir_all` on a nonexistent path returns
        // `Err`, exercising the "Delete failed" log branch.
        let mut dash = make_test_dashboard();
        dash.log.clear();
        let tmp_id = format!("test-run-missing-{}", std::process::id());

        let mut agent = make_test_agent(&tmp_id, AgentDisplayStatus::Complete);
        agent.is_run_state = true;
        agent.pid = 0;
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.delete_selected_agent();

        assert!(
            dash.log.iter().any(|l| l.message.contains("Delete failed")),
            "expected a 'Delete failed' log entry, got: {:?}",
            dash.log
        );
    }

    // ─── selected_stage_can_respond: empty stage_name falls back ──────────

    #[test]
    fn selected_stage_can_respond_empty_stage_name_uses_stage_index() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("prompt".to_string());
        agent.pending_request = Some(crate::interaction::InteractionRequest {
            id: "req-1".to_string(),
            kind: crate::interaction::InteractionKind::FreeText,
            prompt: "prompt".to_string(),
            options: vec![],
            tool_name: None,
            tool_arguments: None,
            required: true,
            stage_name: String::new(), // empty
            body: None,
            body_format: crate::interaction::BodyFormat::Plain,
        });
        agent.stage_index = 2;
        agent.num_stages = 3;
        dash.agents.push(agent);
        dash.update_display_indices();

        // Empty stage_name -> uses agent.stage_index which is 2
        dash.selected_stage = 2;
        assert!(dash.selected_stage_can_respond());
        dash.selected_stage = 0;
        assert!(!dash.selected_stage_can_respond());
    }

    // ─── sync_from_run_state ────────────────────────────────────────────────
    //
    // Uses real on-disk run directories (via runstate::create_run), like
    // runstate.rs's own `list_runs_returns_sorted` test does — unique run_ids
    // + inclusion checks (not exact-list assertions) so these coexist safely
    // with any other real runs on disk and with concurrently-running tests.

    fn make_run_meta(run_id: &str, status: RunStatus) -> runstate::RunMeta {
        let mut meta = runstate::RunMeta::new(
            run_id.to_string(),
            // Use the (unique-per-test) run_id as the agent name too, so toast
            // messages ("Agent '<name>' ...") can be unambiguously matched even
            // when tests run concurrently against the shared real runs dir.
            run_id.to_string(),
            "/nonexistent/agent/path".to_string(),
            "test task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        meta.status = status;
        meta
    }

    fn cleanup_run(run_id: &str) {
        let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
    }

    #[test]
    fn sync_from_run_state_new_agent_active() {
        let run_id = "test-sync-new-active";
        cleanup_run(run_id);
        let meta = make_run_meta(run_id, RunStatus::Running);
        runstate::create_run(&meta).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();

        let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
        assert!(matches!(agent.status, AgentDisplayStatus::Active));
        assert!(agent.is_run_state);
        assert!(dash.initial_sync_done);
        // No toasts on the very first sync (startup), even though this is a "new" agent.
        assert!(dash.toasts.is_empty());

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_new_agent_starting_maps_to_active() {
        let run_id = "test-sync-new-starting";
        cleanup_run(run_id);
        let meta = make_run_meta(run_id, RunStatus::Starting);
        runstate::create_run(&meta).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();

        let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
        assert!(matches!(agent.status, AgentDisplayStatus::Active));

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_new_agent_error_status() {
        let run_id = "test-sync-new-error";
        cleanup_run(run_id);
        let mut meta = make_run_meta(run_id, RunStatus::Error);
        meta.error = Some("boom".to_string());
        runstate::create_run(&meta).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();

        let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
        match &agent.status {
            AgentDisplayStatus::Error(msg) => assert_eq!(msg, "boom"),
            other => panic!("expected Error status, got {:?}", other),
        }

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_new_agent_cancelled_status() {
        let run_id = "test-sync-new-cancelled";
        cleanup_run(run_id);
        let meta = make_run_meta(run_id, RunStatus::Cancelled);
        runstate::create_run(&meta).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();

        let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
        assert!(matches!(agent.status, AgentDisplayStatus::Cancelled));

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_new_agent_waiting_input_reads_pending_request() {
        let run_id = "test-sync-new-waiting";
        cleanup_run(run_id);
        let meta = make_run_meta(run_id, RunStatus::WaitingInput);
        runstate::create_run(&meta).unwrap();
        let req =
            crate::interaction::InteractionRequest::free_text("req1", "What next?", "main", true);
        crate::interaction::write_request(run_id, &req).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();

        let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
        assert!(matches!(agent.status, AgentDisplayStatus::Waiting));
        assert_eq!(agent.waiting_prompt.as_deref(), Some("What next?"));
        assert!(agent.pending_request.is_some());
        assert!(agent.active_until.is_some());

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_new_agent_complete_interactive() {
        let run_id = "test-sync-new-complete-interactive";
        cleanup_run(run_id);
        let meta = make_run_meta(run_id, RunStatus::CompleteInteractive);
        runstate::create_run(&meta).unwrap();
        let req = crate::interaction::InteractionRequest::free_text(
            "req1",
            "Any feedback?",
            "review",
            false,
        );
        crate::interaction::write_request(run_id, &req).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();

        let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
        assert!(matches!(
            agent.status,
            AgentDisplayStatus::CompleteInteractive
        ));
        assert!(agent.waiting_prompt.is_some());
        assert!(agent.active_until.is_some());

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_new_agent_toasts_after_initial_sync_waiting() {
        let run_id = "test-sync-new-toast-waiting";
        cleanup_run(run_id);

        let mut dash = make_test_dashboard();
        dash.initial_sync_done = true; // simulate: not the app's first sync

        let meta = make_run_meta(run_id, RunStatus::WaitingInput);
        runstate::create_run(&meta).unwrap();
        let req = crate::interaction::InteractionRequest::confirm("req1", "Proceed?", "main");
        crate::interaction::write_request(run_id, &req).unwrap();

        dash.sync_from_run_state();

        assert!(dash
            .toasts
            .iter()
            .any(|t| t.message.contains("needs input")));

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_new_agent_toasts_after_initial_sync_complete() {
        let run_id = "test-sync-new-toast-complete";
        cleanup_run(run_id);

        let mut dash = make_test_dashboard();
        dash.initial_sync_done = true;

        let meta = make_run_meta(run_id, RunStatus::Complete);
        runstate::create_run(&meta).unwrap();

        dash.sync_from_run_state();

        assert!(dash.toasts.iter().any(|t| t.message.contains("completed")));

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_existing_agent_active_to_error_toasts_with_message() {
        let run_id = "test-sync-existing-to-error";
        cleanup_run(run_id);
        let meta = make_run_meta(run_id, RunStatus::Running);
        runstate::create_run(&meta).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state(); // first sync: creates the agent, Active

        let mut meta2 = make_run_meta(run_id, RunStatus::Error);
        meta2.error = Some("disk full".to_string());
        runstate::write_meta(&meta2).unwrap();
        dash.sync_from_run_state(); // second sync: transitions to Error

        let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
        assert!(matches!(agent.status, AgentDisplayStatus::Error(_)));
        assert!(dash
            .toasts
            .iter()
            .any(|t| t.message.contains("failed") && t.message.contains("disk full")));

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_existing_agent_active_to_error_empty_message() {
        let run_id = "test-sync-existing-to-error-empty";
        cleanup_run(run_id);
        let meta = make_run_meta(run_id, RunStatus::Running);
        runstate::create_run(&meta).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();

        let meta2 = make_run_meta(run_id, RunStatus::Error); // error left None
        runstate::write_meta(&meta2).unwrap();
        dash.sync_from_run_state();

        let failed_toast = dash
            .toasts
            .iter()
            .find(|t| t.message.contains("failed"))
            .unwrap();
        // No ": <preview>" suffix when the error message is empty.
        assert!(!failed_toast.message.contains(":"));

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_existing_agent_active_to_complete_toasts() {
        let run_id = "test-sync-existing-to-complete";
        cleanup_run(run_id);
        let meta = make_run_meta(run_id, RunStatus::Running);
        runstate::create_run(&meta).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();

        let meta2 = make_run_meta(run_id, RunStatus::Complete);
        runstate::write_meta(&meta2).unwrap();
        dash.sync_from_run_state();

        let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
        assert!(matches!(agent.status, AgentDisplayStatus::Complete));
        assert!(dash.toasts.iter().any(|t| t.message.contains("completed")));

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_existing_agent_active_to_complete_interactive_toasts() {
        let run_id = "test-sync-existing-to-complete-interactive";
        cleanup_run(run_id);
        let meta = make_run_meta(run_id, RunStatus::Running);
        runstate::create_run(&meta).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();

        let meta2 = make_run_meta(run_id, RunStatus::CompleteInteractive);
        runstate::write_meta(&meta2).unwrap();
        dash.sync_from_run_state();

        assert!(dash.toasts.iter().any(|t| t.message.contains("completed")));

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_existing_agent_stays_active_no_toast() {
        let run_id = "test-sync-existing-stays-active";
        cleanup_run(run_id);
        let meta = make_run_meta(run_id, RunStatus::Running);
        runstate::create_run(&meta).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();
        dash.sync_from_run_state(); // still Running -> Active, no transition

        // Scoped to this test's (uniquely-named) agent rather than
        // `dash.toasts.is_empty()` — the dashboard also picks up any other
        // real/concurrently-running-test runs on disk via list_runs(), whose
        // own transitions may toast independently of this one.
        assert!(!dash.toasts.iter().any(|t| t.message.contains(run_id)));

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_existing_agent_enters_waiting_toasts_and_freezes_timer() {
        let run_id = "test-sync-existing-enters-waiting";
        cleanup_run(run_id);
        let meta = make_run_meta(run_id, RunStatus::Running);
        runstate::create_run(&meta).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();
        assert!(dash
            .agents
            .iter()
            .find(|a| a.id == run_id)
            .unwrap()
            .active_until
            .is_none());

        let mut meta2 = make_run_meta(run_id, RunStatus::WaitingInput);
        meta2.updated_at = meta.started_at + 42;
        runstate::write_meta(&meta2).unwrap();
        let req = crate::interaction::InteractionRequest::free_text("req1", "Q?", "main", true);
        crate::interaction::write_request(run_id, &req).unwrap();
        dash.sync_from_run_state();

        let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
        assert!(matches!(agent.status, AgentDisplayStatus::Waiting));
        assert_eq!(agent.active_until, Some(meta2.updated_at));
        assert_eq!(agent.waiting_prompt.as_deref(), Some("Q?"));
        assert!(dash
            .toasts
            .iter()
            .any(|t| t.message.contains("needs input")));

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_existing_agent_leaves_waiting_accumulates_waiting_secs() {
        let run_id = "test-sync-existing-leaves-waiting";
        cleanup_run(run_id);
        let mut meta = make_run_meta(run_id, RunStatus::WaitingInput);
        meta.updated_at = meta.started_at + 10;
        runstate::create_run(&meta).unwrap();
        let req = crate::interaction::InteractionRequest::free_text("req1", "Q?", "main", true);
        crate::interaction::write_request(run_id, &req).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();
        let entered_wait_at = dash
            .agents
            .iter()
            .find(|a| a.id == run_id)
            .unwrap()
            .active_until
            .unwrap();

        // Now the run resumes (Running) — clear the interaction and re-sync.
        crate::interaction::clear_interaction(run_id);
        let mut meta2 = make_run_meta(run_id, RunStatus::Running);
        meta2.updated_at = entered_wait_at + 25;
        runstate::write_meta(&meta2).unwrap();
        dash.sync_from_run_state();

        let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
        assert!(agent.active_until.is_none());
        assert_eq!(agent.waiting_secs, 25);
        assert!(agent.waiting_prompt.is_none());
        assert!(agent.pending_request.is_none());
        assert!(agent.last_answered_request_id.is_none());

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_already_answered_request_not_reapplied() {
        let run_id = "test-sync-already-answered";
        cleanup_run(run_id);
        let meta = make_run_meta(run_id, RunStatus::Running);
        runstate::create_run(&meta).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();
        // Simulate having just answered a request with this id.
        {
            let agent = dash.agents.iter_mut().find(|a| a.id == run_id).unwrap();
            agent.last_answered_request_id = Some("req-answered".to_string());
        }

        let meta2 = make_run_meta(run_id, RunStatus::WaitingInput);
        runstate::write_meta(&meta2).unwrap();
        let req = crate::interaction::InteractionRequest::free_text(
            "req-answered",
            "Already answered?",
            "main",
            true,
        );
        crate::interaction::write_request(run_id, &req).unwrap();
        dash.sync_from_run_state();

        // Should NOT re-populate waiting_prompt/pending_request for a
        // request id that was already answered.
        let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
        assert!(agent.waiting_prompt.is_none());
        assert!(agent.pending_request.is_none());

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_no_reptoast_when_already_waiting() {
        let run_id = "test-sync-no-retoast";
        cleanup_run(run_id);
        let meta = make_run_meta(run_id, RunStatus::WaitingInput);
        runstate::create_run(&meta).unwrap();
        let req = crate::interaction::InteractionRequest::free_text("req1", "Q1?", "main", true);
        crate::interaction::write_request(run_id, &req).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state(); // first sync creates the agent, already Waiting -> no toast (new agent, initial sync)
        dash.toasts.clear();

        // Still waiting, same kind of request — re-sync must not toast again.
        dash.sync_from_run_state();
        assert!(!dash.toasts.iter().any(|t| t.message.contains(run_id)));

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_updates_stage_and_token_fields() {
        let run_id = "test-sync-updates-fields";
        cleanup_run(run_id);
        let meta = make_run_meta(run_id, RunStatus::Running);
        runstate::create_run(&meta).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();

        let mut meta2 = make_run_meta(run_id, RunStatus::Running);
        meta2.current_stage = "implement".to_string();
        meta2.stage_index = 1;
        meta2.num_stages = 3;
        meta2.iteration = 5;
        meta2.prompt_tokens = 100;
        meta2.completion_tokens = 50;
        meta2.cached_tokens = 10;
        meta2.title = Some("My Title".to_string());
        meta2.pid = 4242;
        runstate::write_meta(&meta2).unwrap();
        dash.sync_from_run_state();

        let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
        assert_eq!(agent.stage, "implement");
        assert_eq!(agent.stage_index, 1);
        assert_eq!(agent.num_stages, 3);
        assert_eq!(agent.iteration, 5);
        assert_eq!(agent.tokens_in, 100);
        assert_eq!(agent.tokens_out, 50);
        assert_eq!(agent.cached_tokens, 10);
        assert_eq!(agent.title.as_deref(), Some("My Title"));
        assert_eq!(agent.pid, 4242);

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_waiting_with_no_pending_request_leaves_agent_unchanged() {
        // WaitingInput but no pending.json on disk (e.g. race/cleanup) — the
        // `if waiting_prompt.is_some()` branch is skipped entirely.
        let run_id = "test-sync-waiting-no-pending";
        cleanup_run(run_id);
        let meta = make_run_meta(run_id, RunStatus::Running);
        runstate::create_run(&meta).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();

        let meta2 = make_run_meta(run_id, RunStatus::WaitingInput);
        runstate::write_meta(&meta2).unwrap();
        // No pending.json written.
        dash.sync_from_run_state();

        let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
        assert!(matches!(agent.status, AgentDisplayStatus::Waiting));
        assert!(agent.waiting_prompt.is_none());

        cleanup_run(run_id);
    }

    #[test]
    fn sync_from_run_state_updates_workdir_and_display_indices() {
        let run_id = "test-sync-workdir";
        cleanup_run(run_id);
        let mut meta = make_run_meta(run_id, RunStatus::Running);
        meta.workdir = "/first/workdir".to_string();
        runstate::create_run(&meta).unwrap();

        let mut dash = make_test_dashboard();
        dash.sync_from_run_state();

        let mut meta2 = make_run_meta(run_id, RunStatus::Running);
        meta2.workdir = "/second/workdir".to_string();
        runstate::write_meta(&meta2).unwrap();
        dash.sync_from_run_state();

        let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
        assert_eq!(agent.workdir, "/second/workdir");
        assert!(!dash.display_indices.is_empty());

        cleanup_run(run_id);
    }
}
