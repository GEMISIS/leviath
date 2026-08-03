//! Type definitions for the dashboard: display status, agent representation, events.

use clap::Args;

use super::graph::GraphTransitionInfo;
use super::theme::{C_ACTIVE, C_DIM, C_ERROR, C_SUCCESS, C_WARN};
use super::theme::{GLYPH_ACTIVE, GLYPH_COMPLETE, GLYPH_ERROR, GLYPH_PENDING, GLYPH_WAITING};

use crate::runstate::{self, StageRecord};
use leviath_core::interaction;

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

/// How the main run list is ordered. Whatever the mode, the order is a total
/// one (unique tie-break by id), so a status change alone never reshuffles
/// rows within a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SortMode {
    /// Newest run first, and a run keeps its row for its whole life. The
    /// default: predictable, nothing ever jumps.
    StartedAt,
    /// Most recently progressed run first: whatever just did something is on
    /// top. Rows move only on real progress, never on a status flip alone.
    RecentActivity,
    /// The old grouping: active first, finished below, stable within a group.
    StatusGrouped,
}

impl SortMode {
    pub(super) fn next(self) -> Self {
        match self {
            Self::StartedAt => Self::RecentActivity,
            Self::RecentActivity => Self::StatusGrouped,
            Self::StatusGrouped => Self::StartedAt,
        }
    }

    /// Short label for the table title.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::StartedAt => "started",
            Self::RecentActivity => "activity",
            Self::StatusGrouped => "status",
        }
    }
}

/// Which pane of the main screen holds keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MainPane {
    RunList,
    LogPane,
}

/// A pane with its own wheel-scroll behavior, hit-tested against the rects
/// each renderer registers per frame. Panes not listed here (detail content,
/// review) share the keyboard's scroll target via `scroll_by`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneId {
    RunTable,
    LogPanel,
}

/// A destructive action waiting on its confirmation dialog.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ConfirmAction {
    /// Cancel the run via the daemon (the row stays, marked cancelled).
    Kill { run_id: String },
    /// Cancel and permanently delete the run's on-disk state.
    Delete { run_id: String },
    /// Remove an MCP server from the config.
    McpRemove { name: String },
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
    /// Paused by the user; resumable with `r` (or `lev resume`). Distinct from
    /// `Idle` because a paused run is deliberate unfinished business, not a run
    /// that merely has not ticked yet.
    Paused,
    Cancelled,
    /// On disk the run claims to be live, but the daemon has no such run and its
    /// metadata has not been touched in a long time - so nothing is driving it.
    ///
    /// Shown distinctly rather than as ACTIVE because the two are not the same
    /// thing to the user: an ACTIVE row implies work is happening. Killable, like
    /// every other non-finished state.
    Stale,
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
            Self::Paused => write!(f, "{}PAUSED", GLYPH_PENDING),
            Self::Cancelled => write!(f, "⊘CANCEL"),
            Self::Stale => write!(f, "{}STALE", GLYPH_ERROR),
        }
    }
}

impl AgentDisplayStatus {
    /// Whether this run has finished, one way or another.
    pub(super) fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Complete | Self::CompleteInteractive | Self::Error(_) | Self::Cancelled
        )
    }

    /// Whether the run can be killed. Anything that has not finished can be -
    /// `Idle` and `Stale` included: skipping those would leave a run the
    /// dashboard shows as live with no way to get rid of it.
    pub(super) fn is_killable(&self) -> bool {
        !self.is_terminal()
    }

    pub(super) fn color(&self) -> Color {
        match self {
            Self::Active => C_ACTIVE,
            Self::Waiting => C_WARN,
            Self::Complete | Self::CompleteInteractive => C_SUCCESS,
            Self::Error(_) => C_ERROR,
            Self::Idle => C_DIM,
            Self::Paused => C_WARN,
            Self::Cancelled => C_DIM,
            Self::Stale => C_WARN,
        }
    }
}

