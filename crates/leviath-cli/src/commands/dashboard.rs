//! `lev dashboard` - Interactive terminal UI for managing concurrent agents.

use clap::Args;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use leviath_runtime::{
    AgentEngine, AgentPool, AgentState, AgentStatus, ContextWindow,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
    Frame, Terminal,
};
use std::io::stdout;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

use super::run::{build_provider_registry, initialize_context_window, parse_manifest_public};
use crate::config::Config;

#[derive(Args)]
pub struct DashboardArgs {
    /// Initial agent to run (optional, can add more from dashboard)
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Initial task (required if path is given)
    #[arg(short, long)]
    pub task: Option<String>,
}

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
    pub stage: String,
    pub status: AgentDisplayStatus,
    pub tokens: (usize, usize),
    pub iteration: usize,
    pub waiting_prompt: Option<String>,
    /// The ECS entity for this agent
    pub entity: bevy_ecs::prelude::Entity,
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
    RunAgent {
        agent_id: String,
        entity: bevy_ecs::prelude::Entity,
        provider_name: String,
        model_name: String,
        max_iterations: usize,
    },
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
    /// For spawning new agents inline
    new_agent_mode: NewAgentInputMode,
    event_rx: mpsc::UnboundedReceiver<AgentEvent>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    table_state: TableState,
    should_quit: bool,
    confirm_quit: bool,
    engine: Arc<Mutex<AgentEngine>>,
}

/// Tracks the state of inline new-agent creation.
#[derive(Debug, Clone, PartialEq)]
enum NewAgentInputMode {
    None,
    WaitingForPath,
    WaitingForTask { path: String },
}

