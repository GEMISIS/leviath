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
                    .unwrap_or_else(|| truncate(&agent.blueprint_name, 20));
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
                                    .unwrap_or_else(|| truncate(&agent.blueprint_name, 20));
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
                        let name = run
                            .title
                            .clone()
                            .unwrap_or_else(|| truncate(&run.agent_name, 20));
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
                        let name = run
                            .title
                            .clone()
                            .unwrap_or_else(|| truncate(&run.agent_name, 20));
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
}
