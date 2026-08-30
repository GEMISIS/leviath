//! How a [`Dashboard`] is built, and how it hands its background loops their
//! ends of the channels.
//!
//! Split out of `state.rs`, which is about what a running dashboard does. This
//! is about the moment before that: which paths and seams get injected (so no
//! test touches the real home directory, clipboard, or wall clock), and the
//! one-time handover of the MCP, spawn, and daemon-command channel ends to the
//! tasks that own them for the rest of the process.

use crate::tui::widgets::markdown_edit::MarkdownEdit;
use ratatui::widgets::TableState;
use std::collections::HashMap;
use tokio::sync::mpsc;

use super::state::Dashboard;
use super::types::*;
use crate::runstate;

/// Production clock for staleness checks: wall-clock Unix seconds.
fn system_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Production clock for double-click detection: milliseconds since the epoch.
/// Separate from [`system_now_secs`] because a second is far too coarse to
/// tell one click from two, and injected for the same reason - a test that
/// has to sleep to prove a double click is a test that fails on a busy CI box.
fn system_now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
            // No test built this way reaches a real handshake; the production
            // value keeps the fixture honest about what it stands in for.
            connect_timeout: leviath_mcp::DEFAULT_CONNECT_TIMEOUT,
        };
        // A test new-run context: an empty temp tree, so the agent picker and
        // the `@` completion never read the real agents dir or working
        // directory. Tests that need either point these at their own tempdir.
        let new_run_ctx = NewRunContext {
            agents_dir: std::env::temp_dir()
                .join("leviath-test-dashboard")
                .join("agents"),
            config_path: std::env::temp_dir()
                .join("leviath-test-dashboard")
                .join("config.toml"),
            workdir: std::env::temp_dir()
                .join("leviath-test-dashboard")
                .join("workdir"),
        };
        Self::new_with_log_path(cmd_tx, log_path, |_| false, ctx, new_run_ctx)
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
        new_run_ctx: NewRunContext,
    ) -> Self {
        let mut table_state = TableState::default();
        table_state.select(Some(0));

        // The MCP action lane: the dashboard keeps the command sender + outcome
        // receiver; the other ends go to the background loop (production) or are
        // retained for tests to drive.
        let (mcp_cmd_tx, mcp_cmd_rx) = mpsc::unbounded_channel();
        let (mcp_outcome_tx, mcp_outcome_rx) = mpsc::unbounded_channel();
        let (daemon_outcome_tx, daemon_outcome_rx) = mpsc::unbounded_channel();
        // The daemon-polling lane. Its whole point is that the draw loop never
        // waits on the socket: it drains this, and a daemon taking its time
        // costs a stale run list rather than a frozen terminal.
        let (daemon_poll_tx, daemon_poll_rx) = mpsc::unbounded_channel();
        // The spawn lane, split the same way: resolving a blueprint and the
        // socket round trip both happen off the draw loop.
        let (spawn_cmd_tx, spawn_cmd_rx) = mpsc::unbounded_channel();
        let (spawn_outcome_tx, spawn_outcome_rx) = mpsc::unbounded_channel();

        // Seed the in-memory log buffer from the tail of the persistent log so
        // the panel shows recent history immediately on launch (not a blank panel).
        let log = Self::load_log_seed(&log_path);

        Self {
            log_path,
            yank_fn,
            selection: None,
            selection_regions: Vec::new(),
            agents: Vec::new(),
            selected: 0,
            log,
            input_textarea: MarkdownEdit::default(),
            input_mode: false,
            response_focus_send: false,
            deny_feedback_open: false,
            detail_view: false,
            cmd_tx,
            pending_interactions: HashMap::new(),
            table_state,
            should_quit: false,
            pending_confirm: None,
            sort_mode: SortMode::StartedAt,
            marked: std::collections::HashSet::new(),
            main_focus: MainPane::RunList,
            log_scroll: crate::tui::widgets::scroll::ScrollState::default(),
            pane_rects: Vec::new(),
            mouse_capture: None,
            click_targets: Vec::new(),
            last_click: None,
            mouse_clock: system_now_millis,
            detail_band: None,
            explorer_cache: None,
            new_run_preview: None,
            log_viewport: 0,
            stage_explorer: None,
            context_tree: ContextTreeState::default(),
            history: None,
            history_loader: runstate::context_history,
            detail_scroll: 0,
            choice_selected: 0,
            selected_stage: 0,
            stage_content_mode: StageContentMode::Output,
            context_history_idx: None,
            initial_sync_done: false,
            meta_cache: runstate::StatCache::default(),
            stages_cache: runstate::StatCache::default(),
            context_cache: runstate::StatCache::default(),
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
            tree_rows: Vec::new(),
            collapsed_runs: std::collections::HashSet::new(),
            ui_state_path: None,
            last_launched_agent: None,
            mcp_screen: false,
            mcp_add_mode: false,
            mcp_add_input: String::new(),
            mcp_rows: Vec::new(),
            mcp_selected: 0,
            mcp_ctx,
            mcp_cmd_tx,
            mcp_outcome_rx,
            mcp_bg_ends: Some((mcp_cmd_rx, mcp_outcome_tx)),
            new_run_screen: false,
            new_run_agents: Vec::new(),
            new_run_filter: String::new(),
            new_run_selected: 0,
            new_run_task: MarkdownEdit::default(),
            md_preview: false,
            new_run_focus: NewRunPane::Agents,
            new_run_files: Vec::new(),
            new_run_file_ref: false,
            new_run_file_query: String::new(),
            new_run_file_selected: 0,
            pending_open_run: None,
            help_scroll: std::cell::Cell::new(0),
            new_run_yolo: false,
            new_run_ctx,
            agent_builder: None,
            layout_store_path: None,
            pending_external_edit: None,
            external_edit_dir: std::env::temp_dir(),
            external_edit_scratch: None,
            spawn_cmd_tx,
            spawn_outcome_rx,
            spawn_bg_ends: Some((spawn_cmd_rx, spawn_outcome_tx)),
            daemon_outcome_rx,
            daemon_outcome_tx: Some(daemon_outcome_tx),
            daemon_poll_rx,
            daemon_poll_tx: Some(daemon_poll_tx),
            daemon_run_ids: None,
            daemon_link: Default::default(),
            clock: system_now_secs,
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

    /// Take the background loop's ends of the spawn channels, so
    /// `init_dashboard` can spawn [`super::new_run::spawn_background_loop`].
    /// Returns `None` if already taken.
    pub(super) fn take_spawn_bg_ends(
        &mut self,
    ) -> Option<(
        mpsc::UnboundedReceiver<SpawnCommand>,
        mpsc::UnboundedSender<SpawnOutcome>,
    )> {
        self.spawn_bg_ends.take()
    }

    /// Take the daemon-outcome sender, so `init_dashboard` can hand it to
    /// [`super::daemon_background_loop`]. Returns `None` if already taken.
    pub(super) fn take_daemon_outcome_tx(
        &mut self,
    ) -> Option<mpsc::UnboundedSender<DaemonOutcome>> {
        self.daemon_outcome_tx.take()
    }

    /// Take the daemon-poll sender, so `init_dashboard` can hand it to
    /// [`super::daemon_poll_loop`]. Returns `None` if already taken.
    pub(super) fn take_daemon_poll_tx(
        &mut self,
    ) -> Option<mpsc::UnboundedSender<super::types::DaemonPoll>> {
        self.daemon_poll_tx.take()
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The production clock answers with a plausible wall-clock time (the tests
    /// above inject a fixed one, so this is the only place it runs).
    #[test]
    fn system_clock_reports_a_wall_clock_time() {
        // Well after 2020 and before 2100 - i.e. a real epoch second.
        let now = system_now_secs();
        assert!(now > 1_577_836_800 && now < 4_102_444_800, "got {now}");
    }

    /// The double-click clock reports the same instant in milliseconds (tests
    /// inject a frozen one, so this is the only place it runs).
    #[test]
    fn the_millisecond_clock_agrees_with_the_second_one() {
        let millis = system_now_millis();
        let secs = system_now_secs() as u64;
        assert!(millis / 1000 >= secs.saturating_sub(2), "got {millis}");
        assert!(millis / 1000 <= secs + 2, "got {millis}");
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
}
