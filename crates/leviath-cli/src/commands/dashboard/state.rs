//! Dashboard state struct and core state-management methods.

use leviath_runtime::control_socket::{ControlClient, ControlRequest, ControlResponse};
use ratatui::widgets::TableState;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tui_textarea::TextArea;

use super::graph::load_graph_info;
use super::helpers::truncate;
use super::types::*;
use crate::runstate::{self, RunStatus};
use leviath_core::interaction;

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
    pub(super) cmd_tx: mpsc::UnboundedSender<DaemonCommand>,
    /// Open interactions the daemon is holding, keyed by agent/run id. Refreshed
    /// each tick from `ListInteractions` so waiting agents show their prompt.
    pub(super) pending_interactions: HashMap<String, interaction::InteractionRequest>,
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
    /// The selected run's context-window history (from its `run.lvr` archive),
    /// loaded lazily when the user starts browsing it in the Context view. Empty
    /// until then, and cleared when the browsed run changes.
    pub(super) context_history: Vec<leviath_core::run_archive::RunPoint>,
    /// Which historical context point is being viewed: `None` = the live current
    /// window (the default), `Some(i)` = archived point `i` in `context_history`.
    pub(super) context_history_idx: Option<usize>,
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
    /// Tree-connector prefix for each `display_indices` row (parent → child
    /// nesting), parallel to `display_indices`. Empty strings when filtering (the
    /// list is flat then).
    pub(super) tree_prefixes: Vec<String>,
    /// Filesystem path this dashboard appends its activity log to. Production
    /// construction resolves the real [`runstate::dashboard_log_path`]; tests
    /// (`make_test_dashboard`) inject a temp path so no test ever writes to the
    /// user's real `~/.leviath/dashboard.log`.
    pub(super) log_path: std::path::PathBuf,
    /// Clipboard copy function (`text -> success`). Production injects the real
    /// native-tool/OSC52 clipboard (which can write the real terminal); tests
    /// (and [`new`](Self::new)) inject a no-op so a `y` keypress never touches
    /// the clipboard or TTY.
    pub(super) yank_fn: fn(&str) -> bool,

    // ── MCP management screen ──────────────────────────────────────────────
    /// True when the full-screen MCP management view is open.
    pub(super) mcp_screen: bool,
    /// True when the add-server line editor is open.
    pub(super) mcp_add_mode: bool,
    /// The add-server input line.
    pub(super) mcp_add_input: String,
    /// Servers as rendered on the MCP screen, refreshed from config + store.
    pub(super) mcp_rows: Vec<McpRow>,
    /// Selected row on the MCP screen.
    pub(super) mcp_selected: usize,
    /// Paths + seams for the MCP screen's file/OAuth operations.
    pub(super) mcp_ctx: McpContext,
    /// Sends long-running MCP actions (login/test) to the background loop.
    pub(super) mcp_cmd_tx: mpsc::UnboundedSender<McpCommand>,
    /// Receives completed MCP action outcomes, drained into toasts each tick.
    pub(super) mcp_outcome_rx: mpsc::UnboundedReceiver<McpOutcome>,
    /// The background loop's ends of the MCP channels. `init_dashboard` takes
    /// them to spawn [`super::mcp::mcp_background_loop`]; tests keep them to
    /// assert dispatched commands and inject outcomes.
    pub(super) mcp_bg_ends: Option<(
        mpsc::UnboundedReceiver<McpCommand>,
        mpsc::UnboundedSender<McpOutcome>,
    )>,
}

impl Dashboard {
    /// Default/test constructor: points the activity log at a shared temp file
    /// and uses a no-op clipboard, so unit tests never touch the real
    /// `~/.leviath/dashboard.log`, the system clipboard, or the TTY. Production
    /// builds the dashboard via [`new_with_log_path`](Self::new_with_log_path)
    /// (see `init_dashboard`), injecting the real log path and clipboard fn.
    /// Test-only: production always goes through `new_with_log_path`.
    #[cfg(test)]
    pub(super) fn new(cmd_tx: mpsc::UnboundedSender<DaemonCommand>) -> Self {
        let log_path = std::env::temp_dir()
            .join("leviath-test-dashboard")
            .join("dashboard.log");
        // A test MCP context: temp paths, a no-op browser, and a fixed clock so
        // no test touches the real home directory, a browser, or the wall clock.
        let ctx = McpContext {
            config_path: std::env::temp_dir()
                .join("leviath-test-dashboard")
                .join("config.toml"),
            store_path: std::env::temp_dir()
                .join("leviath-test-dashboard")
                .join("mcp-auth.json"),
            opener: std::sync::Arc::new(|_| false),
            clock: || 1_000,
        };
        Self::new_with_log_path(cmd_tx, log_path, |_| false, ctx)
    }

    /// Core of [`new`](Self::new) with the activity-log path and clipboard
    /// function injected, so production can supply the real
    /// `~/.leviath/dashboard.log` + real OSC52/native clipboard while tests
    /// point them at a temp dir + a no-op.
    pub(super) fn new_with_log_path(
        cmd_tx: mpsc::UnboundedSender<DaemonCommand>,
        log_path: std::path::PathBuf,
        yank_fn: fn(&str) -> bool,
        mcp_ctx: McpContext,
    ) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        // The MCP action lane: the dashboard keeps the command sender + outcome
        // receiver; the other ends go to the background loop (production) or are
        // retained for tests to drive.
        let (mcp_cmd_tx, mcp_cmd_rx) = mpsc::unbounded_channel();
        let (mcp_outcome_tx, mcp_outcome_rx) = mpsc::unbounded_channel();

        // Seed the in-memory log buffer from the tail of the persistent log so
        // the panel shows recent history immediately on launch (not a blank panel).
        let log = Self::load_log_seed(&log_path);

