//! Dashboard state struct and core state-management methods.

use leviath_runtime::control_socket::ControlClient;
use ratatui::widgets::TableState;
use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::tui::widgets::markdown_edit::MarkdownEdit;

use super::graph::load_stage_graph;
use super::helpers::truncate;
use super::types::*;
use crate::runstate::{self, RunStatus};
use leviath_core::interaction;

/// The interactive dashboard state.
pub(crate) struct Dashboard {
    pub(super) agents: Vec<DashboardAgent>,
    pub(super) selected: usize,
    pub(super) log: Vec<LogEntry>,
    /// The long-form response editor (active when input_mode = true and the
    /// interaction takes free text). Soft-wrapped and markdown-aware, the same
    /// component the new-run task and the blueprint prompts use.
    pub(super) input_textarea: MarkdownEdit,
    pub(super) input_mode: bool,
    /// True when the full-screen detail view is open for the selected agent
    pub(super) detail_view: bool,
    pub(super) cmd_tx: mpsc::UnboundedSender<DaemonCommand>,
    /// Open interactions the daemon is holding, keyed by agent/run id. Refreshed
    /// each tick from `ListInteractions` so waiting agents show their prompt.
    pub(super) pending_interactions: HashMap<String, interaction::InteractionRequest>,
    pub(super) table_state: TableState,
    pub(super) should_quit: bool,
    /// A destructive action (kill / delete / MCP remove) waiting on its
    /// confirmation dialog. While open, the dialog owns the keys.
    pub(super) pending_confirm: Option<(ConfirmAction, crate::tui::widgets::confirm::Confirm)>,
    /// How the run list is ordered; `s` cycles it.
    pub(super) sort_mode: SortMode,
    /// Run ids marked with Space on the main list, so a kill or delete can act
    /// on several runs at once. Keyed by id rather than row position, so marks
    /// survive re-sorting and filtering; ids whose runs disappear are pruned by
    /// [`update_display_indices`](Self::update_display_indices).
    pub(super) marked: std::collections::HashSet<String>,
    /// Which main-screen pane holds keyboard focus (Tab toggles).
    pub(super) main_focus: MainPane,
    /// Scroll position of the log panel (tail-anchored; End resumes tailing).
    pub(super) log_scroll: crate::tui::widgets::scroll::ScrollState,
    /// Wheel-scroll hit-testing rects, re-registered by each pane's renderer
    /// every frame so the wheel always moves the pane under the cursor.
    pub(super) pane_rects: Vec<(PaneId, ratatui::layout::Rect)>,
    /// The graph pane holding the mouse between a left press and its
    /// release, so a pan that leaves the canvas keeps panning.
    pub(super) mouse_capture: Option<PaneId>,
    /// What a left click acts on, re-registered by each renderer every frame
    /// (see [`ClickTarget`]). Hit-tested last-match-first, so a target drawn
    /// inside another wins.
    pub(super) click_targets: Vec<(ratatui::layout::Rect, ClickTarget)>,
    /// The cell and time of the last plain click, so a second click on the
    /// same cell inside [`DOUBLE_CLICK_MS`](super::click::DOUBLE_CLICK_MS)
    /// reads as a double click.
    pub(super) last_click: Option<((u16, u16), u64)>,
    /// Monotonic milliseconds, injected so double-click detection is testable
    /// without sleeping (mirroring `clock`/`yank_fn`).
    pub(super) mouse_clock: fn() -> u64,
    /// The detail view's graph band, kept between frames; rebuilt when the
    /// selected run changes.
    pub(super) detail_band: Option<super::detail_band::DetailBand>,
    /// The explorer's canvas from its last visit to a run, so reopening
    /// finds the boxes where they were dragged and the view where it was.
    pub(super) explorer_cache: Option<(String, crate::tui::flowgraph::FlowView)>,
    /// The new-run screen's blueprint preview, for the selected catalog row.
    pub(super) new_run_preview: Option<super::new_run_preview::BlueprintPreview>,
    /// The log panel's viewport height as of the last draw, so key scrolling
    /// pages by what is actually visible.
    pub(super) log_viewport: usize,
    /// The full-screen stage explorer, when open (`g` in the detail view).
    pub(super) stage_explorer: Option<ExplorerState>,
    /// Cursor + expansion state of the structured Context view.
    pub(super) context_tree: ContextTreeState,
    /// Cached archive of the selected run (points + visit timeline).
    pub(super) history: Option<super::history::RunHistoryCache>,
    /// Loads a run's archived points. Injected (mirroring `clock`/`yank_fn`)
    /// so tests can count loads and prove `,`/`.` no longer re-read the
    /// archive per keypress.
    pub(super) history_loader: fn(&str) -> Vec<leviath_core::run_archive::RunPoint>,
    /// Scroll offset for detail view content: 0 = bottom (auto-scroll), >0 = scrolled up
    pub(super) detail_scroll: usize,
    /// Selected option index for MultipleChoice/ToolApproval/Confirm input
    pub(super) choice_selected: usize,
    /// Which stage tab is currently focused in the detail view
    pub(super) selected_stage: usize,
    /// Whether the content pane shows Output or Logs - global across all stage tabs.
    pub(super) stage_content_mode: StageContentMode,
    /// Which historical context point is being viewed: `None` = the live current
    /// window (the default), `Some(i)` = archived point `i` in the cached
    /// history (see `history`).
    pub(super) context_history_idx: Option<usize>,
    /// True after the first sync completes; suppresses startup toasts for pre-existing state.
    pub(super) initial_sync_done: bool,
    // ── Poll caches ───────────────────────────────────────────────────────────
    // The sync tick runs at 10Hz and used to re-read and re-parse every run's
    // meta.json, stages.json, and whole context.json on every tick - ~100 MB/s
    // of allocate-parse-free with 50 runs on disk, for files that change at
    // most once per persist tick. These stat-gated caches reduce the steady
    // state to stat calls.
    pub(super) meta_cache: runstate::StatCache<runstate::RunMeta>,
    pub(super) stages_cache: runstate::StatCache<Vec<leviath_core::run_meta::StageRecord>>,
    pub(super) context_cache: runstate::StatCache<runstate::ContextSnapshot>,
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
    /// Parent → child tree shape for each `display_indices` row (connector
    /// prefix, whether it folds, whether it is folded), parallel to
    /// `display_indices`. Default rows when filtering (the list is flat then).
    pub(super) tree_rows: Vec<RunTreeRow>,
    /// Runs whose sub-agents are folded away (`←` on the row, or a click on
    /// its arrow). Keyed by run id rather than row position so a fold survives
    /// re-sorting, filtering, and rows arriving above it - and, through
    /// `ui_state_path`, closing the dashboard.
    pub(super) collapsed_runs: std::collections::HashSet<String>,
    /// Where the remembered view state is kept between sessions (see
    /// `ui_state.rs`). `None` in tests and on a home with no data dir: nothing
    /// is read or written then, so no test can reach the user's real file.
    pub(super) ui_state_path: Option<std::path::PathBuf>,
    /// The agent the new-run screen opens on: the last one actually launched
    /// from here. `None` until somebody starts a run, and dropped back to the
    /// top of the list when that agent is no longer offered.
    pub(super) last_launched_agent: Option<String>,
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
    /// The active mouse text selection, if any (see `selection.rs`).
    pub(super) selection: Option<super::selection::Selection>,
    /// Screen rects of the panes that accept mouse selection, re-registered by
    /// each pane's renderer every frame so hit-testing always matches what is
    /// actually on screen.
    pub(super) selection_regions: Vec<ratatui::layout::Rect>,

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