impl Dashboard {
    fn new(
        engine: Arc<Mutex<AgentEngine>>,
        cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    ) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        Self {
            agents: Vec::new(),
            selected: 0,
            log: Vec::new(),
            input_buffer: String::new(),
            input_mode: false,
            new_agent_mode: NewAgentInputMode::None,
            event_rx,
            event_tx,
            cmd_tx,
            table_state,
            should_quit: false,
            confirm_quit: false,
            engine,
        }
    }

    fn add_log(&mut self, msg: String) {
        let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();
        self.log.push(LogEntry { timestamp, message: msg });
        if self.log.len() > 50 {
            self.log.remove(0);
        }
    }

    /// Sync agent state from the ECS world.
    fn sync_agent_state_from_world(&mut self, engine: &AgentEngine) {
        for agent in &mut self.agents {
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
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        agent.iteration += 1;
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

    fn spawn_agent_from_path_task(&mut self, path: &str, task: &str) {
        let project_path = Path::new(path);
        let manifest_path = if project_path.is_dir() {
            project_path.join("agent.leviath")
        } else {
            project_path.to_path_buf()
        };

        let content = match std::fs::read_to_string(&manifest_path) {
            Ok(c) => c,
            Err(e) => {
                self.add_log(format!("Error reading {}: {}", manifest_path.display(), e));
                return;
            }
        };

        let blueprint = match parse_manifest_public(&content) {
            Ok(b) => b,
            Err(e) => {
                self.add_log(format!("Error parsing manifest: {}", e));
                return;
            }
        };

        let stage = match blueprint.stages.first() {
            Some(s) => s,
            None => {
                self.add_log("Blueprint has no stages".to_string());
                return;
            }
        };

        let provider_name = stage.model.provider.clone();
        let model_name = stage.model.model.clone();
        let max_iterations = stage.max_iterations.unwrap_or(10);
        let stage_name = stage.name.clone();

        // Spawn entity in the engine
        let task_str = task.to_string();

        // Clone engine Arc to avoid borrowing self during lock
        let engine_arc = self.engine.clone();
        let spawn_result = match engine_arc.try_lock() {
            Ok(mut engine) => {
                let mut pool = AgentPool::new(blueprint.clone());
                let agent_id = pool.spawn_agent(engine.world_mut());
                match pool.get_agent(&agent_id) {
                    Some(entity) => {
                        initialize_context_window(&mut engine, entity, &blueprint, &task_str);
                        Some((agent_id, entity))
                    }
                    None => None,
                }
            }
            Err(_) => None,
        };

        match spawn_result {
            Some((agent_id, entity)) => {
                let dashboard_agent = DashboardAgent {
                    id: agent_id.clone(),
                    blueprint_name: blueprint.name.clone(),
                    stage: stage_name,
                    status: AgentDisplayStatus::Active,
                    tokens: (0, blueprint.context_layout.total_budget_tokens),
                    iteration: 0,
                    waiting_prompt: None,
                    entity,
                };

                self.agents.push(dashboard_agent);

                let _ = self.cmd_tx.send(EngineCommand::RunAgent {
                    agent_id: agent_id.clone(),
                    entity,
                    provider_name,
                    model_name,
                    max_iterations,
                });

                self.add_log(format!(
                    "Spawned {} with task: {}",
                    agent_id,
                    truncate(&task_str, 60)
                ));
            }
            None => {
                self.add_log("Engine busy, try again".to_string());
            }
        }
    }

    fn handle_key(&mut self, key: KeyCode) {
        if self.confirm_quit {
            match key {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.should_quit = true;
                }
                _ => {
                    self.confirm_quit = false;
                }
            }
            return;
        }

        // New agent input mode
        if self.new_agent_mode != NewAgentInputMode::None {
            match key {
                KeyCode::Esc => {
                    self.new_agent_mode = NewAgentInputMode::None;
                    self.input_buffer.clear();
                    self.add_log("Cancelled new agent".to_string());
                }
                KeyCode::Enter => {
                    let input = self.input_buffer.clone();
                    self.input_buffer.clear();

                    match &self.new_agent_mode {
                        NewAgentInputMode::WaitingForPath => {
                            if input.is_empty() {
                                self.new_agent_mode = NewAgentInputMode::None;
                            } else {
                                self.new_agent_mode = NewAgentInputMode::WaitingForTask { path: input };
                                self.add_log("Enter task for the new agent:".to_string());
                            }
                        }
                        NewAgentInputMode::WaitingForTask { path } => {
                            let path = path.clone();
                            self.new_agent_mode = NewAgentInputMode::None;
                            if !input.is_empty() {
                                self.spawn_agent_from_path_task(&path, &input);
                            }
                        }
                        NewAgentInputMode::None => {}
                    }
                }
                KeyCode::Backspace => {
                    self.input_buffer.pop();
                }
                KeyCode::Char(c) => {
                    self.input_buffer.push(c);
                }
                _ => {}
            }
            return;
        }

        if self.input_mode {
            match key {
                KeyCode::Esc => {
                    self.input_mode = false;
                    self.input_buffer.clear();
                }
                KeyCode::Enter => {
                    let input = self.input_buffer.clone();
                    self.input_buffer.clear();
                    self.input_mode = false;
                    if !input.is_empty() {
                        if let Some(agent) = self.agents.get_mut(self.selected) {
                            let agent_id = agent.id.clone();
                            agent.waiting_prompt = None;
                            agent.status = AgentDisplayStatus::Active;
                            // Send input to engine
                            let _ = self.cmd_tx.send(EngineCommand::SendInput {
                                agent_id: agent_id.clone(),
                                input: input.clone(),
                            });
                            self.add_log(format!("Sent to {}: {}", agent_id, truncate(&input, 40)));
                        }
                    }
                }
                KeyCode::Backspace => {
                    self.input_buffer.pop();
                }
                KeyCode::Char(c) => {
                    self.input_buffer.push(c);
                }
                _ => {}
            }
            return;
        }

        match key {
            KeyCode::Char('q') => {
                let has_active = self.agents.iter().any(|a| {
                    matches!(a.status, AgentDisplayStatus::Active | AgentDisplayStatus::Waiting)
                });
                if has_active {
                    self.confirm_quit = true;
                    self.add_log("Press 'y' to confirm quit with running agents".to_string());
                } else {
                    self.should_quit = true;
                }
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
                if let Some(agent) = self.agents.get(self.selected) {
                    if agent.waiting_prompt.is_some() {
                        self.input_mode = true;
                    }
                }
            }
            KeyCode::Char('c') => {
                if let Some(agent) = self.agents.get(self.selected) {
                    if matches!(agent.status, AgentDisplayStatus::Active | AgentDisplayStatus::Waiting) {
                        let agent_id = agent.id.clone();
                        let _ = self.cmd_tx.send(EngineCommand::CancelAgent { agent_id: agent_id.clone() });
                        self.add_log(format!("{}: Cancel requested", agent_id));
                    }
                }
            }
            KeyCode::Char('k') => {
                if let Some(agent) = self.agents.get(self.selected) {
                    let agent_id = agent.id.clone();
                    let _ = self.cmd_tx.send(EngineCommand::CancelAgent { agent_id: agent_id.clone() });
                    // Mark as cancelled immediately in the UI
                    if let Some(agent) = self.agents.get_mut(self.selected) {
                        agent.status = AgentDisplayStatus::Cancelled;
                    }
                    self.add_log(format!("{}: Killed", agent_id));
                }
            }
            KeyCode::Char('n') => {
                self.new_agent_mode = NewAgentInputMode::WaitingForPath;
                self.input_buffer.clear();
                self.add_log("Enter path to agent.leviath (or directory):".to_string());
            }
            _ => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(8),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(1),
            ])
            .split(frame.area());

        self.draw_agent_table(frame, chunks[0]);
        self.draw_detail_panel(frame, chunks[1]);
        self.draw_log_panel(frame, chunks[2]);
        self.draw_help_bar(frame, chunks[3]);

        if self.confirm_quit {
            self.draw_quit_confirm(frame);
        }
    }

    fn draw_agent_table(&mut self, frame: &mut Frame, area: Rect) {
        let header = Row::new(vec![
            Cell::from("ID"),
            Cell::from("Stage"),
            Cell::from("Status"),
            Cell::from("Tokens"),
            Cell::from("Iter"),
        ])
        .style(Style::default().add_modifier(Modifier::BOLD))
        .height(1);

        let rows: Vec<Row> = self
            .agents
            .iter()
            .map(|agent| {
                let status_color = agent.status.color();
                Row::new(vec![
                    Cell::from(agent.id.clone()),
                    Cell::from(agent.stage.clone()),
                    Cell::from(agent.status.to_string()).style(Style::default().fg(status_color)),
                    Cell::from(format!("{}k/{}k", agent.tokens.0 / 1000, agent.tokens.1 / 1000)),
                    Cell::from(agent.iteration.to_string()),
                ])
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Percentage(25),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
                Constraint::Percentage(15),
            ],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Agents "))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        frame.render_stateful_widget(table, area, &mut self.table_state);
    }

    fn draw_detail_panel(&self, frame: &mut Frame, area: Rect) {
        let detail_text = if self.new_agent_mode != NewAgentInputMode::None {
            let prompt_text = match &self.new_agent_mode {
                NewAgentInputMode::WaitingForPath => "Path to agent: ",
                NewAgentInputMode::WaitingForTask { .. } => "Task: ",
                NewAgentInputMode::None => "",
            };
            vec![
                Line::from(Span::styled(
                    "New Agent",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::raw(prompt_text),
                    Span::raw(&self.input_buffer),
                    Span::styled("█", Style::default().fg(Color::Green)),
                ]),
            ]
        } else if let Some(agent) = self.agents.get(self.selected) {
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(
                        format!("[{}] ", agent.id),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        "Blueprint: {} | Stage: {}",
                        agent.blueprint_name, agent.stage
                    )),
                ]),
            ];

            if let Some(ref prompt) = agent.waiting_prompt {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    prompt.clone(),
                    Style::default().fg(Color::Yellow),
                )));
            }

            if self.input_mode {
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::Green)),
                    Span::raw(&self.input_buffer),
                    Span::styled("█", Style::default().fg(Color::Green)),
                ]));
            }

            lines
        } else {
            vec![Line::from("No agents selected. Press 'n' to add one.")]
        };

        let detail = Paragraph::new(detail_text)
            .block(Block::default().borders(Borders::ALL).title(" Agent Detail "))
            .wrap(Wrap { trim: true });

        frame.render_widget(detail, area);
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
        let help = if self.input_mode || self.new_agent_mode != NewAgentInputMode::None {
            Line::from(vec![
                Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("cancel  "),
                Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("send"),
            ])
        } else {
            Line::from(vec![
                Span::styled("[q]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("uit  "),
                Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("respond  "),
                Span::styled("[↑↓]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("select  "),
                Span::styled("[c]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("ancel  "),
                Span::styled("[k]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("ill  "),
                Span::styled("[n]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("ew"),
            ])
        };

        let help_widget = Paragraph::new(help);
        frame.render_widget(help_widget, area);
    }

    fn draw_quit_confirm(&self, frame: &mut Frame) {
        let area = frame.area();
        let popup_area = Rect {
            x: area.width / 4,
            y: area.height / 2 - 2,
            width: area.width / 2,
            height: 4,
        };

        let popup = Paragraph::new(vec![
            Line::from(""),
            Line::from("Agents still running. Quit? (y/n)"),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Confirm Quit ")
                .style(Style::default().fg(Color::Red)),
        );

        frame.render_widget(Clear, popup_area);
        frame.render_widget(popup, popup_area);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

/// Background task that processes engine commands and runs agent inference.
async fn engine_background_loop(
    engine: Arc<Mutex<AgentEngine>>,
    mut cmd_rx: mpsc::UnboundedReceiver<EngineCommand>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            EngineCommand::RunAgent {
                agent_id,
                entity,
                provider_name,
                model_name,
                max_iterations,
            } => {
                let engine = engine.clone();
                let event_tx = event_tx.clone();
                let agent_id_clone = agent_id.clone();

                tokio::spawn(async move {
                    let _ = event_tx.send(AgentEvent::StatusChanged {
                        agent_id: agent_id_clone.clone(),
                        status: AgentDisplayStatus::Active,
                    });

                    // Run inference loop
                    let result = {
                        let mut eng = engine.lock().await;
                        eng.run_inference_loop_filtered(
                            entity,
                            &provider_name,
                            &model_name,
                            Vec::new(),
                            max_iterations,
                            None,
                            None,
                            None,
                            &mut |tool_calls| {
                                let tx = event_tx.clone();
                                let aid = agent_id_clone.clone();
                                async move {
                                    for tc in &tool_calls {
                                        let _ = tx.send(AgentEvent::ToolCalled {
                                            agent_id: aid.clone(),
                                            tool: tc.name.clone(),
                                            args: tc.arguments.to_string(),
                                        });
                                    }
                                    Vec::new()
                                }
                            },
                        )
                        .await
                    };

                    match result {
                        Ok(resp) => {
                            let _ = event_tx.send(AgentEvent::InferenceComplete {
                                agent_id: agent_id_clone.clone(),
                                content: resp.content,
                                tokens_used: resp.tokens_used.completion_tokens,
                                tokens_prompt: resp.tokens_used.prompt_tokens,
                            });
                        }
                        Err(e) => {
                            let _ = event_tx.send(AgentEvent::Error {
                                agent_id: agent_id_clone.clone(),
                                error: e.to_string(),
                            });
                        }
                    }

                    let _ = event_tx.send(AgentEvent::AgentDone {
                        agent_id: agent_id_clone,
                    });
                });
            }
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

pub async fn execute(args: DashboardArgs) -> anyhow::Result<()> {
    // Load config and create engine
    let config = Config::load()?;
    let registry = build_provider_registry(&config);
    let engine = Arc::new(Mutex::new(AgentEngine::with_providers(registry)));

    // Create command channel
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

    let mut dashboard = Dashboard::new(engine.clone(), cmd_tx);

    // Store event_tx for background loop
    let bg_event_tx = dashboard.event_tx.clone();

    // Start engine background loop
    tokio::spawn(engine_background_loop(engine.clone(), cmd_rx, bg_event_tx));

    // If a path+task was provided, spawn the initial agent
    if let Some(ref path) = args.path {
        let task = args
            .task
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--task is required when a path is specified"))?;

        dashboard.spawn_agent_from_path_task(path, task);
    }

    if dashboard.agents.is_empty() {
        dashboard.add_log("Dashboard started. Press 'n' to add agents or start with: lev dashboard <path> --task <task>".to_string());
    }

    // Enter TUI mode
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;

    let backend = ratatui::backend::CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let tick_rate = Duration::from_millis(100);

    loop {
        // Process agent events
        dashboard.process_events();

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
