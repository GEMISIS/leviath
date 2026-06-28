//! `lev dashboard` - Interactive terminal UI for managing concurrent agents.

use clap::Args;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use leviath_runtime::{
    AgentEngine, AgentState, AgentStatus, ContextWindow,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
    Frame, Terminal,
};
use std::io::stdout;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

use super::run::build_provider_registry;
use crate::config::Config;
use crate::interaction;
use crate::runstate::{self, RunStatus};

#[derive(Args)]
pub struct DashboardArgs {}

/// Display status for agents in the dashboard.
#[derive(Debug, Clone)]
pub enum AgentDisplayStatus {
    Active,
    Waiting,
    Complete,
    Error(String),
    Idle,
    Cancelled,
}

impl std::fmt::Display for AgentDisplayStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "●ACTIVE"),
            Self::Waiting => write!(f, "◆WAITING"),
            Self::Complete => write!(f, "✓COMPLETE"),
            Self::Error(msg) => write!(f, "✗ERROR: {}", msg),
            Self::Idle => write!(f, "○IDLE"),
            Self::Cancelled => write!(f, "⊘CANCEL"),
        }
    }
}

impl AgentDisplayStatus {
    fn color(&self) -> Color {
        match self {
            Self::Active => Color::Green,
            Self::Waiting => Color::Yellow,
            Self::Complete => Color::Cyan,
            Self::Error(_) => Color::Red,
            Self::Idle => Color::Gray,
            Self::Cancelled => Color::DarkGray,
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
    pub status: AgentDisplayStatus,
    pub tokens: (usize, usize),
    pub iteration: usize,
    pub waiting_prompt: Option<String>,
    /// Full structured interaction request (populated for WaitingInput agents)
    pub pending_request: Option<interaction::InteractionRequest>,
    /// The ECS entity for this agent (dummy sentinel for run-state agents)
    pub entity: bevy_ecs::prelude::Entity,
    /// True when tracked via on-disk run-state (background worker process)
    pub is_run_state: bool,
    /// PID of worker process (0 for in-process agents)
    pub pid: u32,
    /// Working directory the agent ran in
    pub workdir: String,
    /// Original task
    pub task: String,
    /// Original model override
    #[allow(dead_code)]
    pub model: Option<String>,
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
    /// Scroll offset for detail view log: 0 = bottom (auto-scroll), >0 = scrolled up
    detail_scroll: usize,
    /// Selected option index for MultipleChoice/ToolApproval/Confirm input
    choice_selected: usize,
}

impl Dashboard {
    fn new(cmd_tx: mpsc::UnboundedSender<EngineCommand>) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        Self {
            agents: Vec::new(),
            selected: 0,
            log: Vec::new(),
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
        }
    }

    fn add_log(&mut self, msg: String) {
        let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();
        self.log.push(LogEntry { timestamp, message: msg });
        if self.log.len() > 50 {
            self.log.remove(0);
        }
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
                RunStatus::Error => AgentDisplayStatus::Error(run.error.clone().unwrap_or_default()),
                RunStatus::Cancelled => AgentDisplayStatus::Cancelled,
            };
            // For WaitingInput agents, read the pending interaction from disk once
            let (waiting_prompt, pending_request) = if matches!(run.status, RunStatus::WaitingInput) {
                let req = interaction::read_request(&run.run_id);
                (req.as_ref().map(|r| r.prompt.clone()), req)
            } else {
                (None, None)
            };

            if let Some(agent) = self.agents.iter_mut().find(|a| a.id == run.run_id) {
                agent.stage = run.current_stage.clone();
                agent.iteration = run.iteration;
                agent.tokens = (run.prompt_tokens + run.completion_tokens, 0);
                agent.pid = run.pid;
                agent.status = status;
                agent.workdir = run.workdir.clone();
                // Only update waiting_prompt/pending_request when we have one; clear when no longer waiting
                if matches!(run.status, RunStatus::WaitingInput) {
                    if waiting_prompt.is_some() {
                        agent.waiting_prompt = waiting_prompt;
                        agent.pending_request = pending_request;
                    }
                } else {
                    agent.waiting_prompt = None;
                    agent.pending_request = None;
                }
            } else {
                self.agents.push(DashboardAgent {
                    id: run.run_id.clone(),
                    blueprint_name: run.agent_name.clone(),
                    agent_path: run.agent_path.clone(),
                    stage: run.current_stage.clone(),
                    status,
                    tokens: (run.prompt_tokens + run.completion_tokens, 0),
                    iteration: run.iteration,
                    waiting_prompt,
                    pending_request,
                    entity: dummy,
                    is_run_state: true,
                    pid: run.pid,
                    workdir: run.workdir.clone(),
                    task: run.task.clone(),
                    model: run.model.clone(),
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
                agent.tokens = (window.current_tokens, window.max_tokens);
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
                    let input = self.input_buffer.trim().to_string();
                    if input.is_empty() { return; }
                    let d = truncate(&input, 40);
                    (InteractionResponse::text(&r.id, &input), d)
                }
                InteractionKind::MultipleChoice | InteractionKind::ToolApproval => {
                    let idx = self.choice_selected;
                    let label = r.options.get(idx).cloned().unwrap_or_else(|| idx.to_string());
                    let d = truncate(&label, 40);
                    (InteractionResponse::choice(&r.id, idx), d)
                }
                InteractionKind::Confirm => {
                    let approved = self.choice_selected == 0;
                    let label = if approved { "Yes" } else { "No" };
                    (InteractionResponse::approval(&r.id, approved, ApprovalScope::Once), label.to_string())
                }
            },
            None => {
                let input = self.input_buffer.trim().to_string();
                if input.is_empty() { return; }
                let d = truncate(&input, 40);
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

        if let Some(a) = self.agents.get_mut(self.selected) {
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

    fn handle_key(&mut self, key: KeyCode) {
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
                        if !matches!(kind, Some(InteractionKind::FreeText) | None) {
                            if self.choice_selected > 0 {
                                self.choice_selected -= 1;
                            }
                        }
                    }
                    KeyCode::Down => {
                        if !matches!(kind, Some(InteractionKind::FreeText) | None) && options_len > 0 {
                            if self.choice_selected < options_len - 1 {
                                self.choice_selected += 1;
                            }
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

            // Detail view — not in input mode
            match key {
                KeyCode::Esc => {
                    self.detail_view = false;
                    self.detail_scroll = 0;
                }
                KeyCode::Char('i') => {
                    let can_respond = self.agents.get(self.selected).map(|a| {
                        (a.waiting_prompt.is_some() || a.pending_request.is_some())
                            && !matches!(a.status, AgentDisplayStatus::Cancelled)
                    }).unwrap_or(false);
                    if can_respond {
                        self.input_mode = true;
                        self.choice_selected = 0;
                        self.input_buffer.clear();
                    }
                }
                KeyCode::Up => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                }
                KeyCode::Down => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
                KeyCode::Char('b') => {
                    self.detail_scroll = usize::MAX;
                }
                KeyCode::Char('e') => {
                    self.detail_scroll = 0;
                }
                KeyCode::Char('c') => {
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
                            self.add_log(format!("{}: Cancel requested", agent_id));
                        }
                    }
                }
                KeyCode::Char('k') => {
                    if let Some(agent) = self.agents.get(self.selected) {
                        let agent_id = agent.id.clone();
                        let pid = agent.pid;
                        let is_run_state = agent.is_run_state;
                        let was_waiting = matches!(agent.status, AgentDisplayStatus::Waiting);
                        if is_run_state {
                            #[cfg(unix)]
                            if pid > 0 {
                                unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
                            }
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
                _ => {}
            }
            return;
        }

        // ── Main agent list ──────────────────────────────────────────────────
        match key {
            KeyCode::Char('q') => {
                // Background workers are detached (setsid) and survive dashboard exit — no warning needed
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
                    let agent_id = agent.id.clone();
                    if agent.is_run_state {
                        #[cfg(unix)]
                        if agent.pid > 0 {
                            unsafe { libc::kill(agent.pid as libc::pid_t, libc::SIGTERM); }
                        }
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
            _ => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        if self.detail_view {
            // Full-screen detail view
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(frame.area());
            self.draw_detail_panel(frame, chunks[0]);
            self.draw_help_bar(frame, chunks[1]);
            return;
        }

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

    fn draw_agent_table(&mut self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            Cell::from("ID"),
            Cell::from("Blueprint"),
            Cell::from("Stage"),
            Cell::from("Status"),
            Cell::from("Tokens"),
            Cell::from("Where"),
        ])
        .style(Style::default().add_modifier(Modifier::BOLD))
        .height(1);

        let rows: Vec<Row> = self
            .agents
            .iter()
            .map(|agent| {
                let status_color = agent.status.color();
                let where_str = {
                    let wd = &agent.workdir;
                    if wd.is_empty() {
                        "—".to_string()
                    } else {
                        std::path::Path::new(wd)
                            .components()
                            .rev()
                            .take(2)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .map(|c| c.as_os_str().to_string_lossy().to_string())
                            .collect::<Vec<_>>()
                            .join("/")
                    }
                };
                let tok_str = if agent.tokens.1 > 0 {
                    format!("{}/{}", format_tokens(agent.tokens.0), format_tokens(agent.tokens.1))
                } else {
                    format_tokens(agent.tokens.0)
                };
                Row::new(vec![
                    Cell::from(agent.id.clone()),
                    Cell::from(agent.blueprint_name.clone()),
                    Cell::from(agent.stage.clone()),
                    Cell::from(agent.status.to_string()).style(Style::default().fg(status_color)),
                    Cell::from(tok_str),
                    Cell::from(where_str),
                ])
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Percentage(22),
                Constraint::Percentage(14),
                Constraint::Percentage(14),
                Constraint::Percentage(20),
                Constraint::Percentage(12),
                Constraint::Percentage(18),
            ],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Agents "))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

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

        let is_waiting = matches!(agent.status, AgentDisplayStatus::Waiting);
        let pending_req = agent.pending_request.clone();
        let kind = pending_req.as_ref().map(|r| r.kind.clone());
        let options: Vec<String> = pending_req.as_ref().map(|r| r.options.clone()).unwrap_or_default();
        let has_prompt = is_waiting
            && (pending_req.is_some() || agent.waiting_prompt.is_some())
            && !matches!(agent.status, AgentDisplayStatus::Cancelled);

        // ── Layout heights ──────────────────────────────────────────────────
        let info_height: u16 = if agent.task.is_empty() { 6 } else { 7 };
        let prompt_height: u16 = if has_prompt || (self.input_mode && is_waiting) {
            let n = options.len() as u16;
            if self.input_mode {
                match &kind {
                    Some(InteractionKind::FreeText) | None => 5,
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

        // ── Split area ──────────────────────────────────────────────────────
        let (info_area, log_area, prompt_area_opt) = if prompt_height > 0 {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(info_height),
                    Constraint::Min(3),
                    Constraint::Length(prompt_height),
                ])
                .split(area);
            (chunks[0], chunks[1], Some(chunks[2]))
        } else {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(info_height), Constraint::Min(3)])
                .split(area);
            (chunks[0], chunks[1], None)
        };

        // ── Info block ──────────────────────────────────────────────────────
        {
            let mut info_lines = vec![
                Line::from(vec![
                    Span::styled(format!("[{}]  ", agent.id), Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(agent.status.to_string(), Style::default().fg(agent.status.color())),
                ]),
                Line::from(vec![
                    Span::styled("Blueprint: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(agent.blueprint_name.clone()),
                    Span::styled("  Stage: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(agent.stage.clone()),
                ]),
                Line::from(vec![
                    Span::styled("Location: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(if agent.workdir.is_empty() { "—".to_string() } else { agent.workdir.clone() }),
                ]),
                Line::from(vec![
                    Span::styled("Tokens:   ", Style::default().fg(Color::DarkGray)),
                    Span::raw(if agent.tokens.1 > 0 {
                        format!("{} used / {} max", format_tokens(agent.tokens.0), format_tokens(agent.tokens.1))
                    } else {
                        format!("{} used", format_tokens(agent.tokens.0))
                    }),
                    Span::styled("  Iter: ", Style::default().fg(Color::DarkGray)),
                    Span::raw(agent.iteration.to_string()),
                ]),
            ];
            if !agent.task.is_empty() {
                info_lines.push(Line::from(vec![
                    Span::styled("Task:     ", Style::default().fg(Color::DarkGray)),
                    Span::raw(truncate(&agent.task, 120)),
                ]));
            }
            let info_widget = Paragraph::new(info_lines)
                .block(Block::default().borders(Borders::ALL).title(" Agent "))
                .wrap(Wrap { trim: true });
            frame.render_widget(info_widget, info_area);
        }

        // ── Output / log block (scrollable) ────────────────────────────────
        {
            let inner_h = log_area.height.saturating_sub(2) as usize;

            let log_content = if agent.is_run_state {
                runstate::tail_log(&agent.id, 131_072)
            } else {
                String::new()
            };

            let all_lines: Vec<&str> = log_content.lines().collect();
            let total = all_lines.len();

            // Clamp scroll to valid range
            let max_scroll = total.saturating_sub(inner_h);
            if self.detail_scroll > max_scroll {
                self.detail_scroll = max_scroll;
            }

            let start = total.saturating_sub(inner_h + self.detail_scroll);
            let end = (start + inner_h).min(total);

            let visible: Vec<Line> = all_lines[start..end]
                .iter()
                .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(Color::Gray))))
                .collect();

            let log_title = if total > inner_h {
                let pct = if max_scroll == 0 { 100usize } else {
                    100 - self.detail_scroll * 100 / max_scroll
                };
                format!(" Output  {}%  ({}/{})", pct, end, total)
            } else {
                " Output ".to_string()
            };

            let log_widget = Paragraph::new(visible)
                .block(Block::default().borders(Borders::ALL).title(log_title));
            frame.render_widget(log_widget, log_area);
        }

        // ── Prompt / input block ────────────────────────────────────────────
        if let Some(prompt_area) = prompt_area_opt {
            let (title, prompt_lines): (&str, Vec<Line>) = if self.input_mode {
                let mut lines: Vec<Line> = vec![];
                match &kind {
                    Some(InteractionKind::FreeText) | None => {
                        lines.push(Line::from(vec![
                            Span::styled("> ", Style::default().fg(Color::Green)),
                            Span::raw(self.input_buffer.clone()),
                            Span::styled("█", Style::default().fg(Color::Green)),
                        ]));
                        lines.push(Line::from(""));
                        lines.push(Line::from(Span::styled(
                            "[Enter] send  [Esc] cancel",
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                    _ => {
                        for (i, opt) in options.iter().enumerate() {
                            let sel = i == self.choice_selected;
                            let prefix = if sel { "> " } else { "  " };
                            let label = match &kind {
                                Some(InteractionKind::Confirm) => {
                                    format!("{}{}) {}", prefix, if i == 0 { "y" } else { "n" }, opt)
                                }
                                _ => format!("{}[{}] {}", prefix, i + 1, opt),
                            };
                            let style = if sel {
                                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default()
                            };
                            lines.push(Line::from(Span::styled(label, style)));
                        }
                        lines.push(Line::from(""));
                        lines.push(Line::from(Span::styled(
                            "[↑↓] select  [Enter] confirm  [Esc] cancel",
                            Style::default().fg(Color::DarkGray),
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
                    prompt_text.to_string(),
                    Style::default().fg(Color::Yellow),
                )));
                if !options.is_empty() {
                    lines.push(Line::from(""));
                    for (i, opt) in options.iter().enumerate() {
                        let label = match &kind {
                            Some(InteractionKind::Confirm) => {
                                format!("  {}) {}", if i == 0 { "y" } else { "n" }, opt)
                            }
                            _ => format!("  [{}] {}", i + 1, opt),
                        };
                        lines.push(Line::from(label));
                    }
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "[i] respond  [c/k] cancel/kill",
                    Style::default().fg(Color::DarkGray),
                )));
                (" Input Required ", lines)
            };

            let prompt_widget = Paragraph::new(prompt_lines)
                .block(Block::default().borders(Borders::ALL).title(title))
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
                        format!("{} ", entry.timestamp),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(&entry.message),
                ])
            })
            .collect();

        let log = Paragraph::new(log_lines)
            .block(Block::default().borders(Borders::ALL).title(" Log "));

        frame.render_widget(log, area);
    }

    fn draw_help_bar(&self, frame: &mut Frame, area: Rect) {
        use interaction::InteractionKind;

        let help = if self.confirm_delete {
            Line::from(vec![
                Span::styled("[y]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
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
                    Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" send  "),
                    Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" cancel"),
                ]),
                _ => Line::from(vec![
                    Span::styled("[↑↓]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" select  "),
                    Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" confirm  "),
                    Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" cancel"),
                ]),
            }
        } else if self.detail_view {
            let can_respond = self.agents.get(self.selected).map(|a| {
                (a.waiting_prompt.is_some() || a.pending_request.is_some())
                    && !matches!(a.status, AgentDisplayStatus::Cancelled)
            }).unwrap_or(false);
            if can_respond {
                Line::from(vec![
                    Span::styled("[↑↓]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" scroll  "),
                    Span::styled("[b/e]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" beg/end  "),
                    Span::styled("[i]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" respond  "),
                    Span::styled("[c/k]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" cancel/kill  "),
                    Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" back"),
                ])
            } else {
                Line::from(vec![
                    Span::styled("[↑↓]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" scroll  "),
                    Span::styled("[b/e]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" beg/end  "),
                    Span::styled("[c/k]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" cancel/kill  "),
                    Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" back"),
                ])
            }
        } else {
            Line::from(vec![
                Span::styled("[↑↓]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" select  "),
                Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" detail  "),
                Span::styled("[d]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" delete  "),
                Span::styled("[c]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" cancel  "),
                Span::styled("[k]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" kill  "),
                Span::styled("[q]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" quit"),
            ])
        };

        let help_widget = Paragraph::new(help);
        frame.render_widget(help_widget, area);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
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

    dashboard.add_log("Dashboard started. Use `lev run <blueprint> --task \"...\"` to start agents.".to_string());

    // Enter TUI mode
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;

    let backend = ratatui::backend::CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let tick_rate = Duration::from_millis(100);

    loop {
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
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    dashboard.handle_key(key.code);
                }
            }
        }

        if dashboard.should_quit {
            break;
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    println!("Dashboard closed.");
    Ok(())
}