    // ── New-run screen ─────────────────────────────────────────────────────
    /// True when the full-screen "start a run" view is open (`n`).
    pub(super) new_run_screen: bool,
    /// The agents the picker offers, rebuilt when the screen opens.
    pub(super) new_run_agents: Vec<NewRunAgent>,
    /// Type-to-filter query over the agent picker.
    pub(super) new_run_filter: String,
    /// Selected row of the *filtered* agent list.
    pub(super) new_run_selected: usize,
    /// The task editor. The same [`MarkdownEdit`] the response pane uses, so word
    /// motions, selection, and undo behave identically in both.
    pub(super) new_run_task: MarkdownEdit,
    /// Which of the two panes has the keys.
    pub(super) new_run_focus: NewRunPane,
    /// Which view every long-form box opens in: the markdown you type, or the
    /// rendered preview. One preference for all of them, remembered in
    /// `ui-state.json`, because it is a preference about reading rather than
    /// about any one box.
    pub(super) md_preview: bool,
    /// Workdir-relative file paths the `@` completion offers, walked once when
    /// the screen opens rather than per keystroke.
    pub(super) new_run_files: Vec<String>,
    /// True while an `@` file reference is being typed, so the completion
    /// popup has the keys.
    pub(super) new_run_file_ref: bool,
    /// The text typed after the `@`, held apart from the task buffer so
    /// accepting a completion knows how many characters to replace.
    pub(super) new_run_file_query: String,
    /// Highlighted row of the completion popup.
    pub(super) new_run_file_selected: usize,
    /// A run started from the new-run screen whose page to open once the
    /// daemon reports it, with the ticks left to wait for that.
    pub(super) pending_open_run: Option<(String, u32)>,
    /// How far the help overlay is scrolled.
    ///
    /// A `Cell` so drawing can clamp it: only the frame knows how tall the
    /// overlay came out, and an offset left past the end makes every press
    /// back up do nothing visible.
    pub(super) help_scroll: std::cell::Cell<usize>,
    /// Whether runs started from this screen run unattended.
    ///
    /// Off every time the screen opens is deliberate: the state carries real
    /// consequences, and a toggle that survives out of sight is one a user can
    /// leave on and forget.
    pub(super) new_run_yolo: bool,
    /// Whether the unattended warning has been silenced for this session.
    ///
    /// In memory only, and by design. "Do not ask again" is a statement about
    /// the sitting you are in, not a permanent preference, so closing the
    /// dashboard is what expires it. Persisting it to the config would turn one
    /// tick of a box into a machine-wide change nothing on screen mentions
    /// again.
    pub(super) yolo_warning_silenced: bool,
    /// Paths the screen reads its agents and file candidates from.
    pub(super) new_run_ctx: NewRunContext,
    /// The Agents screen (`a`): catalog and editor. Boxed: it carries a
    /// canvas and a document.
    pub(super) agent_builder: Option<Box<super::agents::AgentsScreen>>,
    /// Where the editor keeps canvas arrangements; `None` keeps them in
    /// memory (tests).
    pub(super) layout_store_path: Option<std::path::PathBuf>,
    /// A prompt handed to `$EDITOR`, waiting for the loop to leave the
    /// terminal and run it.
    pub(super) pending_external_edit: Option<super::agents::ExternalEdit>,
    /// Under which directory a prompt handed to `$EDITOR` is written while
    /// the editor runs. The file itself goes in a private scratch directory
    /// made under here on the first handoff (`external_edit_scratch`).
    pub(super) external_edit_dir: std::path::PathBuf,
    /// The owner-only, randomly named directory the handoff files live in,
    /// made on the first handoff and removed with the dashboard.
    ///
    /// A fixed `<temp>/leviath-dash-prompts/<stage>-system.md` was a path
    /// another local user could create first, as a directory of their own or
    /// a link to one, and read or replace the prompt through. A directory
    /// this process made, with a name nobody can guess, closes both.
    pub(super) external_edit_scratch: Option<tempfile::TempDir>,
    /// Sends resolve-and-spawn work to the background lane.
    pub(super) spawn_cmd_tx: mpsc::UnboundedSender<SpawnCommand>,
    /// Receives spawn results, drained into toasts each tick.
    pub(super) spawn_outcome_rx: mpsc::UnboundedReceiver<SpawnOutcome>,
    /// The background loop's ends of the spawn channels, taken by
    /// `init_dashboard`; tests keep them to assert dispatches and inject
    /// outcomes.
    pub(super) spawn_bg_ends: Option<(
        mpsc::UnboundedReceiver<SpawnCommand>,
        mpsc::UnboundedSender<SpawnOutcome>,
    )>,
    /// Receives the daemon's answer to each [`DaemonCommand`], drained each tick
    /// so a refused cancel is surfaced instead of silently reverting.
    pub(super) daemon_outcome_rx: mpsc::UnboundedReceiver<DaemonOutcome>,
    /// The background loop's sender for [`DaemonOutcome`]s; taken by
    /// `init_dashboard`, retained by tests to inject outcomes.
    pub(super) daemon_outcome_tx: Option<mpsc::UnboundedSender<DaemonOutcome>>,
    /// Receives each round of daemon polling, drained (never awaited) once a
    /// tick. See [`Self::drain_daemon_polls`].
    pub(super) daemon_poll_rx: mpsc::UnboundedReceiver<DaemonPoll>,
    /// The poll loop's sender; taken by `init_dashboard`, retained by tests to
    /// inject a round without a daemon.
    pub(super) daemon_poll_tx: Option<mpsc::UnboundedSender<DaemonPoll>>,
    /// The run ids the daemon currently holds, refreshed each tick. `None` when
    /// the daemon did not answer - the disk view is then taken at face value
    /// rather than declaring every run stale.
    pub(super) daemon_run_ids: Option<std::collections::HashSet<String>>,
    /// What the dashboard last knew about its link to the daemon, kept so a
    /// change is announced once (see [`Self::sync_daemon_link`]) and the run
    /// list can wear a chip while something is wrong.
    pub(super) daemon_link: DaemonLinkView,
    /// Wall-clock source, injected so staleness is testable without sleeping.
    pub(super) clock: fn() -> i64,
    /// The background loop's ends of the MCP channels. `init_dashboard` takes
    /// them to spawn [`super::mcp::mcp_background_loop`]; tests keep them to
    /// assert dispatched commands and inject outcomes.
    pub(super) mcp_bg_ends: Option<(
        mpsc::UnboundedReceiver<McpCommand>,
        mpsc::UnboundedSender<McpOutcome>,
    )>,
}

mod sync;

impl Dashboard {
    /// Drain the daemon's answers to this tick's commands. A refused command
    /// becomes a toast and a log line, so the row reverting on the next disk
    /// sync has a visible explanation rather than none.
    pub(super) fn drain_daemon_outcomes(&mut self) {
        while let Ok(outcome) = self.daemon_outcome_rx.try_recv() {
            if outcome.ok {
                continue;
            }
            Self::push_toast(
                &mut self.toasts,
                outcome.message.clone(),
                ToastLevel::Error,
                50,
            );
            self.add_log(format!("{}: {}", outcome.run_id, outcome.message));
        }
    }

    /// Recompute display_indices: ordered by [`SortMode`], filtered by
    /// list_search_query. Every mode's order is total (id tie-break), so a
    /// status change alone never reshuffles rows.
    pub(super) fn update_display_indices(&mut self) {
        // Drop marks whose runs no longer exist, so a deleted or vanished run
        // cannot linger in a later group kill or delete.
        let agents = &self.agents;
        self.marked.retain(|id| agents.iter().any(|a| a.id == *id));
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
                // Above the finished states: a stale run is unfinished business
                // the user probably wants to clear.
                AgentDisplayStatus::Stale => 2,
                // Same altitude as Stale: deliberately parked, but still the
                // user's unfinished business.
                AgentDisplayStatus::Paused => 2,
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
        let sort_mode = self.sort_mode;
        indices.sort_by(|&a, &b| {
            let (a, b) = (&self.agents[a], &self.agents[b]);
            let newest_first = b
                .started_at
                .cmp(&a.started_at)
                .then_with(|| a.id.cmp(&b.id));
            match sort_mode {
                // A run keeps its row for its whole life: nothing about this
                // key ever changes after start.
                SortMode::StartedAt => newest_first,
                // Most recent progress first; a run that never progressed
                // sorts by its start time. Rows move only when work happens.
                SortMode::RecentActivity => {
                    let ka = a.last_progress_at.unwrap_or(a.started_at);
                    let kb = b.last_progress_at.unwrap_or(b.started_at);
                    kb.cmp(&ka).then(newest_first)
                }
                SortMode::StatusGrouped => status_priority(&a.status)
                    .cmp(&status_priority(&b.status))
                    .then(newest_first),
            }
        });
        // With no filter, re-order into a parent → child tree so sub-agents and
        // fan-out workers nest under their parent (roots keep their recency order);
        // a filter keeps the flat sorted list (a partial match can't form a tree).
        let tree_rows: Vec<RunTreeRow> = if query.is_empty() {
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
            tree.into_iter().map(|(_, row)| row).collect()
        } else {
            vec![RunTreeRow::default(); indices.len()]
        };
        // Preserve selection: try to keep the same agent highlighted after recompute
        let prev_id = self
            .display_indices
            .get(self.selected)
            .and_then(|&i| self.agents.get(i))
            .map(|a| a.id.clone());
        self.display_indices = indices;
        self.tree_rows = tree_rows;
        self.forget_folds_for_vanished_runs();
        if let Some(id) = prev_id {
            // A fold can take the highlighted row off screen. Landing on the
            // ancestor that swallowed it keeps the eye where it was; row 0 is
            // a jump to the top of an unrelated run.
            self.selected = self
                .row_of_run(&id)
                .or_else(|| self.visible_ancestor_row(&id))
                .unwrap_or(0);
        }
        if self.display_indices.is_empty() {
            self.selected = 0;
            self.table_state.select(None);
        } else {
            self.selected = self.selected.min(self.display_indices.len() - 1);
            self.table_state.select(Some(self.selected));
        }
    }

