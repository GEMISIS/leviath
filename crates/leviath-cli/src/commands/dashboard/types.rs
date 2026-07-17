//! Type definitions for the dashboard: display status, agent representation, events.

use clap::Args;
use tokio::sync::mpsc;

use super::graph::GraphTransitionInfo;
use super::theme::{C_ACTIVE, C_DIM, C_ERROR, C_SUCCESS, C_WARN};
use super::theme::{GLYPH_ACTIVE, GLYPH_COMPLETE, GLYPH_ERROR, GLYPH_PENDING, GLYPH_WAITING};

use crate::interaction;
use crate::runstate::{self, StageRecord};

use ratatui::style::Color;

#[derive(Args)]
pub struct DashboardArgs {}

/// Whether the detail content pane shows Output or Logs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum StageContentMode {
    Output,
    Logs,
    Context,
}

/// Display status for agents in the dashboard.
#[derive(Debug, Clone, PartialEq)]
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
    pub(super) fn color(&self) -> Color {
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
    /// Cumulative tokens read from provider cache.
    pub cached_tokens: usize,
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
    /// Parent agent ID (if this is a sub-agent)
    pub parent_id: Option<String>,
    /// Depth in the sub-agent tree (0 = root)
    pub depth: usize,
    /// Unix timestamp when the run started (for elapsed display)
    pub started_at: i64,
    /// Frozen wall-clock time (Unix seconds) when the agent entered a waiting state.
    /// Used to prevent the elapsed timer from incrementing while waiting for input.
    pub active_until: Option<i64>,
    /// Total seconds spent waiting for user input across all completed waits.
    /// Subtracted from elapsed to show only actual running time.
    pub waiting_secs: u64,
    /// Cached graph transition info (None = linear mode or not yet loaded)
    pub(super) graph_info: Option<GraphTransitionInfo>,
    /// Whether the current stage accepts mid-run user messages
    pub accepts_messages: bool,
    /// Per-region taint levels (region_name, taint_level_string).
    /// Empty when taint tracking is disabled or not yet populated.
    pub taint_summary: Vec<(String, String)>,
}

/// Event from an agent back to the dashboard.
#[derive(Debug, Clone)]
#[allow(dead_code)] // All variants are part of the agent event protocol
pub enum AgentEvent {
    StageChanged {
        agent_id: String,
        stage: String,
    },
    StatusChanged {
        agent_id: String,
        status: AgentDisplayStatus,
    },
    NeedsInput {
        agent_id: String,
        prompt: String,
    },
    ToolCalled {
        agent_id: String,
        tool: String,
        args: String,
    },
    InferenceComplete {
        agent_id: String,
        content: String,
        tokens_used: usize,
        tokens_prompt: usize,
    },
    Error {
        agent_id: String,
        error: String,
    },
    Log(String),
    AgentDone {
        agent_id: String,
    },
}

/// Log entry for the dashboard log panel.
#[derive(Debug, Clone)]
pub(super) struct LogEntry {
    pub(super) timestamp: String,
    pub(super) message: String,
}

/// Command sent from the dashboard to the engine background task.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum EngineCommand {
    CancelAgent { agent_id: String },
    SendInput { agent_id: String, input: String },
}

