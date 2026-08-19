//! Type definitions for the dashboard: display status, agent representation, events.

use clap::Args;

use super::theme::{C_ACTIVE, C_DIM, C_ERROR, C_SUCCESS, C_WARN};
use super::theme::{GLYPH_ACTIVE, GLYPH_COMPLETE, GLYPH_ERROR, GLYPH_PENDING, GLYPH_WAITING};
use crate::tui::flowgraph::FlowView;

use crate::runstate::{self, StageRecord};
use leviath_core::interaction;

use ratatui::style::Color;

/// Arguments for `lev dash`. It takes none; the dashboard is interactive.
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
    /// The stage explorer's graph canvas: the wheel zooms, a drag pans.
    ExplorerGraph,
    /// The detail view's graph band: a drag pans.
    DetailBand,
    /// The new-run screen's blueprint preview: a drag pans.
    NewRunPreview,
}

impl PaneId {
    /// Whether the pane is a graph canvas, which takes the mouse before the
    /// text-selection machinery sees it.
    pub(super) fn is_graph(self) -> bool {
        matches!(
            self,
            PaneId::ExplorerGraph | PaneId::DetailBand | PaneId::NewRunPreview
        )
    }
}

/// Which tab of the full-screen stage explorer is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExplorerTab {
    Graph,
    Timeline,
}

/// The full-screen stage explorer (`g` in the detail view): the blueprint's
/// stage graph on a canvas with the run painted onto it, and the visit
/// timeline.
#[derive(Debug)]
pub(super) struct ExplorerState {
    /// The run the canvas was built for; the canvas is kept for it after
    /// the explorer closes.
    pub(super) run_id: String,
    pub(super) tab: ExplorerTab,
    /// Selected row on the timeline tab.
    pub(super) timeline_selected: usize,
    /// The graph canvas. Owns the toggles (`u` unvisited, `e` escape edges),
    /// the selection, the direction and the viewport.
    pub(super) view: FlowView,
}

impl ExplorerState {
    pub(super) fn new(run_id: String, view: FlowView) -> Self {
        Self {
            run_id,
            tab: ExplorerTab::Graph,
            timeline_selected: 0,
            view,
        }
    }
}

/// Cursor + expansion state of the structured Context view.
///
/// Regions default to expanded (header + one-line entry stubs); entries
/// default to collapsed. The state survives ticks and history steps, and
/// resets only when the selected run changes.
#[derive(Debug, Clone, Default)]
pub(super) struct ContextTreeState {
    /// Regions whose entry list is folded away.
    pub(super) collapsed_regions: std::collections::HashSet<String>,
    /// `(region, entry_index)` pairs expanded to their full content.
    pub(super) expanded_entries: std::collections::HashSet<(String, usize)>,
    /// Cursor over the tree's interactive rows (headers + stubs).
    pub(super) cursor: usize,
    /// Set when a key moved the cursor, so the renderer scrolls to it once
    /// rather than pinning the view to the cursor forever.
    pub(super) follow_cursor: bool,
}

/// A destructive action waiting on its confirmation dialog.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ConfirmAction {
    /// Cancel the runs via the daemon (the rows stay, marked cancelled).
    /// Carries one id for the selected run, several when runs are marked.
    Kill { run_ids: Vec<String> },
    /// Cancel and permanently delete the runs' on-disk state.
    /// Carries one id for the selected run, several when runs are marked.
    Delete { run_ids: Vec<String> },
    /// Remove an MCP server from the config.
    McpRemove { name: String },
    /// Turn on unattended runs for the new-run screen.
    EnableYolo,
}