/// An agent displayed in the dashboard.
#[derive(Debug, Clone)]
pub struct DashboardAgent {
    pub id: String,
    pub blueprint_name: String,
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
    /// Working directory the agent ran in
    pub workdir: String,
    /// Original task prompt
    pub task: String,
    /// Auto-generated short title (None until the worker generates it).
    pub title: Option<String>,
    /// Original model override
    pub model: Option<String>,
    /// Parent agent ID (if this is a sub-agent)
    pub parent_id: Option<String>,
    /// Depth in the sub-agent tree (0 = root)
    pub depth: usize,
    /// Unix timestamp when the run started (for elapsed display)
    pub started_at: i64,
    /// Unix timestamp of the run's last recorded progress (`None` before the
    /// first progress mark). Drives the recent-activity sort.
    pub last_progress_at: Option<i64>,
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

/// Log entry for the dashboard log panel.
#[derive(Debug, Clone)]
pub(super) struct LogEntry {
    pub(super) timestamp: String,
    pub(super) message: String,
}

/// Command sent from the dashboard's (sync) input handlers to the async
/// daemon-control background task, which forwards it over the control socket.
#[derive(Debug, PartialEq)]
pub(super) enum DaemonCommand {
    /// Cancel a run.
    Cancel { run_id: String },
    /// Pause a run.
    Pause { run_id: String },
    /// Resume a paused run.
    Resume { run_id: String },
    /// Answer a pending `ask_user` interaction.
    Answer {
        response: interaction::InteractionResponse,
    },
    /// Deliver a mid-run message to a running agent.
    Message { agent_id: String, content: String },
}

/// The result of a [`DaemonCommand`], drained each tick.
///
/// Discarding these would make a cancel the daemon refused look identical to
/// one that worked: the row flashes CANCEL, the log says "Killed", and the
/// next disk sync puts it back to ACTIVE with no explanation.
#[derive(Debug, PartialEq)]
pub(super) struct DaemonOutcome {
    /// The run the command targeted.
    pub(super) run_id: String,
    /// Human-readable result, shown as a toast when it failed.
    pub(super) message: String,
    /// Whether the daemon applied it.
    pub(super) ok: bool,
}

/// A long-running MCP action dispatched from the (sync) MCP screen to the async
/// background task, so browser login and connect-and-list never block the UI.
#[derive(Debug, PartialEq)]
pub(super) enum McpCommand {
    /// Run the OAuth browser login for a server.
    Login { name: String },
    /// Connect to a server and count its tools.
    Test { name: String },
}

/// The result of an [`McpCommand`], drained each tick and shown as a toast.
#[derive(Debug, PartialEq)]
pub(super) struct McpOutcome {
    /// Human-readable result to toast.
    pub(super) message: String,
    /// Whether it succeeded (drives the toast colour).
    pub(super) ok: bool,
}

/// One row of the MCP management screen.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct McpRow {
    pub(super) name: String,
    pub(super) transport: String,
    pub(super) endpoint: String,
    pub(super) auth: String,
}

/// Paths + injected seams the MCP screen's file/OAuth operations use, so the
/// whole screen is testable without the real home directory or a browser.
#[derive(Clone)]
pub(super) struct McpContext {
    pub(super) config_path: std::path::PathBuf,
    pub(super) store_path: std::path::PathBuf,
    pub(super) opener: leviath_mcp::BrowserOpener,
    pub(super) clock: fn() -> u64,
}

/// Toast notification shown as an overlay.
#[derive(Debug, Clone)]
pub(super) struct Toast {
    pub(super) message: String,
    pub(super) remaining_ticks: u32,
    pub(super) level: ToastLevel,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum ToastLevel {
    Info,
    Warning,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_display_status_display() {
        assert!(AgentDisplayStatus::Active.to_string().contains("ACTIVE"));
        assert!(AgentDisplayStatus::Waiting.to_string().contains("WAITING"));
        assert!(
            AgentDisplayStatus::Complete
                .to_string()
                .contains("COMPLETE")
        );
        assert!(
            AgentDisplayStatus::CompleteInteractive
                .to_string()
                .contains("COMPLETE")
        );
        assert!(
            AgentDisplayStatus::Error("boom".to_string())
                .to_string()
                .contains("boom")
        );
        assert!(AgentDisplayStatus::Idle.to_string().contains("IDLE"));
        assert!(AgentDisplayStatus::Paused.to_string().contains("PAUSED"));
        assert!(AgentDisplayStatus::Cancelled.to_string().contains("CANCEL"));
        assert!(AgentDisplayStatus::Stale.to_string().contains("STALE"));
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
        // Stale is a warning, not a finished state: it wants attention.
        assert_eq!(AgentDisplayStatus::Stale.color(), C_WARN);
        // Paused is deliberate unfinished business, not a dim afterthought.
        assert_eq!(AgentDisplayStatus::Paused.color(), C_WARN);
        assert!(!AgentDisplayStatus::Paused.is_terminal());
        assert!(AgentDisplayStatus::Paused.is_killable());
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
    fn daemon_command_debug_and_eq() {
        let cmd = DaemonCommand::Cancel {
            run_id: "run-123".to_string(),
        };
        let dbg = format!("{:?}", cmd);
        assert!(dbg.contains("run-123"));
        assert_eq!(
            cmd,
            DaemonCommand::Cancel {
                run_id: "run-123".to_string()
            }
        );
        assert_ne!(
            cmd,
            DaemonCommand::Message {
                agent_id: "a".to_string(),
                content: "b".to_string()
            }
        );
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
            stage: "plan".to_string(),
            stage_index: 0,
            num_stages: 2,
            status: AgentDisplayStatus::Active,
            tokens_in: 100,
            tokens_out: 50,
            cached_tokens: 0,
            iteration: 1,
            waiting_prompt: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            workdir: "/tmp".to_string(),
            task: "do stuff".to_string(),
            title: Some("My Task".to_string()),
            model: None,
            parent_id: None,
            depth: 0,
            started_at: 1000,
            last_progress_at: None,
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