    /// Open the selected run's page.
    ///
    /// One function so Enter and a just-started run land in the same state: a
    /// second copy of this drifted the moment either grew a field, and "the
    /// page I opened by hand behaves differently from the one that opened
    /// itself" is a bug nobody would think to look for.
    ///
    /// It deliberately does not remember where it was opened from. The new-run
    /// screen closes when the run is submitted, so Esc from here goes back to
    /// the list - which is where a person who has just started a run wants to
    /// be, not back in a form they have already filled in.
    pub(super) fn open_detail_view(&mut self) {
        self.detail_view = true;
        self.detail_scroll = 0;
        // Default to the stage the run is actually on.
        self.selected_stage = self.selected_agent().map(|a| a.stage_index).unwrap_or(0);
        self.reset_exploration();
        // …then put this run's Context tree back the way it was left. The reset
        // above still clears the explorer and the band, which are canvases for
        // a particular graph and have to be rebuilt for a different run.
        self.restore_context_tree();
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
    /// The cached archive is kept: it invalidates by run id and TTL, not here.
    pub(super) fn reset_context_history(&mut self) {
        self.context_history_idx = None;
    }

    /// Make sure the cached archive covers `run_id` and is not older than the
    /// TTL. This is the ONLY place the archive is loaded: `,`/`.` used to
    /// re-read and re-replay the whole `run.lvr` on every keypress.
    pub(super) fn ensure_history(&mut self, run_id: &str) {
        use super::history::{HISTORY_TTL_TICKS, RunHistoryCache};
        let fresh = self.history.as_ref().is_some_and(|h| {
            h.run_id == run_id
                && self.tick_count.saturating_sub(h.loaded_at_tick) < HISTORY_TTL_TICKS
        });
        if fresh {
            return;
        }
        // A run switch drops any browsed position along with the old archive.
        if self.history.as_ref().is_some_and(|h| h.run_id != run_id) {
            self.context_history_idx = None;
        }
        let points = (self.history_loader)(run_id);
        let visits = super::history::derive_visits(&points);
        self.history = Some(RunHistoryCache {
            run_id: run_id.to_string(),
            points,
            visits,
            loaded_at_tick: self.tick_count,
        });
    }

    /// The cached history, if it belongs to the selected run.
    pub(super) fn selected_history(&self) -> Option<&super::history::RunHistoryCache> {
        let id = self.selected_agent().map(|a| a.id.as_str())?;
        self.history.as_ref().filter(|h| h.run_id == id)
    }

    /// Step through the selected run's archived context-window history in the
    /// Context view: `delta > 0` moves to a later point, `delta < 0` to an
    /// earlier one. Reads the cached archive; stepping past the newest point
    /// returns to the live window. No-op if the run has no archived history.
    pub(super) fn step_context_history(&mut self, delta: isize) {
        let Some(run_id) = self.selected_agent().map(|a| a.id.clone()) else {
            return;
        };
        self.ensure_history(&run_id);
        let len = self.selected_history().map(|h| h.points.len()).unwrap_or(0);
        if len == 0 {
            self.reset_context_history();
            return;
        }
        let last = (len - 1) as isize;
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
        self.context_history_idx = new_idx.map(|i| i as usize);
        self.stage_content_mode = StageContentMode::Context;
        // The scroll position deliberately survives the step: comparing the
        // same spot across two points is the whole reason to browse history.
    }

    /// Jump the Context view straight to archived point `idx` (the timeline's
    /// Enter). Clamped; assumes `ensure_history` ran for the selected run.
    pub(super) fn jump_to_history_point(&mut self, idx: usize) {
        let len = self.selected_history().map(|h| h.points.len()).unwrap_or(0);
        if len == 0 {
            return;
        }
        self.context_history_idx = Some(idx.min(len - 1));
        self.stage_content_mode = StageContentMode::Context;
    }

    /// The snapshot the Context view is showing right now: the browsed
    /// archived point, else the selected stage's on-disk snapshot, else the
    /// run's live snapshot - the same fallback chain the renderer uses, so
    /// the key handler and the drawn tree can never disagree.
    pub(super) fn current_context_snapshot(&self) -> Option<runstate::ContextSnapshot> {
        let agent = self.selected_agent()?;
        self.browsed_context_point()
            .map(|p| p.context.clone())
            .or_else(|| runstate::read_stage_context(&agent.id, self.selected_stage))
            .or_else(|| agent.context_snapshot.as_deref().cloned())
    }

    /// The Context view's interactive rows under the current fold state.
    pub(super) fn context_tree_rows(&self) -> Vec<super::context_tree::TreeRow> {
        let searching = self.search_mode || !self.search_query.is_empty();
        self.current_context_snapshot()
            .map(|snap| super::context_tree::rows(&snap, &self.context_tree, searching))
            .unwrap_or_default()
    }

    /// The context snapshot to render in the Context view: the selected archived
    /// history point when browsing, else `None` (callers fall back to the live
    /// current window).
    pub(super) fn browsed_context_point(&self) -> Option<&leviath_core::run_archive::RunPoint> {
        let idx = self.context_history_idx?;
        let id = self.selected_agent().map(|a| a.id.as_str())?;
        self.history
            .as_ref()
            .filter(|h| h.run_id == id)
            .and_then(|h| h.points.get(idx))
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
    /// called while another field of `self` - e.g. `self.agents` - is borrowed.
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

    /// Take whatever the daemon poller has learned since the last tick.
    ///
    /// Only the newest round is applied: each carries the whole answer, so an
    /// older one describes a daemon state that has already been superseded.
    ///
    /// The polling itself happens on [`daemon_poll_loop`], not here. It used to
    /// be two `await`ed control round trips wedged between the tick and the
    /// draw, which meant a busy or restarting daemon stopped the dashboard
    /// dead - no redraw, no keys - for as long as it took to answer. Draining
    /// a channel cannot block, so the UI now keeps its own time whatever the
    /// daemon is doing.
    ///
    /// [`daemon_poll_loop`]: super::daemon_poll_loop
    pub(super) fn drain_daemon_polls(&mut self) {
        let mut newest = None;
        while let Ok(poll) = self.daemon_poll_rx.try_recv() {
            newest = Some(poll);
        }
        let Some(poll) = newest else {
            return;
        };
        // A request that did not come back leaves its state alone: the
        // best-effort reading both polls always had.
        if let Some(interactions) = poll.interactions {
            self.pending_interactions = interactions.into_iter().collect();
        }
        // Except this one, where `None` is itself the answer - "the daemon did
        // not say" is not "the daemon holds nothing", and the difference is
        // whether every run on disk gets condemned as stale. The poller sends
        // the `None` deliberately, so it is applied as given.
        self.daemon_run_ids = poll.run_ids;
    }

    /// Notice what this tick's polls learned about the daemon itself, and say
    /// it once: the daemon stopped answering, it is back (and whether it is
    /// the same daemon or a restarted one), or it came back on a different
    /// build than this dashboard.
    ///
    /// The polls above already ride out a restart on their own - they fail
    /// fast, and the next tick simply succeeds - so nothing here reconnects.
    /// It only announces, because a run list that silently went stale for two
    /// seconds and silently recovered leaves the user guessing what happened,
    /// and a daemon that came back updated is the one case where the fix is
    /// on the user's side of the socket.
    pub(super) fn sync_daemon_link(&mut self, control: &ControlClient) {
        use crate::commands::dashboard::types::ToastLevel;
        let link = control.link();
        let mismatch = control.code_mismatch().map(|m| m.to_string());

        match (self.daemon_link.unreachable, link.reachable) {
            (false, false) => {
                self.daemon_link.unreachable = true;
                self.add_log("The daemon stopped answering; reconnecting.".to_string());
                self.toast("daemon unreachable, reconnecting", ToastLevel::Warning);
            }
            (true, true) => {
                self.daemon_link.unreachable = false;
                let daemon = link
                    .daemon
                    .as_ref()
                    .map(|d| format!(", now {d}"))
                    .unwrap_or_default();
                let what = match link.restarts != self.daemon_link.restarts {
                    true => format!("The daemon restarted{daemon}; reconnected."),
                    false => format!("Reconnected to the daemon{daemon}."),
                };
                self.add_log(what);
                self.toast("reconnected to the daemon", ToastLevel::Info);
            }
            _ => {}
        }
        // A restart the polls never missed (fast enough that no tick failed)
        // still moves the count; note it so the next reconnect is not
        // misreported as one.
        self.daemon_link.restarts = link.restarts;

        if mismatch != self.daemon_link.mismatch {
            if let Some(text) = &mismatch {
                self.add_log(text.clone());
                self.toast(
                    "the daemon was updated; restart lev dash",
                    ToastLevel::Error,
                );
            }
            self.daemon_link.mismatch = mismatch;
        }
    }

    /// Cycle the run-list sort mode and say so in the log.
    ///
    /// Kept between sessions: somebody who works by status has said so, and
    /// saying it again at every launch is the setting failing to be one.
    pub(super) fn cycle_sort_mode(&mut self) {
        self.sort_mode = self.sort_mode.next();
        self.add_log(format!("Sort: {}", self.sort_mode.label()));
        self.save_ui_state();
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

    /// Build a tree-ordered list of agent indices with their row shape.
    ///
    /// Depth-first tree order over the given root agent indices (in the order
    /// supplied - the caller sorts them), nesting each root's children beneath
    /// it. A run in `collapsed_runs` still gets its own row, carrying the
    /// number of descendants the fold hides; none of them are emitted.
    /// Returns `Vec<(original_index, row)>`.
    pub(super) fn build_tree_order(&self, root_order: &[usize]) -> Vec<(usize, RunTreeRow)> {
        let mut result = Vec::new();
        for &idx in root_order {
            self.collect_tree_children(idx, "", &mut result, true);
        }
        result
    }

    /// The agent indices whose `parent_id` is `idx`'s id.
    fn children_of(&self, idx: usize) -> Vec<usize> {
        let agent_id = &self.agents[idx].id;
        self.agents
            .iter()
            .enumerate()
            .filter(|(_, a)| a.parent_id.as_deref() == Some(agent_id))
            .map(|(i, _)| i)
            .collect()
    }

    /// How many rows a fold on `idx` hides: its children, their children, and
    /// so on, whatever their own fold state (the number is what the row shows,
    /// and "4 hidden" that is really 9 would be a lie).
    fn descendant_count(&self, idx: usize) -> usize {
        self.children_of(idx)
            .into_iter()
            .map(|c| 1 + self.descendant_count(c))
            .sum()
    }

    fn collect_tree_children(
        &self,
        idx: usize,
        prefix: &str,
        result: &mut Vec<(usize, RunTreeRow)>,
        is_root: bool,
    ) {
        let children = self.children_of(idx);
        let collapsed = self.collapsed_runs.contains(&self.agents[idx].id);
        result.push((
            idx,
            RunTreeRow {
                prefix: if is_root {
                    String::new()
                } else {
                    prefix.to_string()
                },
                expandable: !children.is_empty(),
                collapsed: collapsed && !children.is_empty(),
                hidden: if collapsed {
                    self.descendant_count(idx)
                } else {
                    0
                },
            },
        ));
        if collapsed {
            return;
        }

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

    /// Drop folds naming runs that no longer exist, and write the store when
    /// that changed anything.
    ///
    /// ⚠️ Skipped while the run list is empty. Folds outlive the process now,
    /// and "no runs" is also what the very first tick sees, and what a runs
    /// directory that could not be read for a moment sees - pruning against
    /// that would delete every remembered fold on startup. An empty list has
    /// nothing to draw, so keeping the folds through it costs nothing; the
    /// first sync that can actually see runs does the pruning.
    fn forget_folds_for_vanished_runs(&mut self) {
        if self.agents.is_empty() {
            return;
        }
        let before = self.collapsed_runs.len();
        let agents = &self.agents;
        self.collapsed_runs
            .retain(|id| agents.iter().any(|a| a.id == *id));
        // Only on the tick a run actually disappears, not ten times a second.
        if self.collapsed_runs.len() != before {
            self.save_ui_state();
        }
    }

    /// The `display_indices` position of the row holding `run_id`, if it is
    /// on screen (a folded-away child is not).
    pub(super) fn row_of_run(&self, run_id: &str) -> Option<usize> {
        self.display_indices
            .iter()
            .position(|&i| self.agents.get(i).is_some_and(|a| a.id == run_id))
    }

    /// Walking up from `run_id`, the row of the first ancestor that is on
    /// screen - where a run that a fold just hid went.
    fn visible_ancestor_row(&self, run_id: &str) -> Option<usize> {
        let mut current = run_id.to_string();
        // Bounded by the agent count: a `parent_id` chain longer than that has
        // a cycle in it, and walking one forever would hang the draw loop.
        for _ in 0..self.agents.len() {
            let parent = self
                .agents
                .iter()
                .find(|a| a.id == current)?
                .parent_id
                .clone()?;
            if let Some(row) = self.row_of_run(&parent) {
                return Some(row);
            }
            current = parent;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::commands::dashboard::test_support::make_test_dashboard;
    // Only the tests speak the control protocol now: the polling itself moved
    // off the draw loop and onto `daemon_poll_loop`.
    use leviath_runtime::control_socket::{ControlRequest, ControlResponse};

    /// Root agent indices (no parent), in agent order - the input the production
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
            broken_scripts: Vec::new(),
            waiting_prompt: None,
            wait_reason: None,
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
            last_progress_at: None,
            runtime_secs: 0,
            clock_now: 0,
            graph: None,
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
        assert!(dash.pending_confirm.is_none());
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
        // The status-grouped mode is opt-in; the default (StartedAt) is tested
        // separately for stability.
        let mut dash = make_test_dashboard();
        dash.sort_mode = SortMode::StatusGrouped;
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

    /// The default order: newest start first, and a run keeps its row when its
    /// status changes - finishing must not move it.
    #[test]
    fn the_default_sort_is_stable_across_status_changes() {
        let mut dash = make_test_dashboard();
        let mut old = make_test_agent("run-old", AgentDisplayStatus::Active);
        old.started_at = 1_000;
        let mut new = make_test_agent("run-new", AgentDisplayStatus::Active);
        new.started_at = 2_000;
        dash.agents.push(old);
        dash.agents.push(new);
        dash.update_display_indices();
        let before: Vec<String> = dash
            .display_indices
            .iter()
            .map(|&i| dash.agents[i].id.clone())
            .collect();
        assert_eq!(before, ["run-new", "run-old"]);

        // The newest run finishes; in the old bucket sort it would leap below
        // the still-active one. Here it stays exactly where it was.
        dash.agents[1].status = AgentDisplayStatus::Complete;
        dash.update_display_indices();
        let after: Vec<String> = dash
            .display_indices
            .iter()
            .map(|&i| dash.agents[i].id.clone())
            .collect();
        assert_eq!(after, before, "a status change must not reshuffle rows");
    }

    /// Recent-activity mode: the run that progressed most recently leads;
    /// one that never progressed falls back to its start time.
    #[test]
    fn recent_activity_sorts_by_last_progress() {
        let mut dash = make_test_dashboard();
        dash.sort_mode = SortMode::RecentActivity;
        let mut a = make_test_agent("run-a", AgentDisplayStatus::Active);
        a.started_at = 1_000;
        a.last_progress_at = Some(5_000);
        let mut b = make_test_agent("run-b", AgentDisplayStatus::Active);
        b.started_at = 2_000;
        b.last_progress_at = Some(3_000);
        let mut c = make_test_agent("run-c", AgentDisplayStatus::Active);
        c.started_at = 4_000;
        c.last_progress_at = None;
        dash.agents.push(a);
        dash.agents.push(b);
        dash.agents.push(c);
        dash.update_display_indices();

        let ids: Vec<&str> = dash
            .display_indices
            .iter()
            .map(|&i| dash.agents[i].id.as_str())
            .collect();
        assert_eq!(ids, ["run-a", "run-c", "run-b"]);
    }

    #[test]
    fn cycle_sort_mode_walks_all_three_and_logs() {
        let mut dash = make_test_dashboard();
        assert_eq!(dash.sort_mode, SortMode::StartedAt);
        dash.cycle_sort_mode();
        assert_eq!(dash.sort_mode, SortMode::RecentActivity);
        dash.cycle_sort_mode();
        assert_eq!(dash.sort_mode, SortMode::StatusGrouped);
        dash.cycle_sort_mode();
        assert_eq!(dash.sort_mode, SortMode::StartedAt);
        assert!(dash.log.iter().any(|l| l.message.contains("Sort:")));
        assert_eq!(SortMode::StartedAt.label(), "started");
        assert_eq!(SortMode::RecentActivity.label(), "activity");
        assert_eq!(SortMode::StatusGrouped.label(), "status");
    }

    /// Paused sorts with Stale: above the finished states (it is the user's
    /// deliberately parked unfinished business), below Active/Waiting.
    #[test]
    fn update_display_indices_ranks_paused_above_finished_runs() {
        let mut dash = make_test_dashboard();
        dash.sort_mode = SortMode::StatusGrouped;
        dash.agents
            .push(make_test_agent("run-done", AgentDisplayStatus::Complete));
        dash.agents
            .push(make_test_agent("run-paused", AgentDisplayStatus::Paused));
        dash.agents
            .push(make_test_agent("run-live", AgentDisplayStatus::Active));
        dash.update_display_indices();

        assert_eq!(dash.agents[dash.display_indices[0]].id, "run-live");
        assert_eq!(dash.agents[dash.display_indices[1]].id, "run-paused");
        assert_eq!(dash.agents[dash.display_indices[2]].id, "run-done");
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

            // reset returns to live; the cache is kept (it invalidates by
            // run id and TTL, not by leaving the browse).
            dash.step_context_history(-1);
            dash.reset_context_history();
            assert_eq!(dash.context_history_idx, None);
            assert!(dash.history.is_some());
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
        });
    }

    /// The whole point of the cache: repeated `,`/`.` steps read the archive
    /// once, not once per keypress.
    #[test]
    fn stepping_history_loads_the_archive_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static LOADS: AtomicUsize = AtomicUsize::new(0);
        fn counting_loader(_run_id: &str) -> Vec<leviath_core::run_archive::RunPoint> {
            LOADS.fetch_add(1, Ordering::SeqCst);
            let mut meta = leviath_core::run_meta::RunMeta::new(
                "run-1".to_string(),
                "a".to_string(),
                "/p".to_string(),
                "t".to_string(),
                None,
                "/w".to_string(),
                1,
            );
            meta.current_stage = "main".to_string();
            let point = |at: i64| leviath_core::run_archive::RunPoint {
                meta: meta.clone(),
                context: leviath_core::run_meta::ContextSnapshot {
                    stage_name: "main".to_string(),
                    total_tokens: 0,
                    max_tokens: 100,
                    regions: vec![],
                },
                at,
            };
            vec![point(1), point(2), point(3)]
        }

        LOADS.store(0, Ordering::SeqCst);
        let mut dash = make_test_dashboard();
        dash.history_loader = counting_loader;
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();

        dash.step_context_history(-1);
        dash.step_context_history(-1);
        dash.step_context_history(1);
        dash.step_context_history(-1);
        assert_eq!(
            LOADS.load(Ordering::SeqCst),
            1,
            "four steps, one archive read"
        );
        assert_eq!(dash.context_history_idx, Some(1));

        // The visit timeline came along for free.
        assert_eq!(dash.selected_history().unwrap().visits.len(), 1);

        // Past the TTL the cache refreshes (a live run keeps growing).
        dash.tick_count += super::super::history::HISTORY_TTL_TICKS;
        dash.step_context_history(-1);
        assert_eq!(LOADS.load(Ordering::SeqCst), 2);
    }

    /// Switching runs drops the browsed position with the stale cache, and
    /// a cache for another run never serves the browsed point.
    #[test]
    fn a_run_switch_invalidates_the_cache_and_the_browsed_point() {
        fn one_point_loader(_run_id: &str) -> Vec<leviath_core::run_archive::RunPoint> {
            let meta = leviath_core::run_meta::RunMeta::new(
                "x".to_string(),
                "a".to_string(),
                "/p".to_string(),
                "t".to_string(),
                None,
                "/w".to_string(),
                1,
            );
            vec![leviath_core::run_archive::RunPoint {
                meta,
                context: leviath_core::run_meta::ContextSnapshot {
                    stage_name: "s".to_string(),
                    total_tokens: 0,
                    max_tokens: 100,
                    regions: vec![],
                },
                at: 1,
            }]
        }
        let mut dash = make_test_dashboard();
        dash.history_loader = one_point_loader;
        dash.agents
            .push(make_test_agent("run-a", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-b", AgentDisplayStatus::Active));
        dash.update_display_indices();

        dash.ensure_history("run-a");
        dash.context_history_idx = Some(0);
        // The cache belongs to run-a; with run-b selected the point is withheld.
        dash.selected = dash
            .display_indices
            .iter()
            .position(|&i| dash.agents[i].id == "run-b")
            .unwrap();
        assert!(dash.browsed_context_point().is_none());

        // Loading run-b's history resets the browsed index too.
        dash.ensure_history("run-b");
        assert_eq!(dash.context_history_idx, None);
        assert_eq!(dash.selected_history().unwrap().run_id, "run-b");
    }

    /// The Context tree reads the browsed point's snapshot while browsing,
    /// and a browsed index with no selected agent serves nothing.
    #[test]
    fn current_context_snapshot_prefers_the_browsed_point() {
        crate::runstate::with_isolated_runs_dir("dash-ctx-browsed-snap", |_d| {
            write_history_archive("run-b", 2);
            let mut dash = make_test_dashboard();
            dash.agents
                .push(make_test_agent("run-b", AgentDisplayStatus::Active));
            dash.update_display_indices();
            dash.step_context_history(-1);

            let snap = dash.current_context_snapshot().expect("browsed snapshot");
            assert_eq!(snap.stage_name, "stage1", "the archived point's window");

            // With no agent selected, a stale browsed index yields nothing.
            dash.display_indices.clear();
            assert!(dash.browsed_context_point().is_none());
            assert!(dash.current_context_snapshot().is_none());
        });
    }

    /// Jumping from the timeline lands on the requested (clamped) point.
    #[test]
    fn jump_to_history_point_clamps_and_switches_to_context() {
        crate::runstate::with_isolated_runs_dir("dash-ctx-hist-jump", |_d| {
            write_history_archive("run-j", 3);
            let mut dash = make_test_dashboard();
            dash.agents
                .push(make_test_agent("run-j", AgentDisplayStatus::Active));
            dash.update_display_indices();

            // Without history loaded, the jump is a no-op.
            dash.jump_to_history_point(1);
            assert_eq!(dash.context_history_idx, None);

            dash.ensure_history("run-j");
            dash.jump_to_history_point(1);
            assert_eq!(dash.context_history_idx, Some(1));
            assert_eq!(dash.stage_content_mode, StageContentMode::Context);
            dash.jump_to_history_point(99);
            assert_eq!(dash.context_history_idx, Some(2), "clamped to the last");
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
        assert_eq!(dash.tree_rows[0].prefix, ""); // root: no prefix
        assert!(dash.tree_rows[1].prefix.contains('─')); // child: tree connector
    }

    /// A grandparent's fold hides the whole subtree, and says how much of it.
    #[test]
    fn a_folded_run_hides_its_whole_subtree_and_counts_it() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("root", AgentDisplayStatus::Active));
        let mut child = make_test_agent("child", AgentDisplayStatus::Active);
        child.parent_id = Some("root".to_string());
        dash.agents.push(child);
        let mut grandchild = make_test_agent("grandchild", AgentDisplayStatus::Active);
        grandchild.parent_id = Some("child".to_string());
        dash.agents.push(grandchild);
        dash.update_display_indices();
        assert_eq!(dash.display_indices.len(), 3);
        assert!(dash.tree_rows[0].expandable);
        assert!(!dash.tree_rows[0].collapsed);
        assert!(!dash.tree_rows[2].expandable, "the leaf cannot fold");

        dash.collapsed_runs.insert("root".to_string());
        dash.update_display_indices();
        assert_eq!(dash.display_indices.len(), 1, "only the root is left");
        assert!(dash.tree_rows[0].collapsed);
        assert_eq!(
            dash.tree_rows[0].hidden, 2,
            "both descendants, not just the direct child"
        );
    }

    /// Folding the run the highlight was inside moves it to the fold, not to
    /// the top of the list.
    #[test]
    fn folding_a_parent_moves_the_highlight_onto_it() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("other", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("root", AgentDisplayStatus::Active));
        let mut child = make_test_agent("child", AgentDisplayStatus::Active);
        child.parent_id = Some("root".to_string());
        dash.agents.push(child);
        dash.update_display_indices();
        let child_row = dash.row_of_run("child").expect("the child is on screen");
        dash.selected = child_row;

        dash.collapsed_runs.insert("root".to_string());
        dash.update_display_indices();
        assert_eq!(dash.row_of_run("child"), None, "the child is folded away");
        assert_eq!(
            dash.selected,
            dash.row_of_run("root").unwrap(),
            "the highlight followed the fold that swallowed it"
        );
    }

    /// A `parent_id` cycle is walked at most once per run rather than forever.
    #[test]
    fn a_parent_cycle_does_not_hang_the_ancestor_walk() {
        let mut dash = make_test_dashboard();
        let mut a = make_test_agent("a", AgentDisplayStatus::Active);
        a.parent_id = Some("b".to_string());
        let mut b = make_test_agent("b", AgentDisplayStatus::Active);
        b.parent_id = Some("a".to_string());
        dash.agents.push(a);
        dash.agents.push(b);
        dash.update_display_indices();
        assert_eq!(dash.visible_ancestor_row("a"), None);
        // A parent that is not a run at all ends the walk too.
        assert_eq!(dash.visible_ancestor_row("nobody"), None);
    }

    /// A fold on a run that later disappears is dropped, so a run that reuses
    /// the id does not come back folded - but only once there is a run list to
    /// compare against. Folds outlive the process, and an empty list is also
    /// what the first tick and an unreadable runs dir look like.
    #[test]
    fn a_fold_is_forgotten_when_its_run_goes_but_not_while_the_list_is_empty() {
        let mut dash = make_test_dashboard();
        dash.collapsed_runs.insert("ghost".to_string());

        dash.update_display_indices();
        assert!(
            dash.collapsed_runs.contains("ghost"),
            "nothing to prune against yet"
        );

        // A list that exists and does not contain it: now it is really gone.
        dash.agents
            .push(make_test_agent("someone-else", AgentDisplayStatus::Active));
        dash.update_display_indices();
        assert!(dash.collapsed_runs.is_empty());
    }

    /// The prune writes the store, so a run deleted in one session does not
    /// come back folded in the next.
    #[test]
    fn forgetting_a_fold_rewrites_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui-state.json");
        let mut dash = make_test_dashboard();
        dash.ui_state_path = Some(path.clone());
        dash.collapsed_runs.insert("ghost".to_string());
        dash.save_ui_state();
        assert!(std::fs::read_to_string(&path).unwrap().contains("ghost"));

        dash.agents
            .push(make_test_agent("someone-else", AgentDisplayStatus::Active));
        dash.update_display_indices();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("ghost"));
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
        assert_eq!(dash.tree_rows[0].prefix, "");
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
        assert!(tree[0].1.prefix.is_empty()); // root has no prefix
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
        assert!(tree[1].1.prefix.contains("└─")); // last child connector
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
        // Re-sort without changing agents - selection should be preserved
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
        assert!(tree[0].1.prefix.is_empty()); // root has no prefix
        // First child
        assert_eq!(tree[1].0, 1);
        assert!(tree[1].1.prefix.contains("├─")); // not last child
        // Second child (last)
        assert_eq!(tree[2].0, 2);
        assert!(tree[2].1.prefix.contains("└─")); // last child
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
        assert!(tree[0].1.prefix.is_empty()); // root has no prefix
        assert_eq!(tree[1].0, 1); // child1
        assert!(tree[1].1.prefix.contains("├─")); // not last child
        assert_eq!(tree[2].0, 3); // grandchild (under child1)
        // grandchild should have deeper connector
        assert!(tree[2].1.prefix.len() > tree[1].1.prefix.len());
        assert_eq!(tree[3].0, 2); // child2
        assert!(tree[3].1.prefix.contains("└─")); // last child
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
    fn delete_of_an_unknown_run_id_is_a_noop() {
        let mut dash = make_test_dashboard();
        dash.perform_delete("no-such-run");
        // Should not panic, just no-op
    }

    // ─── update_display_indices: sort priority order ──────────────────────

    #[test]
    fn update_display_indices_sort_order_comprehensive() {
        let mut dash = make_test_dashboard();
        dash.sort_mode = SortMode::StatusGrouped;
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
        dash.agents.push(make_test_agent(
            "interactive",
            AgentDisplayStatus::CompleteInteractive,
        ));
        dash.update_display_indices();

        // Expected priority: Active(0) < Waiting(1) < CompleteInteractive(2)
        // < Complete(3) < Error(4) < Idle(5) < Cancelled(6)
        let ids: Vec<&str> = dash
            .display_indices
            .iter()
            .map(|&i| dash.agents[i].id.as_str())
            .collect();
        assert_eq!(ids[0], "active");
        assert_eq!(ids[1], "waiting");
        assert_eq!(ids[2], "interactive");
        assert_eq!(ids[3], "complete");
        assert_eq!(ids[4], "error");
        assert_eq!(ids[5], "idle");
        assert_eq!(ids[6], "cancelled");
    }

    #[test]
    fn perform_kill_of_an_unknown_run_id_still_sends_and_logs() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        dash.perform_kill("no-such-run");
        assert!(cmd_rx.try_recv().is_ok(), "the daemon is still asked");
        assert!(
            dash.log
                .iter()
                .any(|l| l.message.contains("kill requested"))
        );
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
        agent.pending_request = None; // no structured request - legacy path
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

                dash.perform_delete(&tmp_id);

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

            dash.perform_delete("test-run-missing");

            assert!(dash.log.iter().any(|l| l.message.contains("Delete failed")));
        });
    }