/// Toast notification shown as an overlay.
#[derive(Debug, Clone)]
pub(super) struct Toast {
    pub(super) message: String,
    pub(super) remaining_ticks: u32,
    pub(super) level: ToastLevel,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub(super) enum ToastLevel {
    Info,
    Warning,
    Error,
}

/// Convenience constructor for creating channels.
pub(super) fn create_event_channel() -> (
    mpsc::UnboundedSender<AgentEvent>,
    mpsc::UnboundedReceiver<AgentEvent>,
) {
    mpsc::unbounded_channel()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_display_status_display() {
        assert!(AgentDisplayStatus::Active.to_string().contains("ACTIVE"));
        assert!(AgentDisplayStatus::Waiting.to_string().contains("WAITING"));
        assert!(AgentDisplayStatus::Complete
            .to_string()
            .contains("COMPLETE"));
        assert!(AgentDisplayStatus::CompleteInteractive
            .to_string()
            .contains("COMPLETE"));
        assert!(AgentDisplayStatus::Error("boom".to_string())
            .to_string()
            .contains("boom"));
        assert!(AgentDisplayStatus::Idle.to_string().contains("IDLE"));
        assert!(AgentDisplayStatus::Cancelled.to_string().contains("CANCEL"));
    }

    #[test]
    fn agent_display_status_colors_are_distinct() {
        let active = AgentDisplayStatus::Active.color();
        let error = AgentDisplayStatus::Error("x".to_string()).color();
        let success = AgentDisplayStatus::Complete.color();
        assert_ne!(active, error);
        assert_ne!(error, success);
    }

    #[test]
    fn agent_display_status_color_idle_and_cancelled() {
        assert_eq!(AgentDisplayStatus::Idle.color(), C_DIM);
        assert_eq!(AgentDisplayStatus::Cancelled.color(), C_DIM);
        assert_eq!(AgentDisplayStatus::Waiting.color(), C_WARN);
        assert_eq!(AgentDisplayStatus::CompleteInteractive.color(), C_SUCCESS);
    }

    #[test]
    fn stage_content_mode_equality() {
        assert_eq!(StageContentMode::Output, StageContentMode::Output);
        assert_ne!(StageContentMode::Output, StageContentMode::Logs);
        assert_ne!(StageContentMode::Logs, StageContentMode::Context);
    }

    #[test]
    fn toast_level_debug() {
        let toast = Toast {
            message: "hello".to_string(),
            remaining_ticks: 25,
            level: ToastLevel::Info,
        };
        let dbg = format!("{:?}", toast);
        assert!(dbg.contains("hello"));
        assert!(dbg.contains("25"));
    }

    #[test]
    fn engine_command_debug() {
        let cmd = EngineCommand::CancelAgent {
            agent_id: "run-123".to_string(),
        };
        let dbg = format!("{:?}", cmd);
        assert!(dbg.contains("run-123"));
    }

    #[test]
    fn agent_event_debug() {
        let ev = AgentEvent::Log("hello world".to_string());
        let dbg = format!("{:?}", ev);
        assert!(dbg.contains("hello world"));
    }

    #[test]
    fn event_channel_works() {
        let (tx, mut rx) = create_event_channel();
        tx.send(AgentEvent::Log("test".to_string())).unwrap();
        let msg = rx.try_recv().unwrap();
        assert!(matches!(msg, AgentEvent::Log(s) if s == "test"));
    }

    #[test]
    fn log_entry_clone() {
        let entry = LogEntry {
            timestamp: "12:00:00".to_string(),
            message: "started".to_string(),
        };
        let cloned = entry.clone();
        assert_eq!(cloned.timestamp, "12:00:00");
        assert_eq!(cloned.message, "started");
    }

    #[test]
    fn dashboard_agent_clone() {
        let agent = DashboardAgent {
            id: "run-1".to_string(),
            blueprint_name: "coder".to_string(),
            agent_path: "/path".to_string(),
            stage: "plan".to_string(),
            stage_index: 0,
            num_stages: 2,
            status: AgentDisplayStatus::Active,
            tokens_in: 100,
            tokens_out: 50,
            cached_tokens: 0,
            context_tokens: (0, 0),
            iteration: 1,
            waiting_prompt: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            entity: bevy_ecs::prelude::Entity::from_raw(0),
            is_run_state: true,
            pid: 0,
            workdir: "/tmp".to_string(),
            task: "do stuff".to_string(),
            title: Some("My Task".to_string()),
            model: None,
            parent_id: None,
            depth: 0,
            started_at: 1000,
            active_until: None,
            waiting_secs: 0,
            graph_info: None,
            accepts_messages: true,
            taint_summary: vec![],
        };
        let cloned = agent.clone();
        assert_eq!(cloned.id, "run-1");
        assert_eq!(cloned.blueprint_name, "coder");
        assert_eq!(cloned.stage, "plan");
        assert_eq!(cloned.tokens_in, 100);
    }

    #[test]
    fn agent_display_status_complete_interactive_shows_complete() {
        let status = AgentDisplayStatus::CompleteInteractive;
        let display = status.to_string();
        assert!(display.contains("COMPLETE"));
        assert_eq!(status.color(), C_SUCCESS);
    }
}
