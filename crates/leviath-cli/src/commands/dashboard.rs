//! `lev dashboard` - Interactive terminal UI for managing concurrent agents.

use clap::Args;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use leviath_core::Blueprint;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
    Frame, Terminal,
};
use std::io::stdout;
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;

use super::run::parse_manifest_public;

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
}

/// Event from an agent back to the dashboard.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    StageChanged { agent_id: String, stage: String },
    StatusChanged { agent_id: String, status: AgentDisplayStatus },
    NeedsInput { agent_id: String, prompt: String },
    ToolCalled { agent_id: String, tool: String, args: String },
    InferenceComplete { agent_id: String },
    Error { agent_id: String, error: String },
    Log(String),
}

/// Log entry for the dashboard log panel.
#[derive(Debug, Clone)]
struct LogEntry {
    timestamp: String,
    message: String,
}

/// The interactive dashboard state.
struct Dashboard {
    agents: Vec<DashboardAgent>,
    selected: usize,
    log: Vec<LogEntry>,
    input_buffer: String,
    input_mode: bool,
    event_rx: mpsc::UnboundedReceiver<AgentEvent>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    table_state: TableState,
    should_quit: bool,
    confirm_quit: bool,
}

impl Dashboard {
    fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut table_state = TableState::default();
        table_state.select(Some(0));
        Self {
            agents: Vec::new(),
            selected: 0,
            log: Vec::new(),
            input_buffer: String::new(),
            input_mode: false,
            event_rx,
            event_tx,
            table_state,
            should_quit: false,
            confirm_quit: false,
        }
    }

    fn add_log(&mut self, msg: String) {
        let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();
        self.log.push(LogEntry { timestamp, message: msg });
        // Keep last 50 entries
        if self.log.len() > 50 {
            self.log.remove(0);
        }
    }

    fn process_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                AgentEvent::StageChanged { agent_id, stage } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        agent.stage = stage.clone();
                    }
                    self.add_log(format!("{}: Stage changed to {}", agent_id, stage));
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
                    self.add_log(format!("{}: Waiting for user input", agent_id));
                }
                AgentEvent::ToolCalled { agent_id, tool, args } => {
                    self.add_log(format!("{}: Called tool {}({})", agent_id, tool, truncate(&args, 40)));
                }
                AgentEvent::InferenceComplete { agent_id } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        agent.iteration += 1;
                    }
                    self.add_log(format!("{}: Inference complete", agent_id));
                }
                AgentEvent::Error { agent_id, error } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        agent.status = AgentDisplayStatus::Error(error.clone());
                    }
                    self.add_log(format!("{}: Error: {}", agent_id, error));
                }
                AgentEvent::Log(msg) => {
                    self.add_log(msg);
                }
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
                        let agent_id = self.agents.get(self.selected).map(|a| a.id.clone());
                        if let Some(agent) = self.agents.get_mut(self.selected) {
                            agent.waiting_prompt = None;
                            agent.status = AgentDisplayStatus::Active;
                        }
                        if let Some(id) = agent_id {
                            self.add_log(format!("Sent response to {}: {}", id, truncate(&input, 40)));
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
                let agent_id = self.agents.get(self.selected).map(|a| a.id.clone());
                if let Some(agent) = self.agents.get_mut(self.selected) {
                    if matches!(agent.status, AgentDisplayStatus::Active | AgentDisplayStatus::Waiting) {
                        agent.status = AgentDisplayStatus::Cancelled;
                    }
                }
                if let Some(id) = agent_id {
                    self.add_log(format!("{}: Cancelled", id));
                }
            }
            KeyCode::Char('k') => {
                let agent_id = self.agents.get(self.selected).map(|a| a.id.clone());
                if let Some(agent) = self.agents.get_mut(self.selected) {
                    agent.status = AgentDisplayStatus::Cancelled;
                }
                if let Some(id) = agent_id {
                    self.add_log(format!("{}: Killed", id));
                }
            }
            KeyCode::Char('n') => {
                self.add_log("New agent spawning is available via CLI: lev dashboard --task <task>".to_string());
            }
            _ => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(8),       // Agent table
                Constraint::Length(8),     // Detail panel
                Constraint::Length(8),     // Log panel
                Constraint::Length(1),     // Help bar
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
        let detail_text = if let Some(agent) = self.agents.get(self.selected) {
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
            vec![Line::from("No agents selected")]
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
        let help = if self.input_mode {
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

pub async fn execute(args: DashboardArgs) -> anyhow::Result<()> {
    let mut dashboard = Dashboard::new();

    // If a path+task was provided, set up the initial agent
    if let Some(ref path) = args.path {
        let task = args
            .task
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--task is required when a path is specified"))?;

        let project_path = Path::new(path);
        let manifest_path = if project_path.is_dir() {
            project_path.join("agent.leviath")
        } else {
            project_path.to_path_buf()
        };

        if manifest_path.exists() {
            let content = std::fs::read_to_string(&manifest_path)?;
            let blueprint = parse_manifest_public(&content)?;

            let stage_name = blueprint
                .stages
                .first()
                .map(|s| s.name.clone())
                .unwrap_or_else(|| "main".to_string());

            let agent = DashboardAgent {
                id: format!("{}-1", blueprint.name),
                blueprint_name: blueprint.name.clone(),
                stage: stage_name,
                status: AgentDisplayStatus::Active,
                tokens: (0, blueprint.context_layout.total_budget_tokens),
                iteration: 0,
                waiting_prompt: None,
            };

            dashboard.add_log(format!(
                "Agent {} started with task: {}",
                agent.id,
                truncate(task, 60)
            ));
            dashboard.agents.push(agent);
        } else {
            anyhow::bail!("Could not find agent.leviath at {}", manifest_path.display());
        }
    }

    // If no agents provided, add a placeholder message
    if dashboard.agents.is_empty() {
        dashboard.add_log("Dashboard started. Use 'n' to add agents or start with: lev dashboard <path> --task <task>".to_string());
    }

    // Enter TUI mode
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;

    let backend = ratatui::backend::CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    let tick_rate = Duration::from_millis(100); // ~10fps

    loop {
        // Process agent events
        dashboard.process_events();

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