    // ─── delete: sub-agent trees ─────────────────────────────────────────

    /// Plant a run directory recorded as `parent`'s sub-agent, and return the
    /// matching list row. Both halves are needed: the rows are what the user
    /// sees, and the directories are what the delete has to remove.
    fn plant_agent(dash: &mut Dashboard, id: &str, parent: Option<&str>) {
        let mut meta = crate::runstate::RunMeta::new(
            id.to_string(),
            "agent".into(),
            "/p".into(),
            "task".into(),
            None,
            "/w".into(),
            1,
        );
        meta.parent_run_id = parent.map(str::to_string);
        crate::runstate::create_run(&meta).expect("run dir");
        let mut agent = make_test_agent(id, AgentDisplayStatus::Complete);
        agent.parent_id = parent.map(str::to_string);
        dash.agents.push(agent);
    }

    /// A sub-agent run is drawn nested under the run that spawned it, and
    /// nothing else on disk accounts for it. Deleting the parent alone left the
    /// children behind - and because the list treats a run whose parent is
    /// absent as a root, they did not even stay put: they jumped to the top
    /// level, so a delete read as a promotion.
    #[test]
    fn deleting_a_run_takes_its_sub_agent_rows_with_it() {
        crate::runstate::with_isolated_runs_dir("dash-delete-tree", |_d| {
            let mut dash = make_test_dashboard();
            plant_agent(&mut dash, "parent", None);
            plant_agent(&mut dash, "kid", Some("parent"));
            plant_agent(&mut dash, "grandkid", Some("kid"));
            plant_agent(&mut dash, "other", None);
            // One worker still going: it is cancelled through the daemon on the
            // way out, where a finished one is only taken off disk.
            dash.agents
                .iter_mut()
                .find(|a| a.id == "kid")
                .expect("the worker is listed")
                .status = AgentDisplayStatus::Active;
            dash.update_display_indices();

            dash.perform_delete("parent");

            let left: Vec<&str> = dash.agents.iter().map(|a| a.id.as_str()).collect();
            assert_eq!(left, vec!["other"], "the whole tree left the list");
            assert!(!crate::runstate::run_dir("parent").exists());
            assert!(!crate::runstate::run_dir("kid").exists());
            assert!(!crate::runstate::run_dir("grandkid").exists());
            assert!(crate::runstate::run_dir("other").exists());
        });
    }

