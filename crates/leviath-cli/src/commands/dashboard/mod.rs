//! `lev dash` - Interactive terminal UI for managing concurrent agents.

mod graph;
mod helpers;
mod input;
mod render;
mod state;
mod theme;
mod types;

pub use types::{AgentDisplayStatus, AgentEvent, DashboardAgent, DashboardArgs};

use crossterm::{
    event::{self, Event, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use leviath_runtime::AgentEngine;
use ratatui::Terminal;
use std::io::stdout;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

use super::run::build_provider_registry;
use crate::config::Config;

use state::Dashboard;
use types::EngineCommand;

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

    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;

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
                    dashboard.handle_key(key);
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
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;

    println!("Dashboard closed.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_args_can_be_constructed() {
        let _args = DashboardArgs {};
    }

    #[test]
    fn agent_display_status_variants_display() {
        let statuses = vec![
            AgentDisplayStatus::Active,
            AgentDisplayStatus::Waiting,
            AgentDisplayStatus::Complete,
            AgentDisplayStatus::CompleteInteractive,
            AgentDisplayStatus::Error("test error".to_string()),
            AgentDisplayStatus::Idle,
            AgentDisplayStatus::Cancelled,
        ];
        for status in statuses {
            let display = format!("{}", status);
            assert!(!display.is_empty());
        }
    }

    #[test]
    fn agent_event_log_variant() {
        let event = AgentEvent::Log("test message".to_string());
        let dbg = format!("{:?}", event);
        assert!(dbg.contains("test message"));
    }

    #[test]
    fn agent_event_stage_changed() {
        let event = AgentEvent::StageChanged {
            agent_id: "agent-1".to_string(),
            stage: "implement".to_string(),
        };
        let dbg = format!("{:?}", event);
        assert!(dbg.contains("agent-1"));
        assert!(dbg.contains("implement"));
    }

    #[test]
    fn agent_event_status_changed() {
        let event = AgentEvent::StatusChanged {
            agent_id: "agent-1".to_string(),
            status: AgentDisplayStatus::Complete,
        };
        let dbg = format!("{:?}", event);
        assert!(dbg.contains("agent-1"));
        assert!(dbg.contains("Complete"));
    }

    #[test]
    fn agent_event_needs_input() {
        let event = AgentEvent::NeedsInput {
            agent_id: "agent-1".to_string(),
            prompt: "What should I do?".to_string(),
        };
        let dbg = format!("{:?}", event);
        assert!(dbg.contains("What should I do?"));
    }

    #[test]
    fn agent_event_tool_called() {
        let event = AgentEvent::ToolCalled {
            agent_id: "agent-1".to_string(),
            tool: "read_file".to_string(),
            args: r#"{"path": "foo.txt"}"#.to_string(),
        };
        let dbg = format!("{:?}", event);
        assert!(dbg.contains("read_file"));
    }

    #[test]
    fn agent_event_error() {
        let event = AgentEvent::Error {
            agent_id: "agent-1".to_string(),
            error: "something broke".to_string(),
        };
        let dbg = format!("{:?}", event);
        assert!(dbg.contains("something broke"));
    }

    #[test]
    fn dashboard_agent_struct_fields() {
        let agent = DashboardAgent {
            id: "run-test".to_string(),
            blueprint_name: "tester".to_string(),
            agent_path: "/path".to_string(),
            stage: "init".to_string(),
            stage_index: 0,
            num_stages: 1,
            status: AgentDisplayStatus::Idle,
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
            is_run_state: false,
            pid: 0,
            workdir: "/tmp".to_string(),
            task: "test task".to_string(),
            title: None,
            model: None,
            parent_id: None,
            depth: 0,
            started_at: 0,
            active_until: None,
            waiting_secs: 0,
            graph_info: None,
            accepts_messages: false,
        };
        assert_eq!(agent.id, "run-test");
        assert_eq!(agent.blueprint_name, "tester");
        assert_eq!(agent.stage, "init");
        assert!(!agent.is_run_state);
    }
}