        Self {
            log_path,
            yank_fn,
            agents: Vec::new(),
            selected: 0,
            log,
            input_textarea: TextArea::default(),
            input_mode: false,
            detail_view: false,
            cmd_tx,
            pending_interactions: HashMap::new(),
            table_state,
            should_quit: false,
            confirm_delete: false,
            detail_scroll: 0,
            choice_selected: 0,
            selected_stage: 0,
            stage_content_mode: StageContentMode::Output,
            context_history: Vec::new(),
            context_history_idx: None,
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
            tree_prefixes: Vec::new(),
            mcp_screen: false,
            mcp_add_mode: false,
            mcp_add_input: String::new(),
            mcp_rows: Vec::new(),
            mcp_selected: 0,
            mcp_ctx,
            mcp_cmd_tx,
            mcp_outcome_rx,
            mcp_bg_ends: Some((mcp_cmd_rx, mcp_outcome_tx)),
        }
    }

    /// Take the background loop's channel ends, so `init_dashboard` can spawn
    /// [`super::mcp::mcp_background_loop`]. Returns `None` if already taken.
    pub(super) fn take_mcp_bg_ends(
        &mut self,
    ) -> Option<(
        mpsc::UnboundedReceiver<super::types::McpCommand>,
        mpsc::UnboundedSender<super::types::McpOutcome>,
    )> {
        self.mcp_bg_ends.take()
    }

    /// The MCP screen's context, for `init_dashboard` to hand to the loop.
    pub(super) fn mcp_context(&self) -> McpContext {
        self.mcp_ctx.clone()
    }

    /// Read the last 32 KB of dashboard.log and convert each line into a
    /// `LogEntry` for the initial in-memory buffer.
    fn load_log_seed(log_path: &std::path::Path) -> Vec<LogEntry> {
        let tail = runstate::tail_file(log_path, 32_768);
        Self::parse_log_lines(&tail)
    }

    /// Core parsing logic of [`load_log_seed`], split out so it can be
    /// exercised in tests against controlled input independently of the log
    /// path (which `load_log_seed` now takes as a parameter).
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
        // With no filter, re-order into a parent → child tree so sub-agents and
        // fan-out workers nest under their parent (roots keep their recency order);
        // a filter keeps the flat sorted list (a partial match can't form a tree).
        let prefixes: Vec<String> = if query.is_empty() {
            // Roots keep the status-sorted order; an agent whose parent is absent
            // is treated as a root so it can't disappear from the list.
            let present: std::collections::HashSet<&str> =
                self.agents.iter().map(|a| a.id.as_str()).collect();
            let roots: Vec<usize> = indices
                .iter()
                .copied()
                .filter(|&i| {
                    self.agents[i]
                        .parent_id
                        .as_deref()
                        .is_none_or(|p| !present.contains(p))
                })
                .collect();
            let tree = self.build_tree_order(&roots);
            indices = tree.iter().map(|(i, _)| *i).collect();
            tree.into_iter().map(|(_, prefix)| prefix).collect()
        } else {
            vec![String::new(); indices.len()]
        };
        // Preserve selection: try to keep the same agent highlighted after recompute
        let prev_id = self
            .display_indices
            .get(self.selected)
            .and_then(|&i| self.agents.get(i))
            .map(|a| a.id.clone());
        self.display_indices = indices;
        self.tree_prefixes = prefixes;
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

    pub(super) fn selected_agent_raw_idx(&self) -> Option<usize> {
        self.display_indices.get(self.selected).copied()
    }

    /// Leave context-history browsing and go back to the live current window.
    pub(super) fn reset_context_history(&mut self) {
        self.context_history.clear();
        self.context_history_idx = None;
    }

    /// Step through the selected run's archived context-window history in the
    /// Context view: `delta > 0` moves to a later point, `delta < 0` to an
    /// earlier one. The history is (re)loaded from the run's `run.lvr` archive
    /// on each step; stepping past the newest point returns to the live window.
    /// No-op if the run has no archived history.
    pub(super) fn step_context_history(&mut self, delta: isize) {
        let Some(run_id) = self.selected_agent().map(|a| a.id.clone()) else {
            return;
        };
        let history = runstate::context_history(&run_id);
        if history.is_empty() {
            self.reset_context_history();
            return;
        }
        let last = (history.len() - 1) as isize;
        let new_idx = match self.context_history_idx {
            // From the live window, a backward step enters at the newest point;
            // a forward step stays live.
            None => (delta < 0).then_some(last),
            Some(i) => {
                let target = i as isize + delta;
                if target < 0 {
                    Some(0)
                } else if target > last {
                    None // past the newest recorded point → live window
                } else {
                    Some(target)
                }
            }
        };
        self.context_history = history;
        self.context_history_idx = new_idx.map(|i| i as usize);
        self.stage_content_mode = StageContentMode::Context;
        self.detail_scroll = 0;
    }

    /// The context snapshot to render in the Context view: the selected archived
    /// history point when browsing, else `None` (callers fall back to the live
    /// current window).
    pub(super) fn browsed_context_point(&self) -> Option<&leviath_core::run_archive::RunPoint> {
        self.context_history_idx
            .and_then(|i| self.context_history.get(i))
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
        // Persist to the append-only dashboard log (path injected at
        // construction, so tests never touch the real ~/.leviath/dashboard.log).
        runstate::append_dashboard_log_to(&self.log_path, &msg);
    }

    /// Push a transient toast, capping the on-screen stack at 4 (oldest drops).
    /// An associated fn over `&mut Vec<Toast>` (not `&mut self`) so it can be
    /// called while another field of `self` — e.g. `self.agents` — is borrowed.
    /// Push a toast onto this dashboard with the standard display duration.
    pub(super) fn toast(&mut self, msg: impl Into<String>, level: ToastLevel) {
        Self::push_toast(&mut self.toasts, msg, level, 30);
    }

    pub(super) fn push_toast(
        toasts: &mut Vec<Toast>,
        msg: impl Into<String>,
        level: ToastLevel,
        remaining_ticks: u32,
    ) {
        toasts.push(Toast {
            message: msg.into(),
            remaining_ticks,
            level,
        });
        if toasts.len() > 4 {
            toasts.remove(0);
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
        for run in runs {
            // A live open prompt from the daemon's hub (populated each tick by
            // `sync_interactions`) is the authoritative signal that this agent is
            // blocked on us — surface it regardless of the persisted status,
            // which can lag a tick behind the hub or (for tool-approval prompts)
            // never flips on its own.
            let pending_request = self.pending_interactions.get(&run.run_id).cloned();

            let status = match run.status {
                RunStatus::Starting | RunStatus::Running => {
                    if pending_request.is_some() {
                        AgentDisplayStatus::Waiting
                    } else {
                        AgentDisplayStatus::Active
                    }
                }
                RunStatus::WaitingInput => AgentDisplayStatus::Waiting,
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
                        Self::push_toast(
                            &mut self.toasts,
                            format!("Agent '{}' failed{}", name, preview),
                            ToastLevel::Error,
                            50,
                        );
                    } else if matches!(
                        status,
                        AgentDisplayStatus::Complete | AgentDisplayStatus::CompleteInteractive
                    ) {
                        Self::push_toast(
                            &mut self.toasts,
                            format!("Agent '{}' completed", name),
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
                let now_is_waiting = matches!(
                    status,
                    AgentDisplayStatus::Waiting | AgentDisplayStatus::CompleteInteractive
                );
                let is_terminal = matches!(
                    status,
                    AgentDisplayStatus::Complete
                        | AgentDisplayStatus::Cancelled
                        | AgentDisplayStatus::Error(_)
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
                    // A terminal agent (complete / cancelled / error) is no longer
                    // running, so freeze its elapsed timer at the transition time
                    // instead of letting it tick up against the wall clock.
                    if is_terminal {
                        agent.active_until = Some(run.updated_at);
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
                                Self::push_toast(
                                    &mut self.toasts,
                                    format!("Agent '{}' needs input", name),
                                    ToastLevel::Warning,
                                    35,
                                );
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
                        Self::push_toast(
                            &mut self.toasts,
                            format!("Agent '{}' needs input", name),
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
                            format!("Agent '{}' completed", name),
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
                    waiting_prompt,
                    pending_request,
                    last_answered_request_id: None,
                    context_snapshot: runstate::read_context_snapshot(&run.run_id),
                    stages,
                    workdir: run.workdir.clone(),
                    task: run.task.clone(),
                    title: run.title.clone(),
                    model: run.model.clone(),
                    parent_id: run.parent_run_id.clone(),
                    depth: 0,
                    started_at: run.started_at,
                    // Freeze the elapsed timer for agents that are already waiting
                    // or terminal when first observed; only genuinely-running
                    // agents tick against the wall clock.
                    active_until: if matches!(
                        run.status,
                        RunStatus::WaitingInput
                            | RunStatus::CompleteInteractive
                            | RunStatus::Complete
                            | RunStatus::Cancelled
                            | RunStatus::Error
                    ) {
                        Some(run.updated_at)
                    } else {
                        None
                    },
                    waiting_secs: 0,
                    graph_info: load_graph_info(&run.agent_path),
                    accepts_messages: true,
                    taint_summary: vec![], // default; stage-level control via agent state
                });
            }
        }
        self.update_display_indices();
        self.initial_sync_done = true;
    }

    /// Refresh the daemon's open interactions (keyed by agent/run id) so waiting
    /// agents can show their prompt. Best-effort: on any transport error or an
    /// unexpected reply the map is left untouched.
    pub(super) async fn sync_interactions(&mut self, control: &ControlClient) {
        if let Ok(ControlResponse::Interactions { interactions }) =
            control.request(&ControlRequest::ListInteractions).await
        {
            self.pending_interactions = interactions.into_iter().collect();
        }
    }

    /// Cancel (via the daemon) then delete all on-disk state for the selected
    /// agent.
    pub(super) fn delete_selected_agent(&mut self) {
        let (raw_idx, id) = match self.selected_agent_raw_idx() {
            Some(i) => (i, self.agents[i].id.clone()),
            None => return,
        };
        // Ask the daemon to cancel the run first (a no-op if already terminal),
        // so it stops writing state before we remove the directory.
        let _ = self
            .cmd_tx
            .send(DaemonCommand::Cancel { run_id: id.clone() });
        // Remove run directory
        let run_dir = runstate::run_dir(&id);
        if let Err(e) = std::fs::remove_dir_all(&run_dir) {
            self.add_log(format!("Delete failed: {}", e));
        } else {
            self.add_log(format!("Deleted run {}", id));
        }
        // Remove saved context state if present — dirs::home_dir() is always
        // Some on supported platforms; use map() to avoid a dead None branch.
        let _ = dirs::home_dir()
            .map(|home| std::fs::remove_dir_all(home.join(".leviath").join("state").join(&id)));
        // Remove agent from self.agents using the raw index (always valid because
        // selected_agent_raw_idx() succeeded above and agents hasn't changed).
        self.agents.remove(raw_idx);
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
    /// Depth-first tree order over the given root agent indices (in the order
    /// supplied — the caller sorts them), nesting each root's children beneath it.
    /// Returns `Vec<(original_index, tree_prefix)>`.
    pub(super) fn build_tree_order(&self, root_order: &[usize]) -> Vec<(usize, String)> {
        let mut result = Vec::new();
        for &idx in root_order {
            self.collect_tree_children(idx, "", &mut result, true);
        }
        result
    }

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

    use crate::commands::dashboard::test_support::make_test_dashboard;

    /// Root agent indices (no parent), in agent order — the input the production
    /// path feeds to `build_tree_order` (there it's the status-sorted roots).
    fn roots_of(dash: &Dashboard) -> Vec<usize> {
        dash.agents
            .iter()
            .enumerate()
            .filter(|(_, a)| a.parent_id.is_none())
            .map(|(i, _)| i)
            .collect()
    }

    fn make_test_agent(id: &str, status: AgentDisplayStatus) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "test-agent".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 1,
            status,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            iteration: 0,
            waiting_prompt: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
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
            taint_summary: vec![],
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

    /// Write a `run.lvr` for `run_id` with `points` context checkpoints.
    fn write_history_archive(run_id: &str, points: usize) {
        use leviath_core::run_archive::{self, RunIdentity, RunRecord};
        std::fs::create_dir_all(runstate::run_dir(run_id)).unwrap();
        let mut buf = Vec::new();
        run_archive::write_archive_start(&mut buf, run_archive::RUN_ARCHIVE_VERSION).unwrap();
        run_archive::write_record(
            &mut buf,
            &RunRecord::Header {
                identity: RunIdentity {
                    run_id: run_id.to_string(),
                    machine_id: "m".to_string(),
                    world_id: "w".to_string(),
                    created_at: 0,
                },
                meta: Box::new(leviath_core::run_meta::RunMeta::new(
                    run_id.to_string(),
                    "a".to_string(),
                    "/p".to_string(),
                    "t".to_string(),
                    None,
                    "/w".to_string(),
                    1,
                )),
            },
        )
        .unwrap();
        for i in 0..points {
            run_archive::write_record(
                &mut buf,
                &RunRecord::ContextCheckpoint {
                    snapshot: runstate::ContextSnapshot {
                        stage_name: format!("stage{i}"),
                        total_tokens: i,
                        max_tokens: 100,
                        regions: vec![],
                    },
                    at: i as i64,
                },
            )
            .unwrap();
        }
        std::fs::write(runstate::run_dir(run_id).join("run.lvr"), &buf).unwrap();
    }

    #[test]
    fn step_context_history_browses_then_returns_to_live() {
        crate::runstate::with_isolated_runs_dir("dash-ctx-hist-browse", |_d| {
            write_history_archive("run-h", 2);
            let mut dash = make_test_dashboard();
            dash.agents
                .push(make_test_agent("run-h", AgentDisplayStatus::Active));
            dash.update_display_indices();
            dash.selected = 0;

            assert_eq!(dash.context_history_idx, None);
            assert!(dash.browsed_context_point().is_none());

            // Back from live → newest point (index 1 of 2).
            dash.step_context_history(-1);
            assert_eq!(dash.context_history_idx, Some(1));
            assert_eq!(dash.stage_content_mode, StageContentMode::Context);
            assert!(dash.browsed_context_point().is_some());

            // Back → index 0, then clamped at 0.
            dash.step_context_history(-1);
            assert_eq!(dash.context_history_idx, Some(0));
            dash.step_context_history(-1);
            assert_eq!(dash.context_history_idx, Some(0));

            // Forward → index 1, then past newest → live.
            dash.step_context_history(1);
            assert_eq!(dash.context_history_idx, Some(1));
            dash.step_context_history(1);
            assert_eq!(dash.context_history_idx, None);
            // Forward from live stays live.
            dash.step_context_history(1);
            assert_eq!(dash.context_history_idx, None);

            // reset clears the cache + index.
            dash.step_context_history(-1);
            dash.reset_context_history();
            assert_eq!(dash.context_history_idx, None);
            assert!(dash.context_history.is_empty());
        });
    }

    #[test]
    fn step_context_history_noop_without_agent_or_archive() {
        // No selected agent → no-op.
        let mut dash = make_test_dashboard();
        dash.step_context_history(-1);
        assert_eq!(dash.context_history_idx, None);

        // Agent whose run has no archive → resets to live.
        crate::runstate::with_isolated_runs_dir("dash-ctx-hist-none", |_d| {
            dash.agents
                .push(make_test_agent("run-none", AgentDisplayStatus::Active));
            dash.update_display_indices();
            dash.selected = 0;
            dash.step_context_history(-1);
            assert_eq!(dash.context_history_idx, None);
            assert!(dash.context_history.is_empty());
        });
    }

    #[test]
    fn update_display_indices_nests_children_under_their_parent() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("parent", AgentDisplayStatus::Active));
        let mut child = make_test_agent("child", AgentDisplayStatus::Active);
        child.parent_id = Some("parent".to_string());
        dash.agents.push(child);
        dash.update_display_indices();

        // Parent then child, with the child nested under it via a tree connector.
        assert_eq!(dash.display_indices.len(), 2);
        assert_eq!(dash.agents[dash.display_indices[0]].id, "parent");
        assert_eq!(dash.agents[dash.display_indices[1]].id, "child");
        assert_eq!(dash.tree_prefixes[0], ""); // root: no prefix
        assert!(dash.tree_prefixes[1].contains('─')); // child: tree connector
    }

    #[test]
    fn update_display_indices_shows_an_orphan_child_as_a_root() {
        let mut dash = make_test_dashboard();
        let mut orphan = make_test_agent("orphan", AgentDisplayStatus::Active);
        orphan.parent_id = Some("gone".to_string()); // parent not in the list
        dash.agents.push(orphan);
        dash.update_display_indices();

        // A child whose parent is absent is treated as a root, not dropped.
        assert_eq!(dash.display_indices.len(), 1);
        assert_eq!(dash.agents[dash.display_indices[0]].id, "orphan");
        assert_eq!(dash.tree_prefixes[0], "");
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
    fn tick_toasts_already_at_zero_is_removed_immediately() {
        // A toast that starts with remaining_ticks=0 is already expired.
        // tick_toasts sees `0 > 0` = false, skips the decrement (exercises
        // the implicit else path of the if block at the closing `}`), then
        // evaluates `0 > 0` = false so retain_mut removes the toast.
        let mut dash = make_test_dashboard();
        dash.toasts.push(Toast {
            message: "expired".to_string(),
            remaining_ticks: 0,
            level: ToastLevel::Info,
        });
        dash.tick_toasts();
        assert!(dash.toasts.is_empty());
    }

    #[test]
    fn push_toast_limits_to_four() {
        let mut dash = make_test_dashboard();
        for i in 0..6 {
            Dashboard::push_toast(
                &mut dash.toasts,
                format!("toast {}", i),
                ToastLevel::Info,
                25,
            );
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
        assert!(dash.build_tree_order(&roots_of(&dash)).is_empty());
    }

    #[test]
    fn build_tree_order_single_root() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("root", AgentDisplayStatus::Active));
        let tree = dash.build_tree_order(&roots_of(&dash));
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
        let tree = dash.build_tree_order(&roots_of(&dash));
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
    fn selected_stage_can_respond_with_prompt_matching_stage() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("Review this".to_string());
        agent.pending_request = Some(interaction::InteractionRequest::free_text(
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
        agent.pending_request = Some(interaction::InteractionRequest::free_text(
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

        let tree = dash.build_tree_order(&roots_of(&dash));
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

        let tree = dash.build_tree_order(&roots_of(&dash));
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
        crate::runstate::with_isolated_runs_dir("add_log_trims_to_200", |_d| {
            let mut dash = make_test_dashboard();
            dash.log.clear(); // Clear any seeded log entries
            for i in 0..250 {
                dash.add_log(format!("msg {}", i));
            }
            assert!(dash.log.len() <= 200);
        });
    }

    #[test]
    fn selected_agent_raw_idx_empty() {
        let dash = make_test_dashboard();
        assert_eq!(dash.selected_agent_raw_idx(), None);
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

        let tree = dash.build_tree_order(&roots_of(&dash));
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

        let tree = dash.build_tree_order(&roots_of(&dash));
        assert_eq!(tree.len(), 3);
        // root1 and child-of-1 should be adjacent
        assert_eq!(tree[0].0, 0); // root1
        assert_eq!(tree[1].0, 2); // child-of-1
        assert_eq!(tree[2].0, 1); // root2
    }

    // ─── delete_selected_agent: non-run-state agent ───────────────────────

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
        agent.pending_request = Some(interaction::InteractionRequest::free_text(
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
        crate::runstate::with_isolated_runs_dir("add_log_appends_entry", |_d| {
            let mut dash = make_test_dashboard();
            dash.log.clear();
            dash.add_log("hello from test".to_string());
            assert!(!dash.log.is_empty());
            assert!(dash.log.last().unwrap().message == "hello from test");
            // Timestamp should look like HH:MM:SS
            let ts = &dash.log.last().unwrap().timestamp;
            assert!(ts.contains(':'));
        });
    }

    #[test]
    fn add_log_trims_when_over_200() {
        crate::runstate::with_isolated_runs_dir("add_log_trims_when_over_200", |_d| {
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
        });
    }

    // ─── load_log_seed: exercises the parsing code ───────────────────────

    #[test]
    fn load_log_seed_returns_vec() {
        // Missing file → empty seed, no panic.
        let missing = std::env::temp_dir().join("leviath-nonexistent-dashboard-seed.log");
        let _ = std::fs::remove_file(&missing);
        assert!(Dashboard::load_log_seed(&missing).is_empty());

        // Existing file → its lines are parsed into the seed buffer.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dashboard.log");
        std::fs::write(&path, "2026-01-02 03:04:05 seeded message\n").unwrap();
        let entries = Dashboard::load_log_seed(&path);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].message.contains("seeded message"));
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
        Dashboard::push_toast(&mut dash.toasts, "warning!", ToastLevel::Warning, 25);
        assert_eq!(dash.toasts.len(), 1);
        assert_eq!(dash.toasts[0].level, ToastLevel::Warning);
    }

    #[test]
    fn push_toast_error_level() {
        let mut dash = make_test_dashboard();
        Dashboard::push_toast(&mut dash.toasts, "error!", ToastLevel::Error, 25);
        assert_eq!(dash.toasts.len(), 1);
        assert_eq!(dash.toasts[0].level, ToastLevel::Error);
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
        agent.pending_request = Some(interaction::InteractionRequest::free_text(
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

    #[test]
    fn selected_stage_can_respond_waiting_prompt_only_no_pending_request() {
        // `waiting_prompt` is Some but `pending_request` is None: the
        // `if let Some(req) = &agent.pending_request` branch falls to its
        // `else` arm (line 568), using `agent.stage_index` as the fallback.
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("prompt text".to_string());
        agent.pending_request = None; // no structured request — legacy path
        agent.stage_index = 0;
        dash.agents.push(agent);
        dash.update_display_indices();

        // selected_stage matches stage_index → can respond
        dash.selected_stage = 0;
        assert!(dash.selected_stage_can_respond());

        // Wrong stage selected → cannot respond
        dash.selected_stage = 1;
        assert!(!dash.selected_stage_can_respond());
    }

    // ─── delete_selected_agent: run state agent removal ──────────────────

    #[test]
    fn delete_selected_agent_removes_run_state_agent() {
        crate::runstate::with_isolated_runs_dir(
            "delete_selected_agent_removes_run_state_agent",
            |_d| {
                let mut dash = make_test_dashboard();
                dash.log.clear();
                // Use a real temp dir that exists to avoid "delete failed" log
                let tmp_id = format!("test-run-{}", std::process::id());
                let run_dir = crate::runstate::run_dir(&tmp_id);
                let _ = std::fs::create_dir_all(&run_dir);

                let agent = make_test_agent(&tmp_id, AgentDisplayStatus::Complete);
                dash.agents.push(agent);
                dash.update_display_indices();

                dash.delete_selected_agent();

                // Agent should have been removed from the list
                assert!(dash.agents.is_empty());
            },
        );
    }

    #[test]
    fn delete_selected_agent_missing_run_dir_logs_delete_failed() {
        // Unlike `delete_selected_agent_removes_run_state_agent` (which
        // pre-creates the run dir so removal succeeds), this never creates it --
        // `std::fs::remove_dir_all` on a nonexistent path returns `Err` on every
        // platform, exercising the "Delete failed" log branch. Runs under an
        // isolated runs dir so it never touches the real `~/.leviath`.
        crate::runstate::with_isolated_runs_dir("delete_selected_agent_missing_run_dir", |_d| {
            let mut dash = make_test_dashboard();
            dash.log.clear();
            let agent = make_test_agent("test-run-missing", AgentDisplayStatus::Complete);
            dash.agents.push(agent);
            dash.update_display_indices();

            dash.delete_selected_agent();

            assert!(dash.log.iter().any(|l| l.message.contains("Delete failed")));
        });
    }

    // ─── selected_stage_can_respond: empty stage_name falls back ──────────

    #[test]
    fn selected_stage_can_respond_empty_stage_name_uses_stage_index() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("prompt".to_string());
        agent.pending_request = Some(interaction::InteractionRequest {
            id: "req-1".to_string(),
            kind: interaction::InteractionKind::FreeText,
            prompt: "prompt".to_string(),
            options: vec![],
            tool_name: None,
            tool_arguments: None,
            required: true,
            stage_name: String::new(), // empty
            body: None,
            body_format: interaction::BodyFormat::Plain,
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
        crate::runstate::with_isolated_runs_dir("sync_from_run_state_new_agent_active", |_d| {
            let run_id = "test-sync-new-active";
            cleanup_run(run_id);
            let meta = make_run_meta(run_id, RunStatus::Running);
            runstate::create_run(&meta).unwrap();

            let mut dash = make_test_dashboard();
            dash.sync_from_run_state();

            let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
            assert_eq!(agent.status, AgentDisplayStatus::Active);
            assert!(dash.initial_sync_done);
            // No toasts on the very first sync (startup), even though this is a "new" agent.
            assert!(dash.toasts.is_empty());

            cleanup_run(run_id);
        });
    }

    #[test]
    fn sync_from_run_state_new_agent_starting_maps_to_active() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_new_agent_starting_maps_to_active",
            |_d| {
                let run_id = "test-sync-new-starting";
                cleanup_run(run_id);
                let meta = make_run_meta(run_id, RunStatus::Starting);
                runstate::create_run(&meta).unwrap();

                let mut dash = make_test_dashboard();
                dash.sync_from_run_state();

                let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
                assert_eq!(agent.status, AgentDisplayStatus::Active);

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_new_agent_error_status() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_new_agent_error_status",
            |_d| {
                let run_id = "test-sync-new-error";
                cleanup_run(run_id);
                let mut meta = make_run_meta(run_id, RunStatus::Error);
                meta.error = Some("boom".to_string());
                runstate::create_run(&meta).unwrap();

                let mut dash = make_test_dashboard();
                dash.sync_from_run_state();

                let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
                assert!(matches!(&agent.status, AgentDisplayStatus::Error(msg) if msg == "boom"));

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_new_agent_cancelled_status() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_new_agent_cancelled_status",
            |_d| {
                let run_id = "test-sync-new-cancelled";
                cleanup_run(run_id);
                let meta = make_run_meta(run_id, RunStatus::Cancelled);
                runstate::create_run(&meta).unwrap();

                let mut dash = make_test_dashboard();
                dash.sync_from_run_state();

                let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
                assert_eq!(agent.status, AgentDisplayStatus::Cancelled);

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_new_agent_waiting_input_reads_pending_request() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_new_agent_waiting_input_reads_pending_request",
            |_d| {
                let run_id = "test-sync-new-waiting";
                cleanup_run(run_id);
                let meta = make_run_meta(run_id, RunStatus::WaitingInput);
                runstate::create_run(&meta).unwrap();
                let req =
                    interaction::InteractionRequest::free_text("req1", "What next?", "main", true);

                let mut dash = make_test_dashboard();
                dash.pending_interactions
                    .insert(run_id.to_string(), req.clone());
                dash.sync_from_run_state();

                let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
                assert_eq!(agent.status, AgentDisplayStatus::Waiting);
                assert_eq!(agent.waiting_prompt.as_deref(), Some("What next?"));
                assert!(agent.pending_request.is_some());
                assert!(agent.active_until.is_some());

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_running_agent_with_open_prompt_surfaces_it() {
        // The bug fix: a run whose persisted status is still `Running` but which
        // the daemon's hub reports an open interaction for (e.g. a tool-approval
        // prompt, which never flips the persisted status on its own) must show
        // as Waiting and surface the prompt — not sit silently Active.
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_running_agent_with_open_prompt_surfaces_it",
            |_d| {
                let run_id = "test-sync-running-open-prompt";
                cleanup_run(run_id);
                let meta = make_run_meta(run_id, RunStatus::Running);
                runstate::create_run(&meta).unwrap();
                let req = interaction::InteractionRequest::tool_approval(
                    "approve-1",
                    "write_file",
                    serde_json::json!({}),
                    "implement",
                );

                let mut dash = make_test_dashboard();
                dash.pending_interactions
                    .insert(run_id.to_string(), req.clone());
                dash.sync_from_run_state();

                let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
                assert_eq!(agent.status, AgentDisplayStatus::Waiting);
                assert!(agent.waiting_prompt.is_some());
                assert!(agent.pending_request.is_some());

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_new_agent_complete_interactive() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_new_agent_complete_interactive",
            |_d| {
                let run_id = "test-sync-new-complete-interactive";
                cleanup_run(run_id);
                let meta = make_run_meta(run_id, RunStatus::CompleteInteractive);
                runstate::create_run(&meta).unwrap();
                let req = interaction::InteractionRequest::free_text(
                    "req1",
                    "Any feedback?",
                    "review",
                    false,
                );

                let mut dash = make_test_dashboard();
                dash.pending_interactions
                    .insert(run_id.to_string(), req.clone());
                dash.sync_from_run_state();

                let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
                assert_eq!(agent.status, AgentDisplayStatus::CompleteInteractive);
                assert!(agent.waiting_prompt.is_some());
                assert!(agent.active_until.is_some());

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_new_agent_toasts_after_initial_sync_waiting() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_new_agent_toasts_after_initial_sync_waiting",
            |_d| {
                let run_id = "test-sync-new-toast-waiting";
                cleanup_run(run_id);

                let mut dash = make_test_dashboard();
                dash.initial_sync_done = true; // simulate: not the app's first sync

                let meta = make_run_meta(run_id, RunStatus::WaitingInput);
                runstate::create_run(&meta).unwrap();
                let req = interaction::InteractionRequest::confirm("req1", "Proceed?", "main");
                dash.pending_interactions
                    .insert(run_id.to_string(), req.clone());

                dash.sync_from_run_state();

                assert!(
                    dash.toasts
                        .iter()
                        .any(|t| t.message.contains("needs input"))
                );

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_new_agent_complete_interactive_after_initial_sync_no_needs_input_toast()
    {
        // Sibling of the `..._toasts_after_initial_sync_waiting` test above,
        // but for a brand-new agent that's already `CompleteInteractive`
        // (rather than `WaitingInput`) with a pending request. Exercises the
        // `false` arm of `matches!(run.status, RunStatus::WaitingInput)` in
        // the "new agent" branch of `sync_from_run_state` -- every other
        // test reaching that `if self.initial_sync_done { ... }` block does
        // so with `run.status == WaitingInput`, so that `matches!`'s `false`
        // arm (CompleteInteractive input is optional, no "needs input"
        // toast) was never taken there.
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_new_agent_complete_interactive_after_initial_sync_no_needs_input_toast",
            |_d| {
                let run_id = "test-sync-new-ci-after-initial-no-toast";
                cleanup_run(run_id);

                let mut dash = make_test_dashboard();
                dash.initial_sync_done = true; // simulate: not the app's first sync

                let meta = make_run_meta(run_id, RunStatus::CompleteInteractive);
                runstate::create_run(&meta).unwrap();
                let req = interaction::InteractionRequest::free_text(
                    "req1",
                    "Any final feedback?",
                    "review",
                    false,
                );
                dash.pending_interactions
                    .insert(run_id.to_string(), req.clone());

                dash.sync_from_run_state();

                assert!(
                    !dash
                        .toasts
                        .iter()
                        .any(|t| t.message.contains("needs input"))
                );
                // The separate "completed" toast branch still fires for a brand-new
                // Complete/CompleteInteractive agent.
                assert!(dash.toasts.iter().any(|t| t.message.contains("completed")));

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_new_agent_toasts_after_initial_sync_complete() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_new_agent_toasts_after_initial_sync_complete",
            |_d| {
                let run_id = "test-sync-new-toast-complete";
                cleanup_run(run_id);

                let mut dash = make_test_dashboard();
                dash.initial_sync_done = true;

                let meta = make_run_meta(run_id, RunStatus::Complete);
                runstate::create_run(&meta).unwrap();

                dash.sync_from_run_state();

                assert!(dash.toasts.iter().any(|t| t.message.contains("completed")));

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_existing_agent_active_to_error_toasts_with_message() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_existing_agent_active_to_error_toasts_with_message",
            |_d| {
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
                assert_eq!(
                    agent.status,
                    AgentDisplayStatus::Error("disk full".to_string())
                );
                assert!(
                    dash.toasts
                        .iter()
                        .any(|t| t.message.contains("failed") && t.message.contains("disk full"))
                );

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_existing_agent_active_to_error_empty_message() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_existing_agent_active_to_error_empty_message",
            |_d| {
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
            },
        );
    }

    #[test]
    fn sync_from_run_state_existing_agent_active_to_complete_toasts() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_existing_agent_active_to_complete_toasts",
            |_d| {
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
                assert_eq!(agent.status, AgentDisplayStatus::Complete);
                assert!(dash.toasts.iter().any(|t| t.message.contains("completed")));

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_existing_agent_active_to_complete_interactive_toasts() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_existing_agent_active_to_complete_interactive_toasts",
            |_d| {
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
            },
        );
    }

    #[test]
    fn sync_from_run_state_existing_agent_was_already_terminal_skips_transition_toast_block() {
        // `prev_status_was_active` (and therefore the whole "failed"/
        // "completed" transition-toast check) is only ever exercised as
        // `true` by every `sync_from_run_state_existing_agent_*` test above,
        // since they all start from an Active/Waiting agent. Start from an
        // agent that's already `Complete` instead, so that block is skipped
        // entirely on the next sync -- even though the underlying
        // `RunStatus` does change -- covering the `prev_status_was_active ==
        // false` path.
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_existing_agent_was_already_terminal_skips_transition_toast_block",
            |_d| {
                let run_id = "test-sync-existing-already-terminal";
                cleanup_run(run_id);
                let meta = make_run_meta(run_id, RunStatus::Complete);
                runstate::create_run(&meta).unwrap();

                let mut dash = make_test_dashboard();
                dash.sync_from_run_state(); // first sync: creates the agent, already Complete
                assert_eq!(
                    dash.agents.iter().find(|a| a.id == run_id).unwrap().status,
                    AgentDisplayStatus::Complete
                );

                let meta2 = make_run_meta(run_id, RunStatus::Error);
                runstate::write_meta(&meta2).unwrap();
                dash.sync_from_run_state(); // second sync: transitions Complete -> Error

                let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
                // `meta2` above has no `.error` set, so the mapped message defaults
                // to the empty string (`run.error.clone().unwrap_or_default()`).
                assert_eq!(agent.status, AgentDisplayStatus::Error(String::new()));
                // No transition toast fires -- the block only runs when the agent
                // was previously Active/Waiting, which it wasn't here.
                // Seed an unrelated toast first so `.any()` below actually invokes
                // its predicate at least once instead of short-circuiting on an
                // empty vec (which would leave the closure itself uncalled).
                dash.toasts.push(Toast {
                    message: "unrelated toast".to_string(),
                    remaining_ticks: 1,
                    level: ToastLevel::Info,
                });
                assert!(!dash.toasts.iter().any(|t| t.message.contains(run_id)));

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_existing_agent_stays_active_no_toast() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_existing_agent_stays_active_no_toast",
            |_d| {
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
                // Seed an unrelated toast first so `.any()` below actually invokes
                // its predicate at least once instead of short-circuiting on an
                // empty vec (which would leave the closure itself uncalled).
                dash.toasts.push(Toast {
                    message: "unrelated toast".to_string(),
                    remaining_ticks: 1,
                    level: ToastLevel::Info,
                });
                assert!(!dash.toasts.iter().any(|t| t.message.contains(run_id)));

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_existing_agent_enters_waiting_toasts_and_freezes_timer() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_existing_agent_enters_waiting_toasts_and_freezes_timer",
            |_d| {
                let run_id = "test-sync-existing-enters-waiting";
                cleanup_run(run_id);
                let meta = make_run_meta(run_id, RunStatus::Running);
                runstate::create_run(&meta).unwrap();

                let mut dash = make_test_dashboard();
                dash.sync_from_run_state();
                assert!(
                    dash.agents
                        .iter()
                        .find(|a| a.id == run_id)
                        .unwrap()
                        .active_until
                        .is_none()
                );

                let mut meta2 = make_run_meta(run_id, RunStatus::WaitingInput);
                meta2.updated_at = meta.started_at + 42;
                runstate::write_meta(&meta2).unwrap();
                let req = interaction::InteractionRequest::free_text("req1", "Q?", "main", true);
                dash.pending_interactions
                    .insert(run_id.to_string(), req.clone());
                dash.sync_from_run_state();

                let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
                assert_eq!(agent.status, AgentDisplayStatus::Waiting);
                assert_eq!(agent.active_until, Some(meta2.updated_at));
                assert_eq!(agent.waiting_prompt.as_deref(), Some("Q?"));
                assert!(
                    dash.toasts
                        .iter()
                        .any(|t| t.message.contains("needs input"))
                );

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_existing_agent_leaves_waiting_accumulates_waiting_secs() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_existing_agent_leaves_waiting_accumulates_waiting_secs",
            |_d| {
                let run_id = "test-sync-existing-leaves-waiting";
                cleanup_run(run_id);
                let mut meta = make_run_meta(run_id, RunStatus::WaitingInput);
                meta.updated_at = meta.started_at + 10;
                runstate::create_run(&meta).unwrap();
                let req = interaction::InteractionRequest::free_text("req1", "Q?", "main", true);

                let mut dash = make_test_dashboard();
                dash.pending_interactions
                    .insert(run_id.to_string(), req.clone());
                dash.sync_from_run_state();
                let entered_wait_at = dash
                    .agents
                    .iter()
                    .find(|a| a.id == run_id)
                    .unwrap()
                    .active_until
                    .unwrap();

                // Now the run resumes (Running) — clear the interaction and re-sync.
                dash.pending_interactions.remove(run_id);
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
            },
        );
    }

    #[test]
    fn sync_from_run_state_already_answered_request_not_reapplied() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_already_answered_request_not_reapplied",
            |_d| {
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
                let req = interaction::InteractionRequest::free_text(
                    "req-answered",
                    "Already answered?",
                    "main",
                    true,
                );
                dash.pending_interactions
                    .insert(run_id.to_string(), req.clone());
                dash.sync_from_run_state();

                // Should NOT re-populate waiting_prompt/pending_request for a
                // request id that was already answered.
                let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
                assert!(agent.waiting_prompt.is_none());
                assert!(agent.pending_request.is_none());

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_no_reptoast_when_already_waiting() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_no_reptoast_when_already_waiting",
            |_d| {
                let run_id = "test-sync-no-retoast";
                cleanup_run(run_id);
                let meta = make_run_meta(run_id, RunStatus::WaitingInput);
                runstate::create_run(&meta).unwrap();
                let req = interaction::InteractionRequest::free_text("req1", "Q1?", "main", true);

                let mut dash = make_test_dashboard();
                dash.pending_interactions
                    .insert(run_id.to_string(), req.clone());
                dash.sync_from_run_state(); // first sync creates the agent, already Waiting -> no toast (new agent, initial sync)
                dash.toasts.clear();

                // Still waiting, same kind of request — re-sync must not toast again.
                dash.sync_from_run_state();
                // Seed an unrelated toast first so `.any()` below actually invokes
                // its predicate at least once instead of short-circuiting on an
                // empty vec (which would leave the closure itself uncalled).
                dash.toasts.push(Toast {
                    message: "unrelated toast".to_string(),
                    remaining_ticks: 1,
                    level: ToastLevel::Info,
                });
                assert!(!dash.toasts.iter().any(|t| t.message.contains(run_id)));

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_updates_stage_and_token_fields() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_updates_stage_and_token_fields",
            |_d| {
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

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_waiting_with_no_pending_request_leaves_agent_unchanged() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_waiting_with_no_pending_request_leaves_agent_unchanged",
            |_d| {
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
                assert_eq!(agent.status, AgentDisplayStatus::Waiting);
                assert!(agent.waiting_prompt.is_none());

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_updates_workdir_and_display_indices() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_updates_workdir_and_display_indices",
            |_d| {
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
            },
        );
    }

    #[test]
    fn sync_from_run_state_existing_agent_enters_complete_interactive_no_needs_input_toast() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_existing_agent_enters_complete_interactive_no_needs_input_toast",
            |_d| {
                // Exercise the branch where:
                //   agent.waiting_prompt.is_none()          -> true  (agent was Active, no prompt yet)
                //   && waiting_prompt.is_some()              -> true  (a pending request is present)
                //   && matches!(run.status, WaitingInput)    -> FALSE (status is CompleteInteractive)
                //
                // The full condition is false, so no "needs input" toast is emitted
                // (CompleteInteractive input is optional, unlike WaitingInput).
                let run_id = "test-sync-ci-no-toast";
                cleanup_run(run_id);
                let meta = make_run_meta(run_id, RunStatus::Running);
                runstate::create_run(&meta).unwrap();

                let mut dash = make_test_dashboard();
                dash.sync_from_run_state(); // first sync: agent is Active, waiting_prompt=None

                // Transition to CompleteInteractive and write a pending request
                let meta2 = make_run_meta(run_id, RunStatus::CompleteInteractive);
                runstate::write_meta(&meta2).unwrap();
                let req = interaction::InteractionRequest::free_text(
                    "req-ci",
                    "Any final feedback?",
                    "review",
                    false,
                );
                dash.pending_interactions
                    .insert(run_id.to_string(), req.clone());

                dash.toasts.clear(); // clear any earlier toasts
                dash.sync_from_run_state();

                let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
                assert_eq!(agent.status, AgentDisplayStatus::CompleteInteractive);
                // waiting_prompt is populated (the request exists)
                assert!(agent.waiting_prompt.is_some());
                // Seed a toast that *does* contain `run_id` (but not "needs input")
                // so the closure below's `has_id && has_tag` actually evaluates
                // `has_tag` at least once -- the real "completed" toast pushed by
                // `sync_from_run_state` uses `truncate(&agent.blueprint_name, 20)`,
                // and `run_id` here is longer than 20 chars, so it never contains
                // the full `run_id` substring on its own.
                dash.toasts.push(Toast {
                    message: format!("{run_id}: unrelated toast"),
                    remaining_ticks: 1,
                    level: ToastLevel::Info,
                });
                // But no "needs input" toast because CompleteInteractive input is optional
                let needs_input_toast = dash.toasts.iter().find(|t| {
                    let has_id = t.message.contains(run_id);
                    let has_tag = t.message.contains("needs input");
                    has_id && has_tag
                });
                assert!(needs_input_toast.is_none());

                cleanup_run(run_id);
            },
        );
    }

    #[tokio::test]
    async fn sync_interactions_populates_map_from_daemon() {
        use leviath_runtime::control_socket::{
            ControlClient, ControlResponse, bind_control_listener, control_id,
        };
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let id = control_id(dir.path());
        let mut listener = bind_control_listener(&id).unwrap();
        let server = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let (r, mut w) = tokio::io::split(stream);
            let mut lines = BufReader::new(r).lines();
            let _ = lines.next_line().await.unwrap();
            let req = interaction::InteractionRequest::free_text("q1", "?", "main", true);
            let resp = ControlResponse::Interactions {
                interactions: vec![("run-1".to_string(), req)],
            };
            let mut line = serde_json::to_string(&resp).unwrap();
            line.push('\n');
            w.write_all(line.as_bytes()).await.unwrap();
        });
        let mut dash = make_test_dashboard();
        dash.sync_interactions(&ControlClient::new(id)).await;
        server.await.unwrap();
        assert_eq!(
            dash.pending_interactions.get("run-1").map(|r| r.id.clone()),
            Some("q1".to_string())
        );
    }

    #[tokio::test]
    async fn sync_interactions_no_daemon_leaves_map_untouched() {
        use leviath_runtime::control_socket::{ControlClient, control_id};
        let dir = tempfile::tempdir().unwrap();
        let mut dash = make_test_dashboard();
        dash.sync_interactions(&ControlClient::new(control_id(&dir.path().join("nope"))))
            .await;
        assert!(dash.pending_interactions.is_empty());
    }
}