    /// The tree is walked downwards only: deleting one fan-out worker is an
    /// ordinary thing to do, and must not take the run that started it or the
    /// workers beside it.
    #[test]
    fn deleting_a_sub_agent_row_leaves_its_parent_and_siblings() {
        crate::runstate::with_isolated_runs_dir("dash-delete-child-only", |_d| {
            let mut dash = make_test_dashboard();
            plant_agent(&mut dash, "parent", None);
            plant_agent(&mut dash, "kid-a", Some("parent"));
            plant_agent(&mut dash, "kid-b", Some("parent"));
            dash.update_display_indices();

            dash.perform_delete("kid-a");

            let left: Vec<&str> = dash.agents.iter().map(|a| a.id.as_str()).collect();
            assert_eq!(left, vec!["parent", "kid-b"]);
            assert!(crate::runstate::run_dir("parent").exists());
            assert!(crate::runstate::run_dir("kid-b").exists());
            assert!(!crate::runstate::run_dir("kid-a").exists());
        });
    }

    /// The sub-agents are rows the user did not select, so the confirmation
    /// says how many of them are about to go. A dialog that says "1 run" and
    /// removes four is the prompt lying about what the key does.
    #[test]
    fn the_delete_dialog_counts_the_sub_agent_rows_that_go_too() {
        let mut dash = make_test_dashboard();
        // No run directories: the count is read off the list on screen, which
        // is what the dialog is describing.
        let mut push = |id: &str, parent: Option<&str>| {
            let mut agent = make_test_agent(id, AgentDisplayStatus::Complete);
            agent.parent_id = parent.map(str::to_string);
            dash.agents.push(agent);
        };
        push("parent", None);
        push("kid", Some("parent"));
        push("grandkid", Some("kid"));
        push("lonely", None);
        dash.update_display_indices();

        // A run with a tree under it says so, counting the whole tree.
        dash.selected = dash.row_of_run("parent").expect("the parent is on screen");
        dash.request_delete();
        let (_, dialog) = dash.pending_confirm.take().expect("d opens the dialog");
        assert!(
            format!("{:?}", dialog.body).contains("Also deletes 2 sub-agent runs below it."),
            "{:?}",
            dialog.body
        );

        // One child reads as one, and a run with nothing under it says nothing
        // at all rather than "0 sub-agent runs".
        dash.selected = dash.row_of_run("kid").expect("the child is on screen");
        dash.request_delete();
        let (_, dialog) = dash.pending_confirm.take().expect("d opens the dialog");
        assert!(
            format!("{:?}", dialog.body).contains("Also deletes 1 sub-agent run below it."),
            "{:?}",
            dialog.body
        );

        dash.selected = dash
            .row_of_run("lonely")
            .expect("the lone run is on screen");
        dash.request_delete();
        let (_, dialog) = dash.pending_confirm.take().expect("d opens the dialog");
        assert_eq!(
            dialog.body.len(),
            1,
            "nothing extra to say: {:?}",
            dialog.body
        );
    }

