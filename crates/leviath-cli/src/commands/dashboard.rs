//! `lev dashboard` - Interactive terminal UI for managing concurrent agents.

use clap::Args;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, MouseEvent, MouseEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use leviath_runtime::{
    AgentEngine, AgentState, AgentStatus, ContextWindow,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect, Margin},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, BorderType, Cell, Clear, Padding,
        Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState,
        Table, TableState, Tabs, Wrap,
    },
    Frame, Terminal,
};
use std::io::stdout;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

use super::run::build_provider_registry;
use crate::config::Config;
use crate::interaction;
use crate::render;
use crate::runstate::{self, RunStatus, StageRecord, StageRunStatus};

// ─── Theme palette ────────────────────────────────────────────────────────────

const C_ACCENT: Color = Color::Cyan;
const C_SUCCESS: Color = Color::Green;
const C_WARN: Color = Color::Yellow;
const C_ERROR: Color = Color::Red;
const C_DIM: Color = Color::DarkGray;
const C_MUTED: Color = Color::Gray;
const C_WHITE: Color = Color::White;
const C_ACTIVE: Color = Color::Cyan;
const C_BORDER: Color = Color::DarkGray;
const C_BORDER_FOCUS: Color = Color::Cyan;

// Stage status glyphs
const GLYPH_PENDING: &str = "○";
const GLYPH_ACTIVE: &str = "●";
const GLYPH_WAITING: &str = "⏸";
const GLYPH_COMPLETE: &str = "✓";
const GLYPH_ERROR: &str = "✗";

// Spinner frames driven by tick_count
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Args)]
pub struct DashboardArgs {}

/// Whether the detail content pane shows Output or Logs.
#[derive(Debug, Clone, PartialEq)]
enum StageContentMode {
    Output,
    Logs,
}

/// Display status for agents in the dashboard.
#[derive(Debug, Clone)]
pub enum AgentDisplayStatus {
    Active,
    Waiting,
    Complete,
    /// All required work done; still accepting optional follow-up input.
    CompleteInteractive,
    Error(String),
    Idle,
    Cancelled,
}

impl std::fmt::Display for AgentDisplayStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "{}ACTIVE", GLYPH_ACTIVE),
            Self::Waiting => write!(f, "{}WAITING", GLYPH_WAITING),
            Self::Complete => write!(f, "{}COMPLETE", GLYPH_COMPLETE),
            Self::CompleteInteractive => write!(f, "{}COMPLETE", GLYPH_COMPLETE),
            Self::Error(msg) => write!(f, "{}ERROR: {}", GLYPH_ERROR, msg),
            Self::Idle => write!(f, "{}IDLE", GLYPH_PENDING),
            Self::Cancelled => write!(f, "⊘CANCEL"),
        }
    }
}

impl AgentDisplayStatus {
    fn color(&self) -> Color {
        match self {
            Self::Active => C_ACTIVE,
            Self::Waiting => C_WARN,
            Self::Complete | Self::CompleteInteractive => C_SUCCESS,
            Self::Error(_) => C_ERROR,
            Self::Idle => C_DIM,
            Self::Cancelled => C_DIM,
        }
    }
}

/// An agent displayed in the dashboard.
#[derive(Debug, Clone)]
pub struct DashboardAgent {
    pub id: String,
    pub blueprint_name: String,
    /// Path to the agent manifest directory (blueprint source)
    #[allow(dead_code)]
    pub agent_path: String,
    pub stage: String,
    pub stage_index: usize,
    pub num_stages: usize,
    pub status: AgentDisplayStatus,
    /// Cumulative prompt (input) tokens for background runs.
    pub tokens_in: usize,
    /// Cumulative completion (output) tokens for background runs.
    pub tokens_out: usize,
    /// Context-window occupancy for in-process agents: (current, max).
    pub context_tokens: (usize, usize),
    pub iteration: usize,
    pub waiting_prompt: Option<String>,
    /// Full structured interaction request (populated for WaitingInput agents)
    pub pending_request: Option<interaction::InteractionRequest>,
    /// The request_id we most recently submitted a response for, used to suppress
    /// re-showing the same prompt before the worker has consumed the response.
    pub last_answered_request_id: Option<String>,
    /// Live context window snapshot from context.json (background workers only)
    pub context_snapshot: Option<runstate::ContextSnapshot>,
    /// Per-stage records from stages.json
    pub stages: Vec<StageRecord>,
    /// The ECS entity for this agent (dummy sentinel for run-state agents)
    pub entity: bevy_ecs::prelude::Entity,
    /// True when tracked via on-disk run-state (background worker process)
    pub is_run_state: bool,
    /// PID of worker process (0 for in-process agents)
    pub pid: u32,
    /// Working directory the agent ran in
    pub workdir: String,
    /// Original task prompt
    pub task: String,
    /// Auto-generated short title (None until the worker generates it).
    pub title: Option<String>,
    /// Original model override
    #[allow(dead_code)]
    pub model: Option<String>,
    /// Unix timestamp when the run started (for elapsed display)
    pub started_at: i64,
}

/// Event from an agent back to the dashboard.
#[derive(Debug, Clone)]
#[allow(dead_code)] // All variants are part of the agent event protocol
pub enum AgentEvent {
    StageChanged { agent_id: String, stage: String },
    StatusChanged { agent_id: String, status: AgentDisplayStatus },
    NeedsInput { agent_id: String, prompt: String },
    ToolCalled { agent_id: String, tool: String, args: String },
    InferenceComplete { agent_id: String, content: String, tokens_used: usize, tokens_prompt: usize },
    Error { agent_id: String, error: String },
    Log(String),
    AgentDone { agent_id: String },
}

/// Log entry for the dashboard log panel.
#[derive(Debug, Clone)]
struct LogEntry {
    timestamp: String,
    message: String,
}

/// Command sent from the dashboard to the engine background task.
#[derive(Debug)]
enum EngineCommand {
    CancelAgent { agent_id: String },
    SendInput { agent_id: String, input: String },
}

/// Toast notification shown as an overlay.
#[derive(Debug, Clone)]
struct Toast {
    message: String,
    remaining_ticks: u32,
    level: ToastLevel,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum ToastLevel {
    Info,
    Warning,
    Error,
}

/// The interactive dashboard state.
struct Dashboard {
    agents: Vec<DashboardAgent>,
    selected: usize,
    log: Vec<LogEntry>,
    input_buffer: String,
    input_mode: bool,
    /// True when the full-screen detail view is open for the selected agent
    detail_view: bool,
    event_rx: mpsc::UnboundedReceiver<AgentEvent>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    table_state: TableState,
    should_quit: bool,
    /// True when the delete-agent confirmation popup is open
    confirm_delete: bool,
    /// Scroll offset for detail view content: 0 = bottom (auto-scroll), >0 = scrolled up
    detail_scroll: usize,
    /// Selected option index for MultipleChoice/ToolApproval/Confirm input
    choice_selected: usize,
    /// Which stage tab is currently focused in the detail view
    selected_stage: usize,
    /// Whether the content pane shows Output or Logs
    stage_content_mode: StageContentMode,
    /// Monotonic tick counter for animations (spinner, toast timeouts)
    tick_count: u64,
    /// Active toast notifications
    toasts: Vec<Toast>,
    /// True when the help overlay (?) is shown
    show_help: bool,
    /// Scroll offset within the review body pane (present_for_review).
    review_scroll: usize,
    // ── Search ────────────────────────────────────────────────────────────────
    /// True when the user is typing a search query (entered with `/`).
    search_mode: bool,
    /// Current search query (empty = no active search).
    search_query: String,
    /// Index into the matched-lines list of the currently highlighted match.
    search_match_idx: usize,
}

impl Dashboard {
    fn new(cmd_tx: mpsc::UnboundedSender<EngineCommand>) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        // Seed the in-memory log buffer from the tail of the persistent log so
        // the panel shows recent history immediately on launch (not a blank panel).
        let log = Self::load_log_seed();

        Self {
            agents: Vec::new(),
            selected: 0,
            log,
            input_buffer: String::new(),
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
            tick_count: 0,
            toasts: Vec::new(),
            show_help: false,
            review_scroll: 0,
            search_mode: false,
            search_query: String::new(),
            search_match_idx: 0,
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
            let timestamp = if time.is_empty() { date.to_string() } else { time.to_string() };
            entries.push(LogEntry { timestamp, message });
        }
        entries
    }

    fn add_log(&mut self, msg: String) {
        let now = chrono::Local::now();
        let timestamp = now.format("%H:%M:%S").to_string();
        self.log.push(LogEntry { timestamp, message: msg.clone() });
        if self.log.len() > 200 {
            self.log.remove(0);
        }
        // Persist to the append-only dashboard log.
        runstate::append_dashboard_log(&msg);
    }