/// Display status for agents in the dashboard.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentDisplayStatus {
    /// Working.
    Active,
    /// Blocked on a person answering.
    Waiting,
    /// Finished, with nothing further to accept.
    Complete,
    /// All required work done; still accepting optional follow-up input.
    CompleteInteractive,
    /// Stopped by a failure, carrying its message.
    Error(String),
    /// Loaded but not currently doing anything.
    Idle,
    /// Paused by the user; resumable with `r` (or `lev resume`). Distinct from
    /// `Idle` because a paused run is deliberate unfinished business, not a run
    /// that merely has not ticked yet.
    Paused,
    /// Stopped from outside, by `lev kill` or a shutting-down daemon.
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
    /// The run id, which is also what every action against this row quotes.
    pub id: String,
    /// The blueprint's name, as the manifest declares it.
    pub blueprint_name: String,
    /// The stage the run is in, by name.
    pub stage: String,
    /// That stage's position in the blueprint's list.
    pub stage_index: usize,
    /// How many stages the blueprint has, so the pair renders as "3 of 7".
    pub num_stages: usize,
    /// What the row shows, including states no other status enum has.
    pub status: AgentDisplayStatus,
    /// Cumulative prompt (input) tokens for background runs.
    pub tokens_in: usize,
    /// Cumulative completion (output) tokens for background runs.
    pub tokens_out: usize,
    /// Cumulative tokens read from provider cache.
    pub cached_tokens: usize,
    /// Inference turns taken in the current stage.
    pub iteration: usize,
    /// The question a waiting run is asking, in one line, for the list row.
    pub waiting_prompt: Option<String>,
    /// Why a waiting run is parked, when `meta.json` says.
    ///
    /// WAITING on its own reads as "go and answer it", which is wrong for a
    /// parent whose fan-out workers are still churning. `None` on a run that
    /// is not parked, and on one written by a build from before the field
    /// existed, which is why the row falls back to the bare status rather
    /// than assuming.
    pub wait_reason: Option<leviath_core::run_meta::WaitReason>,
    /// Full structured interaction request (populated for WaitingInput agents)
    pub pending_request: Option<interaction::InteractionRequest>,
    /// The request_id we most recently submitted a response for, used to suppress
    /// re-showing the same prompt before the worker has consumed the response.
    pub last_answered_request_id: Option<String>,
    /// Live context window snapshot from context.json (background workers only)
    /// Shared, not owned: the live snapshot comes out of the sync tick's
    /// stat-gated cache, and cloning a full context window per tick was the
    /// churn that cache exists to remove.
    pub context_snapshot: Option<std::sync::Arc<runstate::ContextSnapshot>>,
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
    /// The blueprint's stage graph, loaded once when the run first appears.
    /// `None` when the manifest could not be read: the run still shows, the
    /// graph surfaces say why they are empty. Shared, not owned: the detail
    /// view clones the whole agent every frame.
    pub(super) graph: Option<std::sync::Arc<crate::tui::flowgraph::StageGraph>>,
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

/// The dashboard's view of its link to the daemon, refreshed each tick from
/// the control client's own bookkeeping.
///
/// The dashboard polls the daemon ten times a second, so it notices a restart
/// within a tick and needs no reconnect of its own; what it needs is to *say*
/// so, once, and to say when the daemon that came back runs different code
/// than this dashboard, since that is the one restart the dashboard should
/// follow. Both are edge-triggered off this record.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DaemonLinkView {
    /// Whether the last poll went unanswered. Starts `false`: the daemon was
    /// ensured before the dashboard opened.
    pub(super) unreachable: bool,
    /// The restart count last seen, so a return to a *different* daemon is
    /// announced as a restart rather than a blip.
    pub(super) restarts: u64,
    /// The mismatch last announced, so the warning fires once per daemon and
    /// the chip stays up until it is resolved.
    pub(super) mismatch: Option<String>,
}

impl DaemonLinkView {
    /// The chip the run list wears while something is wrong, and its colour;
    /// `None` when the link is healthy and nothing needs saying.
    pub(super) fn chip(&self) -> Option<(&'static str, Color)> {
        match (self.unreachable, &self.mismatch) {
            (true, _) => Some((" ⟳ daemon unreachable, reconnecting ", C_WARN)),
            (false, Some(_)) => Some((" ⚠ daemon updated: restart lev dash ", C_ERROR)),
            (false, None) => None,
        }
    }
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

/// Which pane of the new-run screen holds keyboard focus (Tab toggles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NewRunPane {
    Agents,
    Task,
}

/// One runnable agent offered by the new-run screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NewRunAgent {
    pub(super) name: String,
    /// Where it came from: `installed`, `configured`, `local`, or `bundled`.
    pub(super) source: String,
    pub(super) description: String,
    /// What gets handed to `lev run`'s resolver: the manifest's directory for a
    /// discovered agent, the bare name for a bundled one (which resolves only
    /// once `lev setup` has installed it - and says so if it has not).
    pub(super) path: String,
}

/// Where the new-run screen reads its agent catalog and its `@` file
/// candidates from, so the whole screen is testable against a temp tree
/// instead of the user's real home directory and working directory.
#[derive(Clone)]
pub(super) struct NewRunContext {
    /// `~/.leviath/agents`, scanned for installed agents.
    pub(super) agents_dir: std::path::PathBuf,
    /// The config whose `agent_paths` add more places to look.
    pub(super) config_path: std::path::PathBuf,
    /// The directory the run's tools are confined to, and the root the `@`
    /// completion offers files from.
    pub(super) workdir: std::path::PathBuf,
}

/// A run the new-run screen asked for, dispatched to the async spawn lane.
///
/// Resolving a blueprint reads and parses files and the spawn itself is a
/// socket round trip, so neither happens on the draw loop.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct SpawnCommand {
    /// The agent path or name to resolve.
    pub(super) agent_path: String,
    /// The task text as typed.
    pub(super) task: String,
    /// The working directory the run gets.
    pub(super) workdir: String,
    /// Whether the run approves its own tool calls.
    pub(super) yolo: bool,
}

/// The result of a [`SpawnCommand`], drained each tick and shown as a toast.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct SpawnOutcome {
    /// Human-readable result to toast.
    pub(super) message: String,
    /// Whether the run actually started (drives the toast colour).
    pub(super) ok: bool,
    /// The id the daemon gave it, so the dashboard can open its page.
    pub(super) run_id: Option<String>,
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
            wait_reason: None,
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
            graph: None,
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