    /// Marking a parent and one of its own children is ordinary, and the child
    /// must not be counted twice - it is one row either way.
    #[test]
    fn a_marked_parent_and_child_count_their_shared_tree_once() {
        let mut dash = make_test_dashboard();
        for (id, parent) in [
            ("parent", None),
            ("kid", Some("parent")),
            ("grandkid", Some("kid")),
        ] {
            let mut agent = make_test_agent(id, AgentDisplayStatus::Complete);
            agent.parent_id = parent.map(str::to_string);
            dash.agents.push(agent);
        }
        dash.update_display_indices();
        dash.marked.insert("parent".to_string());
        dash.marked.insert("kid".to_string());

        dash.request_delete();

        let (_, dialog) = dash.pending_confirm.take().expect("d opens the dialog");
        assert!(
            format!("{:?}", dialog.body).contains("Also deletes 1 sub-agent run below them."),
            "the grandchild is the only row not already named: {:?}",
            dialog.body
        );
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
    // runstate.rs's own `list_runs_returns_sorted` test does - unique run_ids
    // + inclusion checks (not exact-list assertions) so these coexist safely
    // with any other real runs on disk and with concurrently-running tests.

    fn make_run_meta(run_id: &str, status: RunStatus) -> runstate::RunMeta {
        let mut meta = runstate::RunMeta::new(
            run_id.to_string(),
            // Use the (unique-per-test) run_id as the agent name too, so toast
            // messages ("Agent run '<name>' ...") can be unambiguously matched even
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

    /// The context window is read only for the run the cursor is on.
    ///
    /// It is the largest file in a run directory by a wide margin, and the only
    /// thing that draws it is the detail view's context card - which draws one
    /// run. Reading every run's made launching the dashboard cost the whole
    /// history: measured on 750 runs holding 194 MB of `context.json` between
    /// them, the list took 1.4s to appear and the process sat at 267 MB, all to
    /// show one run's window.
    #[test]
    fn only_the_run_on_screen_has_its_context_read() {
        crate::runstate::with_isolated_runs_dir("sync-context-one-run", |_d| {
            let snapshot = runstate::ContextSnapshot {
                stage_name: "main".to_string(),
                total_tokens: 10,
                max_tokens: 100,
                regions: vec![],
            };
            for run_id in ["run-shown", "run-offscreen"] {
                runstate::create_run(&make_run_meta(run_id, RunStatus::Complete)).unwrap();
                runstate::write_context_snapshot(run_id, &snapshot).unwrap();
            }

            let mut dash = make_test_dashboard();
            // The first pass has no cursor to read from yet; the second is the
            // steady state every tick after it.
            dash.sync_from_run_state();
            dash.sync_from_run_state();

            let shown = dash
                .display_indices
                .get(dash.selected)
                .and_then(|&i| dash.agents.get(i))
                .map(|agent| agent.id.clone())
                .expect("a row is selected");
            assert_eq!(dash.agents.len(), 2, "both runs are listed");
            for agent in &dash.agents {
                if agent.id == shown {
                    assert!(
                        agent.context_snapshot.is_some(),
                        "the run on screen has its window read"
                    );
                } else {
                    assert!(
                        agent.context_snapshot.is_none(),
                        "{} is off screen; its window is not read",
                        agent.id
                    );
                }
            }
        });
    }

    // ─── staleness: disk says live, but nothing is driving it ───

    /// A clock far enough past a run's `updated_at` (1000) that it counts as
    /// untouched. Shared by every staleness test, including the ones whose
    /// checks short-circuit before reading the clock.
    fn stale_clock() -> i64 {
        1_000 + crate::runstate::STALE_AFTER_SECS * 100
    }

    /// A clock still inside the staleness window.
    fn fresh_clock() -> i64 {
        1_000 + crate::runstate::STALE_AFTER_SECS - 1
    }

    /// The reported bug's shape: a run whose `meta.json` claims `starting` /
    /// `running`, which the daemon does not hold and which has not been touched
    /// in a long time, is not ACTIVE - nothing is driving it.
    #[test]
    fn a_run_the_daemon_does_not_hold_and_has_not_moved_shows_as_stale() {
        crate::runstate::with_isolated_runs_dir("sync-stale-run", |_d| {
            for status in [RunStatus::Starting, RunStatus::Running] {
                let run_id = &format!("test-stale-{status}");
                cleanup_run(run_id);
                let mut meta = make_run_meta(run_id, status.clone());
                meta.updated_at = 1_000;
                runstate::create_run(&meta).unwrap();

                let mut dash = make_test_dashboard();
                dash.clock = stale_clock;
                // The daemon answered, and knows nothing about this run.
                dash.daemon_run_ids = Some(std::collections::HashSet::new());
                dash.sync_from_run_state();

                let agent = dash.agents.iter().find(|a| a.id == *run_id).unwrap();
                assert_eq!(agent.status, AgentDisplayStatus::Stale, "{status}");
                assert!(agent.status.is_killable(), "and it can still be killed");
                cleanup_run(run_id);
            }
        });
    }

    /// A run the daemon *does* hold is active however old its metadata looks -
    /// one long inference legitimately writes nothing for a while.
    #[test]
    fn a_run_the_daemon_holds_is_never_stale() {
        crate::runstate::with_isolated_runs_dir("sync-held-run", |_d| {
            let run_id = "test-held-run";
            cleanup_run(run_id);
            let mut meta = make_run_meta(run_id, RunStatus::Running);
            meta.updated_at = 1_000;
            runstate::create_run(&meta).unwrap();

            let mut dash = make_test_dashboard();
            dash.clock = stale_clock;
            dash.daemon_run_ids = Some([run_id.to_string()].into_iter().collect());
            dash.sync_from_run_state();

            let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
            assert_eq!(agent.status, AgentDisplayStatus::Active);
            cleanup_run(run_id);
        });
    }

    /// An unreachable daemon means "unknown", not "everything is dead" - the
    /// list is empty in both cases, so treating them alike would flip every
    /// healthy run to STALE the moment the socket blipped.
    #[test]
    fn an_unreachable_daemon_does_not_make_runs_look_stale() {
        crate::runstate::with_isolated_runs_dir("sync-daemon-unknown", |_d| {
            let run_id = "test-daemon-unknown";
            cleanup_run(run_id);
            let mut meta = make_run_meta(run_id, RunStatus::Running);
            meta.updated_at = 1_000;
            runstate::create_run(&meta).unwrap();

            let mut dash = make_test_dashboard();
            dash.clock = stale_clock;
            dash.daemon_run_ids = None; // no answer this tick
            dash.sync_from_run_state();

            let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
            assert_eq!(agent.status, AgentDisplayStatus::Active);
            cleanup_run(run_id);
        });
    }

    /// A run the daemon doesn't hold but which is still writing is active - it
    /// may simply not be registered yet (a just-spawned run, a fan-out worker).
    #[test]
    fn a_recently_updated_run_is_not_stale_even_if_unregistered() {
        crate::runstate::with_isolated_runs_dir("sync-recent-run", |_d| {
            let run_id = "test-recent-run";
            cleanup_run(run_id);
            let mut meta = make_run_meta(run_id, RunStatus::Running);
            meta.updated_at = 1_000;
            runstate::create_run(&meta).unwrap();

            let mut dash = make_test_dashboard();
            dash.clock = fresh_clock;
            dash.daemon_run_ids = Some(std::collections::HashSet::new());
            dash.sync_from_run_state();

            let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
            assert_eq!(agent.status, AgentDisplayStatus::Active);
            cleanup_run(run_id);
        });
    }

    /// A reachable daemon's answers are recorded; an unreachable one leaves
    /// "unknown" rather than an empty set, which would read as "nothing is
    /// running" and flip every healthy run to STALE.
    ///
    /// Drives the real polling path: one round against a socket, handed to the
    /// dashboard the way the background loop hands it over.
    #[tokio::test]
    async fn a_poll_round_records_the_daemons_answers_or_says_it_did_not_answer() {
        use leviath_runtime::control_socket::{bind_control_listener, control_id};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir().unwrap();
        let id = control_id(dir.path());
        let mut listener = bind_control_listener(&id).unwrap();
        // Serialized from the real type: a hand-written interaction body is a
        // test of my JSON, and the wire shape is the thing under test.
        let interactions = serde_json::to_string(&ControlResponse::Interactions {
            interactions: vec![(
                "run-1".to_string(),
                interaction::InteractionRequest::free_text("q1", "?", "main", true),
            )],
        })
        .unwrap()
            + "\n";
        // Two connections: a round asks two questions, each on its own.
        let server = tokio::spawn(async move {
            for reply in [
                interactions.into_bytes(),
                b"{\"result\":\"list\",\"runs\":[{\"run_id\":\"run-a\",\"status\":\"Active\",\
                   \"stage\":\"plan\",\"iteration\":1,\"tool_calls\":0}]}\n"
                    .to_vec(),
            ] {
                let stream = listener
                    .accept()
                    .await
                    .expect("accept succeeds")
                    .expect("our own connection is admitted");
                let (read_half, mut write_half) = tokio::io::split(stream);
                let mut lines = BufReader::new(read_half).lines();
                let _ = lines.next_line().await;
                let _ = write_half.write_all(&reply).await;
            }
        });

        let mut dash = make_test_dashboard();
        let polls = dash
            .take_daemon_poll_tx()
            .expect("a fresh dashboard has its poll sender");
        polls
            .send(super::super::poll_daemon(&ControlClient::new(id)).await)
            .unwrap();
        let _ = server.await;
        dash.drain_daemon_polls();
        assert_eq!(
            dash.daemon_run_ids,
            Some(["run-a".to_string()].into_iter().collect())
        );
        assert_eq!(
            dash.pending_interactions.get("run-1").map(|r| r.id.clone()),
            Some("q1".to_string())
        );

        // No daemon at all → unknown, not "none running", and the interactions
        // already on screen are left where they are rather than blanked.
        let gone = tempfile::tempdir().unwrap();
        polls
            .send(
                super::super::poll_daemon(&ControlClient::new(control_id(
                    &gone.path().join("no-daemon"),
                )))
                .await,
            )
            .unwrap();
        dash.drain_daemon_polls();
        assert_eq!(dash.daemon_run_ids, None);
        assert!(
            dash.pending_interactions.contains_key("run-1"),
            "a round that did not answer must not blank the prompts on screen"
        );
    }

    /// Only the newest round is applied. Each carries the whole answer, so an
    /// older one describes a daemon state that has already been superseded -
    /// and applying it would flicker the run list back a step.
    #[test]
    fn draining_the_polls_applies_only_the_newest_round() {
        let mut dash = make_test_dashboard();
        let polls = dash
            .take_daemon_poll_tx()
            .expect("a fresh dashboard has its poll sender");
        for id in ["old-run", "new-run"] {
            polls
                .send(crate::commands::dashboard::types::DaemonPoll {
                    interactions: None,
                    run_ids: Some([id.to_string()].into_iter().collect()),
                })
                .unwrap();
        }
        dash.drain_daemon_polls();
        assert_eq!(
            dash.daemon_run_ids,
            Some(["new-run".to_string()].into_iter().collect())
        );

        // Nothing queued: the last round stands rather than being cleared.
        dash.drain_daemon_polls();
        assert_eq!(
            dash.daemon_run_ids,
            Some(["new-run".to_string()].into_iter().collect())
        );
    }

    /// The messages the log pane holds, newest last.
    fn log_messages(dash: &Dashboard) -> Vec<String> {
        dash.log.iter().map(|e| e.message.clone()).collect()
    }

    /// The daemon stops answering, then answers again: each said once, in the
    /// log and as a toast, and the run list wears the chip in between. Polls
    /// that keep failing, or keep succeeding, add nothing.
    #[tokio::test]
    async fn the_link_announces_an_outage_and_the_return_once_each() {
        use crate::test_support::{TEST_BUILD, identified_daemon_in, same_code_daemon};
        use leviath_runtime::control_socket::{ControlResponse, ControlToken, control_id};
        {
            {
                let dir = tempfile::tempdir().unwrap();
                let _token = ControlToken::create(dir.path()).unwrap();
                let poll = ControlClient::for_home(control_id(dir.path()), dir.path())
                    .with_build(TEST_BUILD)
                    .with_reconnect_grace(std::time::Duration::ZERO);
                let mut dash = make_test_dashboard();
                dash.log.clear();
                dash.toasts.clear();
                assert!(dash.daemon_link.chip().is_none(), "healthy at first");

                // Two failed polls: one announcement.
                let _ = poll.request(&ControlRequest::List).await;
                dash.sync_daemon_link(&poll);
                let _ = poll.request(&ControlRequest::List).await;
                dash.sync_daemon_link(&poll);
                assert_eq!(
                    log_messages(&dash),
                    vec!["The daemon stopped answering; reconnecting.".to_string()]
                );
                assert_eq!(dash.toasts.len(), 1);
                assert!(dash.daemon_link.unreachable);
                let (chip, _) = dash.daemon_link.chip().expect("the chip is up");
                assert!(chip.contains("daemon unreachable"), "{chip}");

                // The daemon appears (the first one this dashboard has met, so
                // not a restart) and two polls succeed: one announcement.
                let (_c, _events, server) =
                    identified_daemon_in(dir.path(), same_code_daemon(4), 2, |_| {
                        ControlResponse::List {
                            runs: vec![],
                            finished: vec![],
                            health: Default::default(),
                        }
                    });
                let _ = poll.request(&ControlRequest::List).await;
                dash.sync_daemon_link(&poll);
                let _ = poll.request(&ControlRequest::List).await;
                dash.sync_daemon_link(&poll);
                server.await.unwrap();
                let messages = log_messages(&dash);
                assert_eq!(messages.len(), 2, "{messages:?}");
                assert!(
                    messages[1].starts_with("Reconnected to the daemon, now"),
                    "{messages:?}"
                );
                assert!(messages[1].contains("pid 4"), "{messages:?}");
                assert_eq!(dash.toasts.len(), 2);
                assert!(!dash.daemon_link.unreachable);
                assert!(dash.daemon_link.chip().is_none(), "the chip is down");
                assert_eq!(dash.daemon_link.restarts, 0);
            }
        }
    }

    /// A different daemon behind the socket is announced as a restart, and
    /// one on different code from this dashboard says so - once - and keeps
    /// the chip up, since that restart is the dashboard's to make.
    #[tokio::test]
    async fn the_link_names_a_restart_and_an_update() {
        use crate::test_support::{TEST_BUILD, identified_daemon_in, same_code_daemon};
        use leviath_runtime::control_socket::{ControlResponse, ControlToken, control_id};
        {
            {
                let dir = tempfile::tempdir().unwrap();
                let _token = ControlToken::create(dir.path()).unwrap();
                let poll = ControlClient::for_home(control_id(dir.path()), dir.path())
                    .with_build(TEST_BUILD)
                    .with_reconnect_grace(std::time::Duration::ZERO);
                let empty = |_| ControlResponse::List {
                    runs: vec![],
                    finished: vec![],
                    health: Default::default(),
                };
                let mut dash = make_test_dashboard();
                dash.log.clear();
                dash.toasts.clear();

                // The first daemon, met and lost.
                let (_c, _e, server) =
                    identified_daemon_in(dir.path(), same_code_daemon(1), 1, empty);
                let _ = poll.request(&ControlRequest::List).await;
                dash.sync_daemon_link(&poll);
                server.await.unwrap();
                assert!(
                    log_messages(&dash).is_empty(),
                    "meeting the daemon is not news"
                );
                let _ = poll.request(&ControlRequest::List).await;
                dash.sync_daemon_link(&poll);
                assert_eq!(log_messages(&dash).len(), 1, "gone");

                // Back as a different process on newer code.
                let mut newer = same_code_daemon(2);
                newer.build = "newer-build".to_string();
                let (_c, _e, server) = identified_daemon_in(dir.path(), newer, 2, empty);
                let _ = poll.request(&ControlRequest::List).await;
                dash.sync_daemon_link(&poll);
                let messages = log_messages(&dash);
                assert_eq!(messages.len(), 3, "{messages:?}");
                assert!(
                    messages[1].starts_with("The daemon restarted, now"),
                    "{messages:?}"
                );
                assert!(messages[1].contains("pid 2"), "{messages:?}");
                assert!(messages[2].contains("newer-build"), "{messages:?}");
                assert!(messages[2].contains("restart this process"), "{messages:?}");
                assert_eq!(dash.daemon_link.restarts, 1);
                let (chip, _) = dash.daemon_link.chip().expect("the chip stays up");
                assert!(chip.contains("restart lev dash"), "{chip}");
                assert_eq!(dash.toasts.len(), 3);

                // Said once: another successful poll adds nothing.
                let _ = poll.request(&ControlRequest::List).await;
                dash.sync_daemon_link(&poll);
                server.await.unwrap();
                assert_eq!(log_messages(&dash).len(), 3);
                assert_eq!(dash.toasts.len(), 3);

                // And a daemon back on this dashboard's own code clears it:
                // the chip comes down without a word.
                let (_c, _e, server) =
                    identified_daemon_in(dir.path(), same_code_daemon(3), 1, empty);
                let _ = poll.request(&ControlRequest::List).await;
                dash.sync_daemon_link(&poll);
                server.await.unwrap();
                assert!(dash.daemon_link.chip().is_none(), "the chip is down");
                assert_eq!(dash.daemon_link.mismatch, None);
                assert_eq!(log_messages(&dash).len(), 3, "nothing to say");
            }
        }
    }

    /// A stale run sorts above the finished ones: it is unfinished business the
    /// user probably wants to clear, not history.
    #[test]
    fn stale_sorts_above_the_finished_states() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("done", AgentDisplayStatus::Complete));
        dash.agents
            .push(make_test_agent("stale", AgentDisplayStatus::Stale));
        dash.agents
            .push(make_test_agent("live", AgentDisplayStatus::Active));
        dash.sort_mode = SortMode::StatusGrouped;
        dash.update_display_indices();

        let order: Vec<&str> = dash
            .display_indices
            .iter()
            .map(|&i| dash.agents[i].id.as_str())
            .collect();
        assert_eq!(order, vec!["live", "stale", "done"]);
    }

    // ─── daemon outcomes are surfaced ───

    /// A refused command becomes a visible toast + log line. Discarding it is
    /// what made a failed kill look like a successful one.
    #[test]
    fn a_refused_daemon_command_is_surfaced() {
        let mut dash = make_test_dashboard();
        let tx = dash
            .take_daemon_outcome_tx()
            .expect("a fresh dashboard has its outcome sender");
        tx.send(DaemonOutcome {
            run_id: "run-x".to_string(),
            message: "the daemon has no such run to cancel".to_string(),
            ok: false,
        })
        .unwrap();

        dash.drain_daemon_outcomes();

        assert!(
            dash.toasts
                .iter()
                .any(|t| t.message.contains("no such run")),
            "the failure is toasted"
        );
        assert!(
            dash.log.iter().any(|l| l.message.contains("run-x")),
            "and recorded in the activity log"
        );
    }

    #[test]
    fn a_successful_daemon_command_is_not_toasted() {
        let mut dash = make_test_dashboard();
        let tx = dash.take_daemon_outcome_tx().unwrap();
        tx.send(DaemonOutcome {
            run_id: "run-y".to_string(),
            message: String::new(),
            ok: true,
        })
        .unwrap();

        dash.drain_daemon_outcomes();

        assert!(dash.toasts.is_empty(), "success is silent");
        assert!(dash.log.iter().all(|l| !l.message.contains("run-y")));
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

    /// A paused run shows PAUSED with its timer stopped, both when an
    /// already-tracked ACTIVE row flips to paused on a later sync (the disk is
    /// authoritative - this is what makes the `p` key's optimistic flip stick
    /// instead of reverting to ACTIVE) and when the dashboard opens on a run
    /// that was already paused.
    ///
    /// The second case is the one that used to be wrong: the dashboard rebuilt
    /// the timer from transitions it had watched, so a pause it never saw start,
    /// because it was closed or because the run was paused before it opened,
    /// counted as work.
    #[test]
    fn sync_from_run_state_paused_run_shows_paused_with_a_stopped_timer() {
        crate::runstate::with_isolated_runs_dir("sync-paused-run", |_d| {
            let run_id = "test-sync-paused";
            cleanup_run(run_id);
            let mut meta = make_run_meta(run_id, RunStatus::Running);
            meta.active = Some(leviath_core::run_meta::ActiveClock {
                banked_secs: 30,
                since: Some(meta.started_at),
            });
            runstate::create_run(&meta).unwrap();

            let mut dash = make_test_dashboard();
            dash.sync_from_run_state();
            let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
            assert_eq!(agent.status, AgentDisplayStatus::Active);
            assert!(
                agent.runtime_secs >= 30,
                "a running run counts its open span too"
            );

            // The daemon pauses the run, banking the span, and persists it.
            meta.status = RunStatus::Paused;
            meta.active = Some(leviath_core::run_meta::ActiveClock {
                banked_secs: 45,
                since: None,
            });
            runstate::write_meta(&meta).unwrap();
            dash.sync_from_run_state();
            let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
            assert_eq!(agent.status, AgentDisplayStatus::Paused);
            assert_eq!(agent.runtime_secs, 45, "the timer stops while paused");

            // A dashboard opened while the run is already paused agrees, however
            // long ago the pause started.
            let mut fresh = make_test_dashboard();
            fresh.sync_from_run_state();
            let agent = fresh.agents.iter().find(|a| a.id == run_id).unwrap();
            assert_eq!(agent.status, AgentDisplayStatus::Paused);
            assert_eq!(agent.runtime_secs, 45);

            cleanup_run(run_id);
        });
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

                cleanup_run(run_id);
            },
        );
    }