    #[allow(dead_code)]
    fn push_toast(&mut self, msg: impl Into<String>, level: ToastLevel) {
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

    fn tick_toasts(&mut self) {
        self.toasts.retain_mut(|t| {
            if t.remaining_ticks > 0 { t.remaining_ticks -= 1; }
            t.remaining_ticks > 0
        });
    }

    /// Sync agent list from on-disk run-state dir (background workers).
    fn sync_from_run_state(&mut self) {
        let runs = runstate::list_runs();
        let dummy = bevy_ecs::prelude::Entity::from_raw(u32::MAX);
        for run in runs {
            let status = match run.status {
                RunStatus::Starting | RunStatus::Running => AgentDisplayStatus::Active,
                RunStatus::WaitingInput => AgentDisplayStatus::Waiting,
                RunStatus::Complete => AgentDisplayStatus::Complete,
                RunStatus::CompleteInteractive => AgentDisplayStatus::CompleteInteractive,
                RunStatus::Error => AgentDisplayStatus::Error(run.error.clone().unwrap_or_default()),
                RunStatus::Cancelled => AgentDisplayStatus::Cancelled,
            };

            // For WaitingInput and CompleteInteractive agents, read the pending interaction from disk once
            let needs_input = matches!(run.status, RunStatus::WaitingInput | RunStatus::CompleteInteractive);
            let (waiting_prompt, pending_request) = if needs_input {
                let req = interaction::read_request(&run.run_id);
                (req.as_ref().map(|r| r.prompt.clone()), req)
            } else {
                (None, None)
            };

            // Read stages index
            let stages = runstate::read_stages_index(&run.run_id);

            if let Some(agent) = self.agents.iter_mut().find(|a| a.id == run.run_id) {
                let prev_status_was_active = matches!(agent.status, AgentDisplayStatus::Active | AgentDisplayStatus::Waiting);
                let now_needs_input = needs_input;

                // Toast on terminal state transitions
                let name = agent.title.clone()
                    .unwrap_or_else(|| truncate(&agent.blueprint_name, 20));
                if prev_status_was_active {
                    if let AgentDisplayStatus::Error(msg) = &status {
                        let preview = if msg.is_empty() { String::new() } else {
                            format!(": {}", truncate(msg, 40))
                        };
                        self.toasts.push(Toast {
                            message: format!("Agent '{}' failed{}", name, preview),
                            remaining_ticks: 50,
                            level: ToastLevel::Error,
                        });
                    } else if matches!(status, AgentDisplayStatus::Complete | AgentDisplayStatus::CompleteInteractive) {
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
                agent.title = run.title.clone();
                agent.pid = run.pid;
                agent.status = status;
                agent.workdir = run.workdir.clone();
                agent.context_snapshot = runstate::read_context_snapshot(&run.run_id);
                agent.stages = stages;

                if now_needs_input {
                    if waiting_prompt.is_some() {
                        let pending_id = pending_request.as_ref().map(|r| r.id.as_str()).unwrap_or("");
                        let already_answered = agent.last_answered_request_id.as_deref()
                            .map(|a| !a.is_empty() && a == pending_id)
                            .unwrap_or(false);
                        if !already_answered {
                            if agent.waiting_prompt.is_none() && waiting_prompt.is_some() {
                                // Newly needs input — toast
                                let name = agent.title.clone()
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
                // New agent — check if it needs input right away
                if needs_input && waiting_prompt.is_some() {
                    let name = run.title.clone().unwrap_or_else(|| truncate(&run.agent_name, 20));
                    self.toasts.push(Toast {
                        message: format!("Agent '{}' needs input", name),
                        remaining_ticks: 35,
                        level: ToastLevel::Warning,
                    });
                }
                // Check for completion toast for new entries we haven't seen before
                if matches!(run.status, RunStatus::Complete | RunStatus::CompleteInteractive) {
                    let name = run.title.clone().unwrap_or_else(|| truncate(&run.agent_name, 20));
                    self.toasts.push(Toast {
                        message: format!("Agent '{}' completed", name),
                        remaining_ticks: 35,
                        level: ToastLevel::Info,
                    });
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
                    started_at: run.started_at,
                });
            }
        }
    }

    /// Sync agent state from the ECS world (in-process agents only).
    fn sync_agent_state_from_world(&mut self, engine: &AgentEngine) {
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
            }
            if let Some(window) = engine.world().get::<ContextWindow>(agent.entity) {
                agent.context_tokens = (window.current_tokens, window.max_tokens);
            }
        }
    }

    /// Kill + delete all on-disk state for the selected agent.
    fn delete_selected_agent(&mut self) {
        if let Some(agent) = self.agents.get(self.selected) {
            if !agent.is_run_state {
                self.add_log("Can only delete background run-state agents".to_string());
                return;
            }
            let id = agent.id.clone();
            let pid = agent.pid;

            // Kill the worker process first if it is still running
            #[cfg(unix)]
            if pid > 0 {
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
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
            self.agents.remove(self.selected);
            if self.selected > 0 && self.selected >= self.agents.len() {
                self.selected = self.agents.len().saturating_sub(1);
            }
            self.table_state.select(Some(self.selected));
        }
    }

    fn submit_input(&mut self) {
        use interaction::{ApprovalScope, InteractionKind, InteractionResponse};

        let (agent_id, is_run_state, req) = match self.agents.get(self.selected) {
            Some(a) => (a.id.clone(), a.is_run_state, a.pending_request.clone()),
            None => return,
        };

        let (resp, display) = match &req {
            Some(r) => match r.kind {
                InteractionKind::FreeText => {
                    let raw = self.input_buffer.trim().to_string();
                    let input = if raw == "/quit" || raw == "/exit" {
                        String::new()
                    } else {
                        raw
                    };
                    let d = if input.is_empty() { "(end)".to_string() } else { truncate(&input, 40) };
                    (InteractionResponse::text(&r.id, &input), d)
                }
                InteractionKind::MultipleChoice => {
                    let idx = self.choice_selected;
                    let label = r.options.get(idx).cloned().unwrap_or_else(|| idx.to_string());
                    let d = truncate(&label, 40);
                    (InteractionResponse::choice(&r.id, idx), d)
                }
                InteractionKind::ToolApproval => {
                    let idx = self.choice_selected;
                    let label = r.options.get(idx).cloned().unwrap_or_else(|| idx.to_string());
                    let d = truncate(&label, 40);
                    let (approved, scope) = match idx {
                        0 => (true, ApprovalScope::Once),
                        1 => (true, ApprovalScope::Session),
                        _ => (false, ApprovalScope::Once),
                    };
                    (InteractionResponse::approval(&r.id, approved, scope), d)
                }
                InteractionKind::Confirm => {
                    let approved = self.choice_selected == 0;
                    let label = if approved { "Yes" } else { "No" };
                    (InteractionResponse::approval(&r.id, approved, ApprovalScope::Once), label.to_string())
                }
            },
            None => {
                let raw = self.input_buffer.trim().to_string();
                let input = if raw == "/quit" || raw == "/exit" {
                    String::new()
                } else {
                    raw
                };
                let d = if input.is_empty() { "(end)".to_string() } else { truncate(&input, 40) };
                (InteractionResponse {
                    request_id: String::new(),
                    value: Some(input),
                    choice_index: None,
                    approved: None,
                    scope: None,
                }, d)
            }
        };

        self.input_mode = false;
        self.input_buffer.clear();
        self.choice_selected = 0;

        let answered_id = resp.request_id.clone();
        if let Some(a) = self.agents.get_mut(self.selected) {
            a.last_answered_request_id = if answered_id.is_empty() { None } else { Some(answered_id) };
            a.waiting_prompt = None;
            a.pending_request = None;
            a.status = AgentDisplayStatus::Active;
        }

        if is_run_state {
            match interaction::write_response(&agent_id, &resp) {
                Ok(()) => self.add_log(format!("Sent: {}", display)),
                Err(e) => self.add_log(format!("Failed to send response: {}", e)),
            }
        } else {
            let input_text = resp.value
                .or_else(|| resp.choice_index.map(|i| i.to_string()))
                .unwrap_or_default();
            let _ = self.cmd_tx.send(EngineCommand::SendInput {
                agent_id: agent_id.clone(),
                input: input_text,
            });
            self.add_log(format!("Sent: {}", display));
        }
    }

    fn process_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                AgentEvent::StageChanged { agent_id, stage } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        agent.stage = stage.clone();
                    }
                    self.add_log(format!("{}: Stage -> {}", agent_id, stage));
                }
                AgentEvent::StatusChanged { agent_id, status } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        agent.status = status;
                    }
                }
                AgentEvent::NeedsInput { agent_id, prompt } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        agent.status = AgentDisplayStatus::Waiting;
                        agent.waiting_prompt = Some(prompt.clone());
                    }
                    self.add_log(format!("{}: Waiting for input", agent_id));
                }
                AgentEvent::ToolCalled { agent_id, tool, args } => {
                    self.add_log(format!("{}: Tool {}({})", agent_id, tool, truncate(&args, 40)));
                }
                AgentEvent::InferenceComplete { agent_id, content, tokens_used, tokens_prompt } => {
                    if let Some(_agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        _agent.iteration += 1;
                    }
                    self.add_log(format!(
                        "{}: Inference done ({}tok in, {}tok out) {}",
                        agent_id, tokens_prompt, tokens_used, truncate(&content, 60)
                    ));
                }
                AgentEvent::Error { agent_id, error } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        agent.status = AgentDisplayStatus::Error(error.clone());
                    }
                    self.add_log(format!("{}: ERROR: {}", agent_id, error));
                }
                AgentEvent::Log(msg) => {
                    self.add_log(msg);
                }
                AgentEvent::AgentDone { agent_id } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        if !matches!(agent.status, AgentDisplayStatus::Error(_) | AgentDisplayStatus::Cancelled) {
                            agent.status = AgentDisplayStatus::Complete;
                        }
                    }
                    self.add_log(format!("{}: Done", agent_id));
                }
            }
        }
    }

    /// True if the currently selected stage tab is the one actively accepting input.
    fn selected_stage_can_respond(&self) -> bool {
        let agent = match self.agents.get(self.selected) {
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
                agent.stages.iter().position(|s| s.name == req.stage_name)
                    .unwrap_or(agent.stage_index)
            } else {
                agent.stage_index
            }
        } else {
            agent.stage_index
        };
        self.selected_stage == input_stage_idx
    }

    fn handle_key(&mut self, key: KeyCode) {
        // Help overlay takes priority
        if self.show_help {
            self.show_help = false;
            return;
        }

        // Delete confirmation popup has highest priority
        if self.confirm_delete {
            match key {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.confirm_delete = false;
                    self.delete_selected_agent();
                }
                _ => {
                    self.confirm_delete = false;
                    self.add_log("Delete cancelled".to_string());
                }
            }
            return;
        }

        // ── Detail view ─────────────────────────────────────────────────────
        if self.detail_view {
            if self.input_mode {
                use interaction::InteractionKind;
                let kind = self.agents.get(self.selected)
                    .and_then(|a| a.pending_request.as_ref())
                    .map(|r| r.kind.clone());
                let options_len = self.agents.get(self.selected)
                    .and_then(|a| a.pending_request.as_ref())
                    .map(|r| r.options.len())
                    .unwrap_or(0);

                match key {
                    KeyCode::Esc => {
                        self.input_mode = false;
                        self.input_buffer.clear();
                        self.choice_selected = 0;
                    }
                    KeyCode::Enter => {
                        self.submit_input();
                    }
                    KeyCode::Up => {
                        if !matches!(kind, Some(InteractionKind::FreeText) | None) && self.choice_selected > 0 {
                            self.choice_selected -= 1;
                        }
                    }
                    KeyCode::Down => {
                        if !matches!(kind, Some(InteractionKind::FreeText) | None) && options_len > 0 && self.choice_selected < options_len - 1 {
                            self.choice_selected += 1;
                        }
                    }
                    KeyCode::Char(c) if matches!(kind, Some(InteractionKind::FreeText) | None) => {
                        self.input_buffer.push(c);
                    }
                    KeyCode::Backspace if matches!(kind, Some(InteractionKind::FreeText) | None) => {
                        self.input_buffer.pop();
                    }
                    _ => {}
                }
                return;
            }

            // Search mode: intercept all keys for query editing
            if self.search_mode {
                match key {
                    KeyCode::Esc | KeyCode::Enter => {
                        if key == KeyCode::Esc {
                            self.search_query.clear();
                            self.search_match_idx = 0;
                        }
                        self.search_mode = false;
                    }
                    KeyCode::Backspace => {
                        self.search_query.pop();
                        self.search_match_idx = 0;
                    }
                    KeyCode::Char(c) => {
                        self.search_query.push(c);
                        self.search_match_idx = 0;
                    }
                    _ => {}
                }
                return;
            }

            // Detail view — not in input mode
            match key {
                KeyCode::Esc => {
                    if !self.search_query.is_empty() {
                        // First Esc clears the search; second exits detail view
                        self.search_query.clear();
                        self.search_match_idx = 0;
                    } else {
                        self.detail_view = false;
                        self.detail_scroll = 0;
                        self.review_scroll = 0;
                    }
                }
                // Stage tab navigation
                KeyCode::Left => {
                    if self.selected_stage > 0 {
                        self.selected_stage -= 1;
                        self.detail_scroll = 0;
                        self.review_scroll = 0;
                        self.stage_content_mode = StageContentMode::Output;
                    }
                }
                KeyCode::Right => {
                    let max_stage = self.agents.get(self.selected)
                        .map(|a| a.num_stages.saturating_sub(1))
                        .unwrap_or(0);
                    if self.selected_stage < max_stage {
                        self.selected_stage += 1;
                        self.detail_scroll = 0;
                        self.review_scroll = 0;
                        self.stage_content_mode = StageContentMode::Output;
                    }
                }
                // Number keys 1-9: jump to stage tab
                KeyCode::Char(c @ '1'..='9') => {
                    let idx = (c as usize) - ('1' as usize);
                    let max_stage = self.agents.get(self.selected)
                        .map(|a| a.num_stages.saturating_sub(1))
                        .unwrap_or(0);
                    if idx <= max_stage {
                        self.selected_stage = idx;
                        self.detail_scroll = 0;
                        self.review_scroll = 0;
                        self.stage_content_mode = StageContentMode::Output;
                    }
                }
                // Content mode toggle
                KeyCode::Char('l') => {
                    self.stage_content_mode = StageContentMode::Logs;
                    self.detail_scroll = 0;
                }
                KeyCode::Char('o') => {
                    self.stage_content_mode = StageContentMode::Output;
                    self.detail_scroll = 0;
                }
                KeyCode::Char('i') => {
                    if self.selected_stage_can_respond() {
                        self.input_mode = true;
                        self.choice_selected = 0;
                        self.input_buffer.clear();
                    }
                }
                KeyCode::Up => {
                    // When a review body is present, Up scrolls the review document
                    let has_review = self.agents.get(self.selected)
                        .and_then(|a| a.pending_request.as_ref())
                        .and_then(|r| r.body.as_deref())
                        .map(|b| !b.is_empty())
                        .unwrap_or(false);
                    if has_review {
                        self.review_scroll = self.review_scroll.saturating_add(1);
                    } else {
                        self.detail_scroll = self.detail_scroll.saturating_add(1);
                    }
                }
                KeyCode::Down => {
                    let has_review = self.agents.get(self.selected)
                        .and_then(|a| a.pending_request.as_ref())
                        .and_then(|r| r.body.as_deref())
                        .map(|b| !b.is_empty())
                        .unwrap_or(false);
                    if has_review {
                        self.review_scroll = self.review_scroll.saturating_sub(1);
                    } else {
                        self.detail_scroll = self.detail_scroll.saturating_sub(1);
                    }
                }
                KeyCode::PageUp => {
                    let has_review = self.agents.get(self.selected)
                        .and_then(|a| a.pending_request.as_ref())
                        .and_then(|r| r.body.as_deref())
                        .map(|b| !b.is_empty())
                        .unwrap_or(false);
                    if has_review {
                        self.review_scroll = self.review_scroll.saturating_add(10);
                    } else {
                        self.detail_scroll = self.detail_scroll.saturating_add(10);
                    }
                }
                KeyCode::PageDown => {
                    let has_review = self.agents.get(self.selected)
                        .and_then(|a| a.pending_request.as_ref())
                        .and_then(|r| r.body.as_deref())
                        .map(|b| !b.is_empty())
                        .unwrap_or(false);
                    if has_review {
                        self.review_scroll = self.review_scroll.saturating_sub(10);
                    } else {
                        self.detail_scroll = self.detail_scroll.saturating_sub(10);
                    }
                }
                KeyCode::Char('b') => {
                    self.detail_scroll = usize::MAX;
                    self.review_scroll = usize::MAX;
                }
                KeyCode::Char('e') => {
                    self.detail_scroll = 0;
                    self.review_scroll = 0;
                }
                KeyCode::Char('?') => {
                    self.show_help = true;
                }
                // Search: `/` enters search mode; `n`/`N` step through matches
                KeyCode::Char('/') => {
                    self.search_mode = true;
                    self.search_query.clear();
                    self.search_match_idx = 0;
                }
                KeyCode::Char('n') => {
                    if !self.search_query.is_empty() {
                        self.search_match_idx = self.search_match_idx.saturating_add(1);
                        // Clamp is done during render; we set a large value so the
                        // render pass will clamp it to the last match.
                    }
                }
                KeyCode::Char('N') => {
                    if !self.search_query.is_empty() {
                        self.search_match_idx = self.search_match_idx.saturating_sub(1);
                    }
                }
                // Yank: `y` copies the current stage output to clipboard via OSC52
                KeyCode::Char('y') => {
                    if let Some(agent) = self.agents.get(self.selected) {
                        if agent.is_run_state {
                            let is_output = self.stage_content_mode == StageContentMode::Output;
                            let content = if is_output {
                                runstate::tail_stage_output(&agent.id, self.selected_stage, 524_288)
                            } else {
                                runstate::tail_stage_log(&agent.id, self.selected_stage, 524_288)
                            };
                            osc52_yank(&content);
                            self.add_log(format!(
                                "Yanked {} to clipboard ({} bytes)",
                                if is_output { "output" } else { "logs" },
                                content.len()
                            ));
                        }
                    }
                }
                KeyCode::Char('k') => {
                    if let Some(agent) = self.agents.get(self.selected) {
                        if matches!(agent.status, AgentDisplayStatus::Active | AgentDisplayStatus::Waiting) {
                            let agent_id = agent.id.clone();
                            let pid = agent.pid;
                            let is_run_state = agent.is_run_state;
                            let was_waiting = matches!(agent.status, AgentDisplayStatus::Waiting);
                            if is_run_state {
                                #[cfg(unix)]
                                if pid > 0 {
                                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
                                }
                                kill_write_cancelled(&agent_id);
                                if was_waiting {
                                    interaction::clear_interaction(&agent_id);
                                }
                            } else {
                                let _ = self.cmd_tx.send(EngineCommand::CancelAgent { agent_id: agent_id.clone() });
                            }
                            if let Some(a) = self.agents.get_mut(self.selected) {
                                a.status = AgentDisplayStatus::Cancelled;
                                a.waiting_prompt = None;
                                a.pending_request = None;
                            }
                            self.input_mode = false;
                            self.input_buffer.clear();
                            self.add_log(format!("{}: Killed", agent_id));
                        }
                    }
                }
                _ => {}
            }
            return;
        }

        // ── Main agent list ──────────────────────────────────────────────────
        match key {
            KeyCode::Esc => {
                self.should_quit = true;
            }
            KeyCode::Up => {
                if !self.agents.is_empty() && self.selected > 0 {
                    self.selected -= 1;
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::Down => {
                if !self.agents.is_empty() && self.selected < self.agents.len() - 1 {
                    self.selected += 1;
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::Enter => {
                if !self.agents.is_empty() {
                    self.detail_view = true;
                    self.detail_scroll = 0;
                    // Default to the currently active stage when opening detail view
                    self.selected_stage = self.agents.get(self.selected)
                        .map(|a| a.stage_index)
                        .unwrap_or(0);
                    self.stage_content_mode = StageContentMode::Output;
                }
            }
            KeyCode::Char('d') => {
                if let Some(agent) = self.agents.get(self.selected) {
                    if agent.is_run_state {
                        self.confirm_delete = true;
                        self.add_log(format!(
                            "Delete run '{}'? This kills the process and is PERMANENT. (y/n)",
                            agent.id
                        ));
                    } else {
                        self.add_log("Only background runs can be deleted from the dashboard".to_string());
                    }
                }
            }
            KeyCode::Char('c') => {
                if let Some(agent) = self.agents.get(self.selected) {
                    if matches!(agent.status, AgentDisplayStatus::Active | AgentDisplayStatus::Waiting) {
                        let agent_id = agent.id.clone();
                        if agent.is_run_state {
                            #[cfg(unix)]
                            if agent.pid > 0 {
                                unsafe { libc::kill(agent.pid as libc::pid_t, libc::SIGTERM); }
                            }
                            kill_write_cancelled(&agent_id);
                            if matches!(agent.status, AgentDisplayStatus::Waiting) {
                                interaction::clear_interaction(&agent_id);
                            }
                            if let Some(a) = self.agents.get_mut(self.selected) {
                                a.status = AgentDisplayStatus::Cancelled;
                                a.waiting_prompt = None;
                                a.pending_request = None;
                            }
                        } else {
                            let _ = self.cmd_tx.send(EngineCommand::CancelAgent { agent_id: agent_id.clone() });
                        }
                        self.add_log(format!("{}: Cancel requested", agent_id));
                    }
                }
            }
            KeyCode::Char('k') => {
                if let Some(agent) = self.agents.get(self.selected) {
                    if matches!(agent.status, AgentDisplayStatus::Active | AgentDisplayStatus::Waiting) {
                        let agent_id = agent.id.clone();
                        if agent.is_run_state {
                            #[cfg(unix)]
                            if agent.pid > 0 {
                                unsafe { libc::kill(agent.pid as libc::pid_t, libc::SIGTERM); }
                            }
                            kill_write_cancelled(&agent_id);
                            if matches!(agent.status, AgentDisplayStatus::Waiting) {
                                interaction::clear_interaction(&agent_id);
                            }
                        } else {
                            let _ = self.cmd_tx.send(EngineCommand::CancelAgent { agent_id: agent_id.clone() });
                        }
                        if let Some(a) = self.agents.get_mut(self.selected) {
                            a.status = AgentDisplayStatus::Cancelled;
                            a.waiting_prompt = None;
                            a.pending_request = None;
                        }
                        self.add_log(format!("{}: Killed", agent_id));
                    }
                }
            }
            KeyCode::Char('?') => {
                self.show_help = true;
            }
            _ => {}
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) {
        // Mouse support: scroll wheel in detail view
        if self.detail_view && !self.input_mode {
            match event.kind {
                MouseEventKind::ScrollUp => {
                    self.detail_scroll = self.detail_scroll.saturating_add(3);
                }
                MouseEventKind::ScrollDown => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(3);
                }
                _ => {}
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        if self.detail_view {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(frame.area());
            self.draw_detail_panel(frame, chunks[0]);
            self.draw_help_bar(frame, chunks[1]);
        } else {
            // Normal layout: agent table + log panel + help bar
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Percentage(55),
                    Constraint::Percentage(44),
                    Constraint::Length(1),
                ])
                .split(frame.area());
            self.draw_agent_table(frame, chunks[0]);
            self.draw_log_panel(frame, chunks[1]);
            self.draw_help_bar(frame, chunks[2]);
        }

        // Render toasts (top-right overlay)
        self.draw_toasts(frame);

        // Help overlay
        if self.show_help {
            self.draw_help_overlay(frame);
        }

        // Delete confirmation popup
        if self.confirm_delete {
            self.draw_confirm_popup(frame);
        }
    }

    fn draw_toasts(&self, frame: &mut Frame) {
        if self.toasts.is_empty() { return; }
        let area = frame.area();
        let toast_w: u16 = 40;
        let toast_h: u16 = self.toasts.len() as u16;
        let x = area.width.saturating_sub(toast_w + 1);
        let y: u16 = 1;
        let toast_area = Rect { x, y, width: toast_w, height: toast_h };
        frame.render_widget(Clear, toast_area);
        for (i, toast) in self.toasts.iter().enumerate() {
            let color = match toast.level {
                ToastLevel::Info => C_SUCCESS,
                ToastLevel::Warning => C_WARN,
                ToastLevel::Error => C_ERROR,
            };
            let icon = match toast.level {
                ToastLevel::Info => "✓",
                ToastLevel::Warning => "⏸",
                ToastLevel::Error => "✗",
            };
            let msg = truncate(&toast.message, (toast_w - 4) as usize);
            let line = Line::from(vec![
                Span::styled(format!(" {} ", icon), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(msg, Style::default().fg(C_WHITE)),
            ]);
            let row = Rect { x, y: y + i as u16, width: toast_w, height: 1 };
            frame.render_widget(
                Paragraph::new(line).style(Style::default().bg(Color::Rgb(30, 30, 30))),
                row,
            );
        }
    }

    fn draw_help_overlay(&self, frame: &mut Frame) {
        let area = frame.area();
        let w: u16 = 62.min(area.width.saturating_sub(4));
        let h: u16 = 32.min(area.height.saturating_sub(4));
        let x = (area.width.saturating_sub(w)) / 2;
        let y = (area.height.saturating_sub(h)) / 2;
        let popup = Rect { x, y, width: w, height: h };
        frame.render_widget(Clear, popup);

        let lines: Vec<Line> = vec![
            Line::from(Span::styled("  Main list", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))),
            Line::from(""),
            Line::from(vec![Span::styled("  ↑/↓      ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Select agent (sorted: active first)")]),
            Line::from(vec![Span::styled("  Enter    ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Open detail view")]),
            Line::from(vec![Span::styled("  d        ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Delete run (permanent)")]),
            Line::from(vec![Span::styled("  c / k    ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Cancel / Kill agent")]),
            Line::from(vec![Span::styled("  Esc      ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Quit")]),
            Line::from(""),
            Line::from(Span::styled("  Detail view", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))),
            Line::from(""),
            Line::from(vec![Span::styled("  ←/→      ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Switch stage tab")]),
            Line::from(vec![Span::styled("  1-9      ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Jump to stage by number")]),
            Line::from(vec![Span::styled("  ↑/↓      ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Scroll output (review doc when shown)")]),
            Line::from(vec![Span::styled("  PgUp/Dn  ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Scroll 10 lines")]),
            Line::from(vec![Span::styled("  b / e    ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Jump to begin / end")]),
            Line::from(vec![Span::styled("  l / o    ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Toggle Logs / Output")]),
            Line::from(vec![Span::styled("  /        ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Search output/logs")]),
            Line::from(vec![Span::styled("  n / N    ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Next / previous search match")]),
            Line::from(vec![Span::styled("  y        ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Yank output/logs to clipboard (OSC52)")]),
            Line::from(vec![Span::styled("  i        ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Respond (when input needed)")]),
            Line::from(vec![Span::styled("  k        ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Kill agent")]),
            Line::from(vec![Span::styled("  Esc      ", Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)), Span::raw("Clear search / back to list")]),
            Line::from(""),
            Line::from(Span::styled("  Any key to dismiss", Style::default().fg(C_DIM))),
        ];

        let widget = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(C_ACCENT))
                    .title(Span::styled(" Help  ? ", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)))
                    .padding(Padding::uniform(0)),
            );
        frame.render_widget(widget, popup);
    }

    fn draw_confirm_popup(&self, frame: &mut Frame) {
        let area = frame.area();
        let w: u16 = 56.min(area.width.saturating_sub(4));
        let h: u16 = 5;
        let x = (area.width.saturating_sub(w)) / 2;
        let y = (area.height.saturating_sub(h)) / 2;
        let popup = Rect { x, y, width: w, height: h };
        frame.render_widget(Clear, popup);

        let agent_id = self.agents.get(self.selected).map(|a| a.id.as_str()).unwrap_or("?");
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("  Delete run '{}'?  This is permanent.", truncate(agent_id, 24)),
                Style::default().fg(C_WARN),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  [y]", Style::default().fg(C_ERROR).add_modifier(Modifier::BOLD)),
                Span::raw(" confirm  "),
                Span::styled("[any key]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" cancel"),
            ]),
        ];
        let widget = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(C_ERROR))
                    .title(Span::styled(" Confirm Delete ", Style::default().fg(C_ERROR).add_modifier(Modifier::BOLD))),
            );
        frame.render_widget(widget, popup);
    }

    fn draw_agent_table(&mut self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            Cell::from(Span::styled("Title / ID", Style::default().add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Agent", Style::default().add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Stage", Style::default().add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Status", Style::default().add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Tokens", Style::default().add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Started", Style::default().add_modifier(Modifier::BOLD))),
        ])
        .style(Style::default().fg(C_MUTED))
        .height(1);

        let spinner_frame = SPINNER[(self.tick_count as usize) % SPINNER.len()];

        // Sort agents: Active/Waiting first, then Complete/CompleteInteractive,
        // then Error, then Cancelled/Idle — within each group, newest first.
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
        let mut sorted_indices: Vec<usize> = (0..self.agents.len()).collect();
        sorted_indices.sort_by(|&a, &b| {
            let pa = status_priority(&self.agents[a].status);
            let pb = status_priority(&self.agents[b].status);
            pa.cmp(&pb).then(self.agents[b].started_at.cmp(&self.agents[a].started_at))
        });

        let rows: Vec<Row> = sorted_indices
            .iter()
            .map(|&idx| {
                let agent = &self.agents[idx];
                let status_color = agent.status.color();
                let started_str = relative_time(agent.started_at);
                let title_str = agent
                    .title
                    .as_deref()
                    .map(|t| truncate(t, 26))
                    .unwrap_or_else(|| truncate(&agent.task, 26));
                let tok_str = if agent.is_run_state {
                    if agent.tokens_in == 0 && agent.tokens_out == 0 {
                        "—".to_string()
                    } else {
                        format!("{}↑ {}↓", format_tokens(agent.tokens_in), format_tokens(agent.tokens_out))
                    }
                } else {
                    let (cur, max) = agent.context_tokens;
                    if max > 0 {
                        format!("{}/{}", format_tokens(cur), format_tokens(max))
                    } else {
                        format_tokens(cur)
                    }
                };
                // Stage progress: "plan 2/3"
                let stage_str = if agent.num_stages > 1 {
                    format!("{} {}/{}", truncate(&agent.stage, 10), agent.stage_index + 1, agent.num_stages)
                } else {
                    truncate(&agent.stage, 14)
                };
                // Status with spinner for active
                let status_str = if matches!(agent.status, AgentDisplayStatus::Active) {
                    format!("{} ACTIVE", spinner_frame)
                } else {
                    agent.status.to_string()
                };
                Row::new(vec![
                    Cell::from(title_str),
                    Cell::from(agent.blueprint_name.clone()),
                    Cell::from(stage_str),
                    Cell::from(status_str).style(Style::default().fg(status_color)),
                    Cell::from(tok_str),
                    Cell::from(started_str).style(Style::default().fg(C_DIM)),
                ])
            })
            .collect();

        let empty_state_msg = if self.agents.is_empty() {
            Some("  No agents running. Use `lev run <agent> --task \"...\"` to start one.")
        } else {
            None
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(C_BORDER))
            .title(Span::styled(" Agents ", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)));

        if let Some(msg) = empty_state_msg {
            let widget = Paragraph::new(Line::from(Span::styled(msg, Style::default().fg(C_DIM))))
                .block(block);
            frame.render_widget(widget, area);
            return;
        }

        let table = Table::new(
            rows,
            [
                Constraint::Percentage(22),
                Constraint::Percentage(12),
                Constraint::Percentage(14),
                Constraint::Percentage(18),
                Constraint::Percentage(14),
                Constraint::Percentage(20),
            ],
        )
        .header(header)
        .block(block)
        .row_highlight_style(
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .fg(C_WHITE),
        );

        frame.render_stateful_widget(table, area, &mut self.table_state);
    }

    fn draw_detail_panel(&mut self, frame: &mut Frame, area: Rect) {
        use interaction::InteractionKind;

        let agent = match self.agents.get(self.selected) {
            Some(a) => a.clone(),
            None => {
                let msg = Paragraph::new("No agent selected.")
                    .block(Block::default().borders(Borders::ALL).title(" Detail "));
                frame.render_widget(msg, area);
                return;
            }
        };

        // Clamp selected_stage to valid range
        let max_tab = agent.num_stages.saturating_sub(1);
        if self.selected_stage > max_tab {
            self.selected_stage = max_tab;
        }

        // ── Layout: header + tabs + context bar + content + [input] ──────────
        let is_waiting = matches!(agent.status, AgentDisplayStatus::Waiting | AgentDisplayStatus::CompleteInteractive);
        let pending_req = agent.pending_request.clone();
        let kind = pending_req.as_ref().map(|r| r.kind.clone());
        let options: Vec<String> = pending_req.as_ref().map(|r| r.options.clone()).unwrap_or_default();

        // Only show input pane on the tab that can actually respond
        let has_prompt = is_waiting
            && (pending_req.is_some() || agent.waiting_prompt.is_some())
            && !matches!(agent.status, AgentDisplayStatus::Cancelled)
            && self.selected_stage_can_respond();

        let header_h: u16 = 1;   // compact breadcrumb line
        let tabs_h: u16 = 3;     // tabs row in a block (border top + tab line + border bottom)
        let context_h: u16 = if agent.context_snapshot.is_some() || !agent.stages.is_empty() { 3 } else { 0 };

        // Review body: shown when the pending interaction carries markdown for review
        let review_body = if !self.input_mode && has_prompt {
            pending_req.as_ref().and_then(|r| r.body.as_deref())
        } else {
            None
        };
        // Pre-render the markdown so we know how many lines it produces
        let review_lines: Vec<Line<'static>> = if let Some(body) = review_body {
            let w = area.width.saturating_sub(4);
            render::markdown_to_text(body, w).lines
        } else {
            Vec::new()
        };
        let review_h: u16 = if review_lines.is_empty() {
            0
        } else {
            // Allocate up to 40% of the panel height, minimum 8 lines + 2 border
            let max_review = (area.height as usize * 2 / 5).clamp(10, 24);
            (review_lines.len() + 2).min(max_review) as u16
        };

        let prompt_height: u16 = if has_prompt || (self.input_mode && is_waiting && self.selected_stage_can_respond()) {
            let n = options.len() as u16;
            if self.input_mode {
                match &kind {
                    Some(InteractionKind::FreeText) | None => 7,
                    _ => (n + 4).min(14),
                }
            } else {
                match &kind {
                    Some(InteractionKind::FreeText) | None => 6,
                    _ => (n + 5).min(14),
                }
            }
        } else {
            0
        };

        let mut constraints = vec![
            Constraint::Length(header_h),
            Constraint::Length(tabs_h),
        ];
        if context_h > 0 {
            constraints.push(Constraint::Length(context_h));
        }
        constraints.push(Constraint::Min(4)); // content pane
        if review_h > 0 {
            constraints.push(Constraint::Length(review_h));
        }
        if prompt_height > 0 {
            constraints.push(Constraint::Length(prompt_height));
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let mut chunk_idx = 0;

        // ── Header breadcrumb ─────────────────────────────────────────────────
        {
            let hdr_area = chunks[chunk_idx]; chunk_idx += 1;
            let elapsed = elapsed_str(agent.started_at);
            let status_color = agent.status.color();
            let spinner_frame = SPINNER[(self.tick_count as usize) % SPINNER.len()];
            let status_span = match &agent.status {
                AgentDisplayStatus::Active => Span::styled(
                    format!("{} {} ", spinner_frame, agent.status),
                    Style::default().fg(status_color).add_modifier(Modifier::BOLD),
                ),
                _ => Span::styled(
                    format!("{} ", agent.status),
                    Style::default().fg(status_color).add_modifier(Modifier::BOLD),
                ),
            };
            let title_text = agent.title.as_deref().unwrap_or(&agent.blueprint_name);
            let hdr_line = Line::from(vec![
                Span::styled(format!(" {} ", truncate(title_text, 28)), Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)),
                Span::styled("· ", Style::default().fg(C_DIM)),
                status_span,
                Span::styled("· ", Style::default().fg(C_DIM)),
                Span::styled(format!("{}↑", format_tokens(agent.tokens_in)), Style::default().fg(C_DIM)),
                Span::styled(format!(" {}↓", format_tokens(agent.tokens_out)), Style::default().fg(C_DIM)),
                Span::styled(format!(" · {} ", elapsed), Style::default().fg(C_DIM)),
            ]);
            frame.render_widget(Paragraph::new(hdr_line).style(Style::default().bg(Color::Rgb(20, 20, 30))), hdr_area);
        }

        // ── Stage tabs ─────────────────────────────────────────────────────────
        {
            let tabs_area = chunks[chunk_idx]; chunk_idx += 1;

            // Build tab titles with status glyphs
            let tab_titles: Vec<Line> = if agent.stages.is_empty() {
                // Fallback: synthesize stage names from RunMeta info
                (0..agent.num_stages.max(1)).map(|i| {
                    let glyph = if i < agent.stage_index {
                        Span::styled(format!("{} ", GLYPH_COMPLETE), Style::default().fg(C_SUCCESS))
                    } else if i == agent.stage_index {
                        match &agent.status {
                            AgentDisplayStatus::Active => Span::styled(
                                format!("{} ", SPINNER[(self.tick_count as usize) % SPINNER.len()]),
                                Style::default().fg(C_ACTIVE),
                            ),
                            AgentDisplayStatus::Waiting => Span::styled(format!("{} ", GLYPH_WAITING), Style::default().fg(C_WARN)),
                            AgentDisplayStatus::Error(_) => Span::styled(format!("{} ", GLYPH_ERROR), Style::default().fg(C_ERROR)),
                            _ => Span::styled(format!("{} ", GLYPH_COMPLETE), Style::default().fg(C_SUCCESS)),
                        }
                    } else {
                        Span::styled(format!("{} ", GLYPH_PENDING), Style::default().fg(C_DIM))
                    };
                    let stage_label = if i == agent.stage_index {
                        truncate(&agent.stage, 12)
                    } else {
                        format!("stage {}", i + 1)
                    };
                    let label_span = if i == agent.stage_index {
                        Span::styled(stage_label, Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD))
                    } else {
                        Span::styled(stage_label, Style::default().fg(C_MUTED))
                    };
                    // Live stage marker
                    let live_marker = if i == agent.stage_index && !matches!(agent.status, AgentDisplayStatus::Complete | AgentDisplayStatus::CompleteInteractive | AgentDisplayStatus::Cancelled | AgentDisplayStatus::Error(_)) {
                        Span::styled("*", Style::default().fg(C_WARN))
                    } else {
                        Span::raw("")
                    };
                    Line::from(vec![glyph, label_span, live_marker])
                }).collect()
            } else {
                agent.stages.iter().enumerate().map(|(i, s)| {
                    // Compute stage duration string
                    let dur_str = match (s.started_at, s.ended_at) {
                        (Some(start), Some(end)) => {
                            let secs = (end - start).max(0) as u64;
                            if secs < 60 { format!(" {}s", secs) }
                            else { format!(" {}m{}s", secs / 60, secs % 60) }
                        }
                        (Some(start), None) if s.status == StageRunStatus::Active => {
                            // Show live elapsed for the active stage
                            elapsed_str(start)
                                .chars().next()
                                .map(|_| format!(" {}", elapsed_str(start)))
                                .unwrap_or_default()
                        }
                        _ => String::new(),
                    };

                    let (glyph, glyph_style) = match &s.status {
                        StageRunStatus::Pending => (GLYPH_PENDING, Style::default().fg(C_DIM)),
                        StageRunStatus::Active => {
                            let spin = SPINNER[(self.tick_count as usize) % SPINNER.len()];
                            return Line::from(vec![
                                Span::styled(format!("{} ", spin), Style::default().fg(C_ACTIVE)),
                                Span::styled(truncate(&s.name, 10), Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)),
                                Span::styled("*", Style::default().fg(C_WARN)),
                                Span::styled(dur_str, Style::default().fg(C_DIM)),
                            ]);
                        }
                        StageRunStatus::WaitingInput => (GLYPH_WAITING, Style::default().fg(C_WARN)),
                        StageRunStatus::Complete => (GLYPH_COMPLETE, Style::default().fg(C_SUCCESS)),
                        StageRunStatus::Error => (GLYPH_ERROR, Style::default().fg(C_ERROR)),
                    };
                    let is_live = i == agent.stage_index
                        && !matches!(agent.status, AgentDisplayStatus::Complete | AgentDisplayStatus::CompleteInteractive | AgentDisplayStatus::Cancelled | AgentDisplayStatus::Error(_));
                    let label_style = if is_live {
                        Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(C_MUTED)
                    };
                    Line::from(vec![
                        Span::styled(format!("{} ", glyph), glyph_style),
                        Span::styled(truncate(&s.name, 10), label_style),
                        Span::styled(dur_str, Style::default().fg(C_DIM)),
                    ])
                }).collect()
            };

            let tabs_count = tab_titles.len().max(1);
            let selected_tab = self.selected_stage.min(tabs_count - 1);

            let tab_nav = if tabs_count > 1 {
                format!(" ←/→ to switch  stage {}/{}", selected_tab + 1, tabs_count)
            } else {
                " stage 1/1".to_string()
            };

            let tabs_widget = Tabs::new(tab_titles)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(C_BORDER_FOCUS))
                        .title(Span::styled(
                            format!(" Stages{}", tab_nav),
                            Style::default().fg(C_DIM),
                        )),
                )
                .select(selected_tab)
                .highlight_style(
                    Style::default()
                        .fg(C_ACCENT)
                        .add_modifier(Modifier::BOLD | Modifier::REVERSED),
                )
                .divider(Span::styled(" │ ", Style::default().fg(C_DIM)));

            frame.render_widget(tabs_widget, tabs_area);
        }

        // ── Context bar for selected stage ────────────────────────────────────
        if context_h > 0 {
            let ctx_area = chunks[chunk_idx]; chunk_idx += 1;
            // Use per-stage context if available, else fall back to global snapshot
            let snap_opt = if agent.is_run_state {
                runstate::read_stage_context(&agent.id, self.selected_stage)
                    .or_else(|| agent.context_snapshot.clone())
            } else {
                agent.context_snapshot.clone()
            };

            if let Some(snap) = snap_opt {
                let inner_w = ctx_area.width.saturating_sub(4) as usize;
                let total_pct = if snap.max_tokens > 0 {
                    (snap.total_tokens * 100 / snap.max_tokens).min(100)
                } else { 0 };
                let bar_color = if total_pct >= 90 { C_ERROR } else if total_pct >= 70 { C_WARN } else { C_SUCCESS };

                // Build compact mini region line
                let bar_w = inner_w.saturating_sub(28).max(8);
                let filled = bar_w * total_pct / 100;
                let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_w - filled));

                // Region dots
                let regions_str: String = snap.regions.iter().take(5).map(|r| {
                    let kind_char = match r.kind.as_str() {
                        "pinned" => "P",
                        "sliding" => "S",
                        "compacting" | "history" => "H",
                        _ => "·",
                    };
                    kind_char
                }).collect::<Vec<_>>().join(" ");

                let ctx_line = Line::from(vec![
                    Span::styled(" ctx ", Style::default().fg(C_DIM)),
                    Span::styled(bar, Style::default().fg(bar_color)),
                    Span::styled(
                        format!(" {:>3}%  {}/{}", total_pct, format_tokens(snap.total_tokens), format_tokens(snap.max_tokens)),
                        Style::default().fg(C_DIM),
                    ),
                    Span::styled(format!("  [{}]", regions_str), Style::default().fg(C_DIM)),
                ]);

                frame.render_widget(
                    Paragraph::new(ctx_line)
                        .block(Block::default().borders(Borders::LEFT | Borders::RIGHT)
                            .border_style(Style::default().fg(C_BORDER))),
                    ctx_area,
                );
            } else {
                // No context yet for this stage
                let empty = Paragraph::new(Line::from(Span::styled(
                    " ctx  —  no context snapshot yet",
                    Style::default().fg(C_DIM),
                )));
                frame.render_widget(empty, ctx_area);
            }
        }

        // ── Content pane (Output or Logs) ─────────────────────────────────────
        {
            let content_area = chunks[chunk_idx]; chunk_idx += 1;
            let inner_h = content_area.height.saturating_sub(2) as usize;

            let is_output = self.stage_content_mode == StageContentMode::Output;

            // Read per-stage content
            let content = if agent.is_run_state {
                if is_output {
                    runstate::tail_stage_output(&agent.id, self.selected_stage, 131_072)
                } else {
                    runstate::tail_stage_log(&agent.id, self.selected_stage, 131_072)
                }
            } else {
                String::new()
            };

            // Render content: markdown for Output, color-coded for Logs
            let render_width = content_area.width.saturating_sub(2);
            let all_lines: Vec<Line> = if is_output && !content.is_empty() {
                // Run agent output through the markdown renderer — degrades cleanly
                // for plain text while making markdown-formatted output beautiful.
                let rendered = render::markdown_to_text(&content, render_width);
                rendered.lines
            } else if !is_output {
                // Logs: color-code prefixes
                content.lines()
                    .map(|l| {
                        let (color, prefix_end) = if l.starts_with("[tool]") {
                            (C_ACCENT, 6)
                        } else if l.starts_with("[error]") {
                            (C_ERROR, 7)
                        } else if l.starts_with("[denied]") {
                            (C_WARN, 8)
                        } else if l.starts_with("---") || l.starts_with("[All") {
                            (C_DIM, 0)
                        } else {
                            (C_MUTED, 0)
                        };
                        if prefix_end > 0 && l.len() > prefix_end {
                            Line::from(vec![
                                Span::styled(format!(" {}", &l[..prefix_end]), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                                Span::styled(l[prefix_end..].to_string(), Style::default().fg(C_MUTED)),
                            ])
                        } else {
                            Line::from(Span::styled(format!(" {}", l), Style::default().fg(color)))
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };

            // ── Error / Cancelled banner ─────────────────────────────────────
            let mut all_lines = all_lines;
            match &agent.status {
                AgentDisplayStatus::Error(msg) if !msg.is_empty() => {
                    all_lines.push(Line::from(vec![
                        Span::styled(" ✗ Error  ", Style::default().fg(Color::Black).bg(C_ERROR).add_modifier(Modifier::BOLD)),
                        Span::styled(format!(" {}", msg), Style::default().fg(C_ERROR)),
                    ]));
                }
                AgentDisplayStatus::Error(_) => {
                    all_lines.push(Line::from(Span::styled(
                        " ✗ Agent terminated with an error.",
                        Style::default().fg(C_ERROR),
                    )));
                }
                AgentDisplayStatus::Cancelled => {
                    all_lines.push(Line::from(Span::styled(
                        " ⊘ Run was cancelled.",
                        Style::default().fg(C_DIM),
                    )));
                }
                _ => {}
            }

            let total = all_lines.len();

            // ── Search: compute match indices + navigate ──────────────────────
            let query_lc = self.search_query.to_lowercase();
            let match_indices: Vec<usize> = if query_lc.is_empty() {
                Vec::new()
            } else {
                all_lines.iter().enumerate().filter_map(|(i, line)| {
                    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                    if text.to_lowercase().contains(&query_lc) { Some(i) } else { None }
                }).collect()
            };

            // Clamp search_match_idx to valid range and jump to current match
            if !match_indices.is_empty() {
                self.search_match_idx = self.search_match_idx.min(match_indices.len() - 1);
                let match_line = match_indices[self.search_match_idx];
                // Center the current match in the viewport
                let center_scroll = total.saturating_sub(match_line + inner_h / 2);
                self.detail_scroll = center_scroll;
            }

            let max_scroll = total.saturating_sub(inner_h);
            if self.detail_scroll > max_scroll {
                self.detail_scroll = max_scroll;
            }
            let start = total.saturating_sub(inner_h + self.detail_scroll);
            let end = (start + inner_h).min(total);

            let visible: Vec<Line> = if total == 0 {
                let stage_name = agent.stages.get(self.selected_stage)
                    .map(|s| s.name.as_str())
                    .unwrap_or("this stage");
                vec![Line::from(Span::styled(
                    format!(" No {} yet for {}.", if is_output { "output" } else { "logs" }, stage_name),
                    Style::default().fg(C_DIM),
                ))]
            } else {
                // Apply search highlighting
                let current_match_line = match_indices.get(self.search_match_idx).copied();
                all_lines[start..end].iter().enumerate().map(|(rel_idx, line)| {
                    let abs_idx = start + rel_idx;
                    let is_current_match = current_match_line == Some(abs_idx);
                    let is_any_match = !query_lc.is_empty() && match_indices.contains(&abs_idx);
                    if is_current_match {
                        // Current match: bright yellow background
                        Line::from(line.spans.iter().map(|s| {
                            Span::styled(s.content.clone(), Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD))
                        }).collect::<Vec<_>>())
                    } else if is_any_match {
                        // Other matches: dim yellow background
                        Line::from(line.spans.iter().map(|s| {
                            Span::styled(s.content.clone(), Style::default().fg(C_WHITE).bg(Color::Rgb(80, 60, 0)))
                        }).collect::<Vec<_>>())
                    } else {
                        line.clone()
                    }
                }).collect()
            };

            // Tool count badge for logs tab (count from raw content, not rendered Lines)
            let tool_count = if !is_output && !content.is_empty() {
                let tc = content.lines().filter(|l| l.starts_with("[tool]")).count();
                if tc > 0 { format!(" · {} tools", tc) } else { String::new() }
            } else { String::new() };

            // Search indicator in the title
            let search_indicator = if !query_lc.is_empty() {
                if match_indices.is_empty() {
                    format!(" 🔍/{}/  0 matches", self.search_query)
                } else {
                    format!(" /{}/  {}/{}", self.search_query, self.search_match_idx + 1, match_indices.len())
                }
            } else if self.search_mode {
                format!(" /{}▌", self.search_query)
            } else {
                String::new()
            };

            let mode_label = if is_output {
                format!(" Output  [l] logs{}{} ", tool_count, search_indicator)
            } else {
                format!(" Logs  [o] output{}{} ", tool_count, search_indicator)
            };
            let scroll_info = if total > inner_h {
                let pct = if max_scroll == 0 { 100usize } else {
                    100 - self.detail_scroll.min(max_scroll) * 100 / max_scroll
                };
                format!(" {}% ({}/{}) ", pct, end, total)
            } else {
                String::new()
            };

            let content_block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(C_BORDER_FOCUS))
                .title(Span::styled(mode_label, Style::default().fg(C_ACCENT)))
                .title_bottom(Span::styled(scroll_info, Style::default().fg(C_DIM)));

            let content_widget = Paragraph::new(visible)
                .block(content_block)
                .wrap(Wrap { trim: false });
            frame.render_widget(content_widget, content_area);

            // Scrollbar
            if total > inner_h {
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("↑"))
                    .end_symbol(Some("↓"));
                let mut sb_state = ScrollbarState::new(max_scroll)
                    .position(max_scroll.saturating_sub(self.detail_scroll));
                frame.render_stateful_widget(
                    scrollbar,
                    content_area.inner(Margin { vertical: 1, horizontal: 0 }),
                    &mut sb_state,
                );
            }
        }

        // ── Review body pane (present_for_review) ────────────────────────────
        if review_h > 0 {
            let review_area = chunks[chunk_idx]; chunk_idx += 1;
            let inner_h = review_area.height.saturating_sub(2) as usize;

            // Clamp scroll
            let max_rv_scroll = review_lines.len().saturating_sub(inner_h);
            if self.review_scroll > max_rv_scroll {
                self.review_scroll = max_rv_scroll;
            }
            let rv_start = review_lines.len().saturating_sub(inner_h + self.review_scroll);
            let rv_end = (rv_start + inner_h).min(review_lines.len());
            let visible_review: Vec<Line> = review_lines[rv_start..rv_end].to_vec();

            let rv_title = if let Some(req) = &pending_req {
                format!(" {} ", truncate(&req.prompt, 50))
            } else {
                " Review ".to_string()
            };
            let rv_scroll_info = if review_lines.len() > inner_h {
                let pct = if max_rv_scroll == 0 { 100usize } else {
                    100 - self.review_scroll.min(max_rv_scroll) * 100 / max_rv_scroll
                };
                format!(" {}% ", pct)
            } else {
                String::new()
            };
            let review_widget = Paragraph::new(visible_review)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(C_WARN))
                        .title(Span::styled(&rv_title, Style::default().fg(C_WARN).add_modifier(Modifier::BOLD)))
                        .title_bottom(Span::styled(rv_scroll_info, Style::default().fg(C_DIM))),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(review_widget, review_area);

            // Scrollbar for review body
            if review_lines.len() > inner_h {
                let rv_scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("↑"))
                    .end_symbol(Some("↓"));
                let mut rv_sb = ScrollbarState::new(max_rv_scroll)
                    .position(max_rv_scroll.saturating_sub(self.review_scroll));
                frame.render_stateful_widget(
                    rv_scrollbar,
                    review_area.inner(Margin { vertical: 1, horizontal: 0 }),
                    &mut rv_sb,
                );
            }
        }

        // ── Input / prompt pane ───────────────────────────────────────────────
        if prompt_height > 0 {
            let prompt_area = chunks[chunk_idx];
            let required = pending_req.as_ref().map(|r| r.required).unwrap_or(true);
            let (title, prompt_lines): (&str, Vec<Line>) = if self.input_mode {
                let mut lines: Vec<Line> = vec![];
                match &kind {
                    Some(InteractionKind::FreeText) | None => {
                        let prompt_text = pending_req.as_ref()
                            .map(|r| r.prompt.as_str())
                            .or(agent.waiting_prompt.as_deref())
                            .unwrap_or("Enter response:");
                        lines.push(Line::from(Span::styled(
                            format!(" {}", prompt_text),
                            Style::default().fg(C_WARN),
                        )));
                        lines.push(Line::from(""));
                        lines.push(Line::from(vec![
                            Span::styled(" > ", Style::default().fg(C_SUCCESS)),
                            Span::raw(self.input_buffer.clone()),
                            Span::styled("█", Style::default().fg(C_SUCCESS)),
                        ]));
                        lines.push(Line::from(""));
                        let hint = if !required {
                            " [Enter] send  [empty or /quit to end]  [Esc] cancel"
                        } else {
                            " [Enter] send  [Esc] cancel"
                        };
                        lines.push(Line::from(Span::styled(hint, Style::default().fg(C_DIM))));
                    }
                    _ => {
                        for (i, opt) in options.iter().enumerate() {
                            let sel = i == self.choice_selected;
                            let prefix = if sel { " > " } else { "   " };
                            let label = match &kind {
                                Some(InteractionKind::Confirm) => {
                                    format!("{}{}) {}", prefix, if i == 0 { "y" } else { "n" }, opt)
                                }
                                _ => format!("{}[{}] {}", prefix, i + 1, opt),
                            };
                            let style = if sel {
                                Style::default().fg(C_WARN).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(C_MUTED)
                            };
                            lines.push(Line::from(Span::styled(label, style)));
                        }
                        lines.push(Line::from(""));
                        lines.push(Line::from(Span::styled(
                            " [↑↓] select  [Enter] confirm  [Esc] cancel",
                            Style::default().fg(C_DIM),
                        )));
                    }
                }
                (" Response ", lines)
            } else {
                let mut lines: Vec<Line> = vec![];
                let prompt_text = pending_req.as_ref()
                    .map(|r| r.prompt.as_str())
                    .or(agent.waiting_prompt.as_deref())
                    .unwrap_or("Waiting for input");
                lines.push(Line::from(Span::styled(
                    format!(" {}", prompt_text),
                    Style::default().fg(C_WARN),
                )));
                if !options.is_empty() {
                    lines.push(Line::from(""));
                    for (i, opt) in options.iter().enumerate() {
                        let label = match &kind {
                            Some(InteractionKind::Confirm) => {
                                format!("   {}) {}", if i == 0 { "y" } else { "n" }, opt)
                            }
                            _ => format!("   [{}] {}", i + 1, opt),
                        };
                        lines.push(Line::from(Span::styled(label, Style::default().fg(C_MUTED))));
                    }
                }
                lines.push(Line::from(""));
                let hint = if matches!(agent.status, AgentDisplayStatus::CompleteInteractive) {
                    " [i] respond"
                } else {
                    " [i] respond  [k] kill"
                };
                lines.push(Line::from(Span::styled(hint, Style::default().fg(C_DIM))));
                let title = if matches!(agent.status, AgentDisplayStatus::CompleteInteractive) {
                    " Input Allowed "
                } else {
                    " Input Required "
                };
                (title, lines)
            };

            let prompt_color = if self.input_mode { C_SUCCESS } else { C_WARN };
            let prompt_widget = Paragraph::new(prompt_lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(prompt_color))
                        .title(Span::styled(title, Style::default().fg(prompt_color).add_modifier(Modifier::BOLD))),
                )
                .wrap(Wrap { trim: true });
            frame.render_widget(prompt_widget, prompt_area);
        }
    }

    fn draw_log_panel(&self, frame: &mut Frame, area: Rect) {
        let log_lines: Vec<Line> = self
            .log
            .iter()
            .rev()
            .take(area.height.saturating_sub(2) as usize)
            .rev()
            .map(|entry| {
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", entry.timestamp),
                        Style::default().fg(C_DIM),
                    ),
                    Span::styled(&entry.message, Style::default().fg(C_MUTED)),
                ])
            })
            .collect();

        let log = Paragraph::new(log_lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(C_BORDER))
                    .title(Span::styled(" Log ", Style::default().fg(C_DIM))),
            );
        frame.render_widget(log, area);
    }

    fn draw_help_bar(&self, frame: &mut Frame, area: Rect) {
        use interaction::InteractionKind;

        let help = if self.confirm_delete {
            Line::from(vec![
                Span::styled("[y]", Style::default().fg(C_ERROR).add_modifier(Modifier::BOLD)),
                Span::raw(" confirm delete  "),
                Span::styled("[any key]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" cancel"),
            ])
        } else if self.detail_view && self.input_mode {
            let kind = self.agents.get(self.selected)
                .and_then(|a| a.pending_request.as_ref())
                .map(|r| r.kind.clone());
            match kind {
                Some(InteractionKind::FreeText) | None => Line::from(vec![
                    Span::styled("[Enter]", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
                    Span::raw(" send  "),
                    Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" cancel"),
                ]),
                _ => Line::from(vec![
                    Span::styled("[↑↓]", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
                    Span::raw(" select  "),
                    Span::styled("[Enter]", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
                    Span::raw(" confirm  "),
                    Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" cancel"),
                ]),
            }
        } else if self.detail_view && self.search_mode {
            Line::from(vec![
                Span::styled(" Search: /", Style::default().fg(C_ACCENT)),
                Span::raw(self.search_query.clone()),
                Span::styled("▌", Style::default().fg(C_ACCENT)),
                Span::raw("  "),
                Span::styled("[Enter]", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
                Span::raw(" confirm  "),
                Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" cancel"),
            ])
        } else if self.detail_view && !self.search_query.is_empty() {
            Line::from(vec![
                Span::styled(format!(" /{}/", self.search_query), Style::default().fg(C_ACCENT)),
                Span::raw("  "),
                Span::styled("[n]", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
                Span::raw(" next  "),
                Span::styled("[N]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" prev  "),
                Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" clear search  "),
                Span::styled("[y]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" yank  "),
                Span::styled("[?]", Style::default().fg(C_DIM).add_modifier(Modifier::BOLD)),
                Span::raw(" help"),
            ])
        } else if self.detail_view {
            let can_respond = self.selected_stage_can_respond();
            let can_kill = self.agents.get(self.selected).map(|a| {
                matches!(a.status, AgentDisplayStatus::Active | AgentDisplayStatus::Waiting)
            }).unwrap_or(false);
            let mut spans = vec![
                Span::styled("[←/→]", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
                Span::raw(" stage  "),
                Span::styled("[↑/↓]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" scroll  "),
                Span::styled("[/]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" search  "),
                Span::styled("[y]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" yank  "),
                Span::styled("[l/o]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" logs/out  "),
            ];
            if can_respond {
                spans.push(Span::styled("[i]", Style::default().fg(C_WARN).add_modifier(Modifier::BOLD)));
                spans.push(Span::raw(" respond  "));
            }
            if can_kill {
                spans.push(Span::styled("[k]", Style::default().fg(C_ERROR).add_modifier(Modifier::BOLD)));
                spans.push(Span::raw(" kill  "));
            }
            spans.push(Span::styled("[?]", Style::default().fg(C_DIM).add_modifier(Modifier::BOLD)));
            spans.push(Span::raw(" help  "));
            spans.push(Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)));
            spans.push(Span::raw(" back"));
            Line::from(spans)
        } else {
            let can_kill = self.agents.get(self.selected).map(|a| {
                matches!(a.status, AgentDisplayStatus::Active | AgentDisplayStatus::Waiting)
            }).unwrap_or(false);
            let mut spans = vec![
                Span::styled("[↑↓]", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
                Span::raw(" select  "),
                Span::styled("[Enter]", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
                Span::raw(" detail  "),
                Span::styled("[d]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" delete  "),
            ];
            if can_kill {
                spans.push(Span::styled("[c]", Style::default().add_modifier(Modifier::BOLD)));
                spans.push(Span::raw(" cancel  "));
                spans.push(Span::styled("[k]", Style::default().fg(C_ERROR).add_modifier(Modifier::BOLD)));
                spans.push(Span::raw(" kill  "));
            }
            spans.push(Span::styled("[?]", Style::default().fg(C_DIM).add_modifier(Modifier::BOLD)));
            spans.push(Span::raw(" help  "));
            spans.push(Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)));
            spans.push(Span::raw(" quit"));
            Line::from(spans)
        };

        let help_widget = Paragraph::new(help)
            .style(Style::default().bg(Color::Rgb(20, 20, 30)));
        frame.render_widget(help_widget, area);
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Yank `text` to the system clipboard via the OSC52 terminal escape sequence.
///
/// Works over SSH and in most modern terminal emulators (iTerm2, kitty, WezTerm,
/// Windows Terminal, Alacritty, etc.).  Silently no-ops if the write fails.
fn osc52_yank(text: &str) {
    use std::io::Write;
    // Base64-encode the content
    let encoded = {
        use std::fmt::Write as FmtWrite;
        let bytes = text.as_bytes();
        let mut out = String::with_capacity((bytes.len() * 4 / 3) + 8);
        const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0;
        while i + 3 <= bytes.len() {
            let b0 = bytes[i] as usize;
            let b1 = bytes[i + 1] as usize;
            let b2 = bytes[i + 2] as usize;
            let _ = FmtWrite::write_char(&mut out, TABLE[b0 >> 2] as char);
            let _ = FmtWrite::write_char(&mut out, TABLE[((b0 & 3) << 4) | (b1 >> 4)] as char);
            let _ = FmtWrite::write_char(&mut out, TABLE[((b1 & 0xf) << 2) | (b2 >> 6)] as char);
            let _ = FmtWrite::write_char(&mut out, TABLE[b2 & 0x3f] as char);
            i += 3;
        }
        let rem = bytes.len() - i;
        if rem == 1 {
            let b0 = bytes[i] as usize;
            let _ = FmtWrite::write_char(&mut out, TABLE[b0 >> 2] as char);
            let _ = FmtWrite::write_char(&mut out, TABLE[(b0 & 3) << 4] as char);
            out.push_str("==");
        } else if rem == 2 {
            let b0 = bytes[i] as usize;
            let b1 = bytes[i + 1] as usize;
            let _ = FmtWrite::write_char(&mut out, TABLE[b0 >> 2] as char);
            let _ = FmtWrite::write_char(&mut out, TABLE[((b0 & 3) << 4) | (b1 >> 4)] as char);
            let _ = FmtWrite::write_char(&mut out, TABLE[(b1 & 0xf) << 2] as char);
            out.push('=');
        }
        out
    };
    // Write OSC52 sequence to stdout; the terminal intercepts it and
    // writes the content to the clipboard.
    let osc = format!("\x1b]52;c;{}\x07", encoded);
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(osc.as_bytes());
    let _ = stdout.flush();
}

/// Format a Unix timestamp as a relative time string ("just now", "2m ago", "1h ago").
fn relative_time(ts: i64) -> String {
    if ts == 0 { return "—".to_string(); }
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = (now - ts).max(0) as u64;
    if secs < 10 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        let m = secs / 60;
        format!("{}m ago", m)
    } else if secs < 86400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 { format!("{}h ago", h) } else { format!("{}h{}m ago", h, m) }
    } else {
        let d = secs / 86400;
        format!("{}d ago", d)
    }
}

/// After SIGTERMing a background worker, immediately write Cancelled to meta.json
/// so the next sync tick doesn't revert the status.
fn kill_write_cancelled(run_id: &str) {
    if let Ok(mut meta) = runstate::read_meta(run_id) {
        meta.status = runstate::RunStatus::Cancelled;
        meta.touch();
        let _ = runstate::write_meta(&meta);
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

/// Format a token count in compact style: ≥1000 → "21k", else raw.
fn format_tokens(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// Format elapsed seconds as a human-readable duration string.
fn elapsed_str(started_at: i64) -> String {
    if started_at == 0 { return "—".to_string(); }
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = (now - started_at).max(0) as u64;
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Background task that processes engine commands (cancel/input) for in-process agent state.
async fn engine_background_loop(
    engine: Arc<Mutex<AgentEngine>>,
    mut cmd_rx: mpsc::UnboundedReceiver<EngineCommand>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            EngineCommand::CancelAgent { agent_id } => {
                let mut eng = engine.lock().await;
                let _ = eng.cancel_agent(&agent_id);
                let _ = event_tx.send(AgentEvent::StatusChanged {
                    agent_id,
                    status: AgentDisplayStatus::Cancelled,
                });
            }
            EngineCommand::SendInput { agent_id, input } => {
                let eng = engine.lock().await;
                let msg = leviath_runtime::AgentMessage {
                    agent_id,
                    content: input,
                    target_region: Some("conversation".to_string()),
                    priority: 0,
                };
                let _ = eng.send_message(msg);
            }
        }
    }
}

pub async fn execute(_args: DashboardArgs) -> anyhow::Result<()> {
    // Load config and create engine
    let config = Config::load()?;
    let registry = build_provider_registry(&config);
    let engine = Arc::new(Mutex::new(AgentEngine::with_providers(registry)));

    // Create command channel
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

    let mut dashboard = Dashboard::new(cmd_tx);

    // Store event_tx for background loop
    let bg_event_tx = dashboard.event_tx.clone();

    // Start engine background loop
    tokio::spawn(engine_background_loop(engine.clone(), cmd_rx, bg_event_tx));

    dashboard.add_log("Dashboard started. Use `lev run <agent>` to start an agent.".to_string());

    // Enter TUI mode with mouse support
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    crossterm::execute!(stdout(), crossterm::event::EnableMouseCapture)?;

    let backend = ratatui::backend::CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let tick_rate = Duration::from_millis(100);

    loop {
        dashboard.tick_count += 1;
        dashboard.tick_toasts();

        // Process agent events
        dashboard.process_events();

        // Sync background runs from on-disk run-state dir
        dashboard.sync_from_run_state();

        // Sync state from ECS world (try_lock to avoid blocking the UI)
        if let Ok(eng) = engine.try_lock() {
            dashboard.sync_agent_state_from_world(&eng);
        }

        // Draw
        terminal.draw(|frame| dashboard.draw(frame))?;

        // Handle input
        if event::poll(tick_rate)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    dashboard.handle_key(key.code);
                }
                Event::Mouse(mouse) => {
                    dashboard.handle_mouse(mouse);
                }
                Event::Resize(_, _) => {
                    // Terminal will redraw automatically on next tick
                }
                _ => {}
            }
        }

        if dashboard.should_quit {
            break;
        }
    }

    // Restore terminal
    crossterm::execute!(stdout(), crossterm::event::DisableMouseCapture)?;
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    println!("Dashboard closed.");
    Ok(())
}