    #[test]
    fn sync_from_run_state_running_agent_with_open_prompt_surfaces_it() {
        // The bug fix: a run whose persisted status is still `Running` but which
        // the daemon's hub reports an open interaction for (e.g. a tool-approval
        // prompt, which never flips the persisted status on its own) must show
        // as Waiting and surface the prompt - not sit silently Active.
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
                    &[],
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
        // the "new agent" branch of `sync_from_run_state` - every other
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
        // entirely on the next sync - even though the underlying
        // `RunStatus` does change - covering the `prev_status_was_active ==
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
                // A finished run's record is re-read once a second, not every
                // tick; this test is about the toast path, not the window, so
                // the cache forgets the run before the next sync.
                dash.meta_cache = Default::default();
                dash.sync_from_run_state(); // second sync: transitions Complete -> Error

                let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
                // `meta2` above has no `.error` set, so the mapped message defaults
                // to the empty string (`run.error.clone().unwrap_or_default()`).
                assert_eq!(agent.status, AgentDisplayStatus::Error(String::new()));
                // No transition toast fires - the block only runs when the agent
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
                // `dash.toasts.is_empty()` - the dashboard also picks up any other
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
    fn sync_from_run_state_existing_agent_enters_waiting_toasts_and_stops_timer() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_existing_agent_enters_waiting_toasts_and_stops_timer",
            |_d| {
                let run_id = "test-sync-existing-enters-waiting";
                cleanup_run(run_id);
                let mut meta = make_run_meta(run_id, RunStatus::Running);
                meta.active = Some(leviath_core::run_meta::ActiveClock {
                    banked_secs: 0,
                    since: Some(meta.started_at),
                });
                runstate::create_run(&meta).unwrap();

                let mut dash = make_test_dashboard();
                dash.sync_from_run_state();

                let mut meta2 = make_run_meta(run_id, RunStatus::WaitingInput);
                meta2.updated_at = meta.started_at + 42;
                meta2.active = Some(leviath_core::run_meta::ActiveClock {
                    banked_secs: 42,
                    since: None,
                });
                runstate::write_meta(&meta2).unwrap();
                let req = interaction::InteractionRequest::free_text("req1", "Q?", "main", true);
                dash.pending_interactions
                    .insert(run_id.to_string(), req.clone());
                dash.sync_from_run_state();

                let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
                assert_eq!(agent.status, AgentDisplayStatus::Waiting);
                assert_eq!(agent.runtime_secs, 42);
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

    /// Leaving a wait does not add the wait to the run's time.
    ///
    /// The dashboard no longer reconstructs any of this: the run's own clock is
    /// what it reads, so a wait it never observed costs nothing either.
    #[test]
    fn sync_from_run_state_leaving_a_wait_does_not_bill_the_wait() {
        crate::runstate::with_isolated_runs_dir(
            "sync_from_run_state_leaving_a_wait_does_not_bill_the_wait",
            |_d| {
                let run_id = "test-sync-existing-leaves-waiting";
                cleanup_run(run_id);
                let mut meta = make_run_meta(run_id, RunStatus::WaitingInput);
                meta.updated_at = meta.started_at + 10;
                meta.active = Some(leviath_core::run_meta::ActiveClock {
                    banked_secs: 10,
                    since: None,
                });
                runstate::create_run(&meta).unwrap();
                let req = interaction::InteractionRequest::free_text("req1", "Q?", "main", true);

                let mut dash = make_test_dashboard();
                dash.pending_interactions
                    .insert(run_id.to_string(), req.clone());
                dash.sync_from_run_state();
                let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
                assert_eq!(agent.runtime_secs, 10);

                // The answer lands 25 seconds later and the run goes again. The
                // daemon restarts the clock; none of the wait was banked.
                dash.pending_interactions.remove(run_id);
                let mut meta2 = make_run_meta(run_id, RunStatus::Running);
                let resumed_at = meta.updated_at + 25;
                meta2.updated_at = resumed_at;
                meta2.active = Some(leviath_core::run_meta::ActiveClock {
                    banked_secs: 10,
                    since: Some(resumed_at),
                });
                runstate::write_meta(&meta2).unwrap();
                dash.sync_from_run_state();

                let agent = dash.agents.iter().find(|a| a.id == run_id).unwrap();
                assert!(
                    agent.runtime_secs >= 10,
                    "the banked 10s survives the resume"
                );
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

                // Still waiting, same kind of request - re-sync must not toast again.
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
                // WaitingInput but no pending.json on disk (e.g. race/cleanup) - the
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
                // `has_tag` at least once - the real "completed" toast pushed by
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

    // ── Marks for group kill / delete ─────────────────────────────────────

    #[test]
    fn stale_marks_are_pruned_when_their_run_disappears() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.marked.insert("run-1".to_string());
        dash.marked.insert("run-2".to_string());
        dash.agents.retain(|a| a.id != "run-2");
        dash.update_display_indices();
        assert!(dash.marked.contains("run-1"), "the live run stays marked");
        assert!(!dash.marked.contains("run-2"), "the gone run is pruned");
    }

    #[test]
    fn marks_survive_filtering_and_resorting() {
        let mut dash = make_test_dashboard();
        let mut hidden = make_test_agent("run-1", AgentDisplayStatus::Active);
        hidden.blueprint_name = "alpha".to_string();
        dash.agents.push(hidden);
        let mut shown = make_test_agent("run-2", AgentDisplayStatus::Complete);
        shown.blueprint_name = "beta".to_string();
        dash.agents.push(shown);
        dash.update_display_indices();
        dash.marked.insert("run-1".to_string());

        // A filter that hides the marked run does not drop its mark.
        dash.list_search_query = "beta".to_string();
        dash.update_display_indices();
        assert_eq!(dash.display_indices.len(), 1);
        assert!(dash.marked.contains("run-1"));

        // Neither does re-sorting: marks key by id, not row.
        dash.list_search_query.clear();
        dash.cycle_sort_mode();
        assert!(dash.marked.contains("run-1"));
    }
}
