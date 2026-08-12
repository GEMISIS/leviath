//! New-run screen: the agent catalog, the `@` file completion, and the async
//! spawn lane.
//!
//! `lev dash` could only ever watch runs somebody else had started. This is the
//! other half: pick an agent, write the task, press Enter. The screen is modal
//! like the MCP one, and for the same reason - it owns the keys while open, so
//! typing a task can never also mean "kill the selected run".
//!
//! Resolving a blueprint reads and parses files, and the spawn is a socket
//! round trip, so both go to [`spawn_background_loop`] and come back as toasts.
//! `send_spawn` is deliberately not reused: it prints its report on stdout,
//! which would land in the middle of the alternate screen.

use std::collections::HashMap;
use std::path::Path;

use leviath_runtime::control_socket::{ControlClient, ControlResponse};
use tokio::sync::mpsc;

use super::state::Dashboard;
use super::types::{
    NewRunAgent, NewRunContext, NewRunPane, SpawnCommand, SpawnOutcome, ToastLevel,
};
use crate::commands::list::{ListFilter, build_list_report};
use crate::config::Config;
use crate::daemon::client::{LaunchRequest, never_interactive, resolve_spawn_args};

/// How many workdir files the `@` completion will offer.
///
/// A repository can hold hundreds of thousands of files; walking all of them
/// would stall the screen open, and the menu only ever shows the first handful
/// of matches anyway. Typing more of the path is what narrows it, not a longer
/// list.
const FILE_CANDIDATE_CAP: usize = 2_000;

/// How many completions are shown at once.
const FILE_REF_VISIBLE: usize = 8;

impl Dashboard {
    /// Open the new-run screen: rebuild the agent catalog, walk the workdir for
    /// `@` candidates, and start from an empty task.
    pub(super) fn open_new_run_screen(&mut self) {
        self.new_run_screen = true;
        self.new_run_focus = NewRunPane::Agents;
        self.new_run_filter.clear();
        self.new_run_selected = 0;
        self.new_run_task = ratatui_textarea::TextArea::default();
        self.close_file_ref();
        self.refresh_new_run_agents();
        self.new_run_files = collect_workdir_files(&self.new_run_ctx.workdir, FILE_CANDIDATE_CAP);
    }

    /// Close the screen, discarding whatever was typed.
    pub(super) fn close_new_run_screen(&mut self) {
        self.new_run_screen = false;
        self.close_file_ref();
    }

    /// Rebuild the agent list from the same catalog `lev list` reports, so the
    /// picker offers exactly the agents `lev run` can resolve - plus the
    /// bundled blueprints, which say so when they are not installed yet.
    pub(super) fn refresh_new_run_agents(&mut self) {
        let ctx = &self.new_run_ctx;
        let config = Config::load_from_path_public(&ctx.config_path).unwrap_or_default();
        let report = build_list_report(&ctx.agents_dir, &ctx.workdir, &config, ListFilter::All);
        let mut agents: Vec<NewRunAgent> = report
            .agents
            .iter()
            .map(|a| NewRunAgent {
                name: a.info.name.clone(),
                source: a.source.to_string(),
                description: a.info.description.clone(),
                path: a.path.clone(),
            })
            .collect();
        // A bundled blueprint already installed under the same name is the same
        // agent twice; the installed copy is the one that resolves, so it wins.
        for entry in &report.bundled {
            if !agents.iter().any(|a| a.name == entry.name) {
                agents.push(NewRunAgent {
                    name: entry.name.clone(),
                    source: "bundled".to_string(),
                    description: format!("v{} - install with `lev setup`", entry.version),
                    path: entry.name.clone(),
                });
            }
        }
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        self.new_run_agents = agents;
        self.clamp_new_run_selection();
    }

    /// Indices into `new_run_agents` that match the filter, in display order.
    pub(super) fn filtered_new_run_agents(&self) -> Vec<usize> {
        let query = self.new_run_filter.to_lowercase();
        self.new_run_agents
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                query.is_empty()
                    || a.name.to_lowercase().contains(&query)
                    || a.description.to_lowercase().contains(&query)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// The agent the picker is pointing at, if the filtered list is not empty.
    pub(super) fn new_run_selected_agent(&self) -> Option<&NewRunAgent> {
        let visible = self.filtered_new_run_agents();
        visible
            .get(self.new_run_selected)
            .and_then(|i| self.new_run_agents.get(*i))
    }

    /// Keep the selection inside the filtered list after a filter edit.
    fn clamp_new_run_selection(&mut self) {
        let len = self.filtered_new_run_agents().len();
        if self.new_run_selected >= len {
            self.new_run_selected = len.saturating_sub(1);
        }
    }

    /// Dispatch the run to the background lane and close the screen, so the new
    /// row shows up in the list the user is returned to.
    pub(super) fn submit_new_run(&mut self) {
        let Some(agent) = self.new_run_selected_agent().cloned() else {
            self.toast("Pick an agent first", ToastLevel::Error);
            return;
        };
        let task = self.new_run_task.lines().join("\n").trim().to_string();
        // An agent driven entirely by `--<region>` flags takes no task, and
        // there is no way to give it one here; that is a `lev run` command line,
        // and the daemon says so if this screen is pointed at such an agent.
        if task.is_empty() {
            self.toast("Write a task first", ToastLevel::Error);
            return;
        }
        let _ = self.spawn_cmd_tx.send(SpawnCommand {
            agent_path: agent.path.clone(),
            task,
            workdir: self.new_run_ctx.workdir.display().to_string(),
        });
        self.toast(format!("Starting '{}'…", agent.name), ToastLevel::Info);
        self.add_log(format!("run requested: {}", agent.name));
        self.close_new_run_screen();
    }

    /// Drain finished spawns into toasts, mirroring [`Self::drain_mcp_outcomes`].
    pub(super) fn drain_spawn_outcomes(&mut self) {
        while let Ok(outcome) = self.spawn_outcome_rx.try_recv() {
            let level = match outcome.ok {
                true => ToastLevel::Info,
                false => ToastLevel::Error,
            };
            self.add_log(outcome.message.clone());
            self.toast(outcome.message, level);
        }
    }

    // ── `@` file references ──────────────────────────────────────────────────

    /// The workdir paths matching what has been typed after the `@`, capped to
    /// what the popup shows.
    pub(super) fn file_ref_matches(&self) -> Vec<&str> {
        if !self.new_run_file_ref {
            return Vec::new();
        }
        let query = self.new_run_file_query.to_lowercase();
        self.new_run_files
            .iter()
            .filter(|p| p.to_lowercase().contains(&query))
            .take(FILE_REF_VISIBLE)
            .map(String::as_str)
            .collect()
    }

    /// Replace the typed query with the highlighted path and close the popup.
    /// With nothing matching there is nothing to insert, so the text stands as
    /// typed.
    fn accept_file_ref(&mut self) {
        let chosen = self
            .file_ref_matches()
            .get(self.new_run_file_selected)
            .map(|p| p.to_string());
        if let Some(path) = chosen {
            for _ in 0..self.new_run_file_query.chars().count() {
                self.new_run_task.delete_char();
            }
            self.new_run_task.insert_str(&path);
        }
        self.close_file_ref();
    }

    /// Dismiss the completion, leaving the task text exactly as typed.
    fn close_file_ref(&mut self) {
        self.new_run_file_ref = false;
        self.new_run_file_query.clear();
        self.new_run_file_selected = 0;
    }

    // ── Keys ─────────────────────────────────────────────────────────────────

    /// Keys while the new-run screen is open. The `@` popup takes them first -
    /// it is a menu over the text being typed, not a mode beside it - then the
    /// focused pane.
    pub(super) fn handle_new_run_key(&mut self, key: crossterm::event::KeyEvent) {
        if self.new_run_file_ref {
            self.handle_file_ref_key(key);
            return;
        }
        match self.new_run_focus {
            NewRunPane::Agents => self.handle_new_run_agents_key(key.code),
            NewRunPane::Task => self.handle_new_run_task_key(key),
        }
    }

    /// Agent picker keys. Every printable character filters, so there is no
    /// separate search mode to enter - and no letter is free to mean something
    /// else here.
    fn handle_new_run_agents_key(&mut self, key_code: crossterm::event::KeyCode) {
        use crossterm::event::KeyCode;
        match key_code {
            // Esc clears the filter first, then closes - the same two-step the
            // main list's filter uses.
            KeyCode::Esc => match self.new_run_filter.is_empty() {
                true => self.close_new_run_screen(),
                false => {
                    self.new_run_filter.clear();
                    self.new_run_selected = 0;
                }
            },
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Enter => {
                self.new_run_focus = NewRunPane::Task;
            }
            KeyCode::Up => {
                self.new_run_selected = self.new_run_selected.saturating_sub(1);
            }
            KeyCode::Down => {
                if self.new_run_selected + 1 < self.filtered_new_run_agents().len() {
                    self.new_run_selected += 1;
                }
            }
            KeyCode::Backspace => {
                self.new_run_filter.pop();
                self.new_run_selected = 0;
            }
            KeyCode::Char(c) => {
                self.new_run_filter.push(c);
                self.new_run_selected = 0;
            }
            _ => {}
        }
    }

    /// Task editor keys. Enter starts the run and Alt+Enter breaks the line,
    /// matching the response pane so the two editors do not disagree.
    fn handle_new_run_task_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Esc => self.new_run_focus = NewRunPane::Agents,
            KeyCode::Tab | KeyCode::BackTab => self.new_run_focus = NewRunPane::Agents,
            KeyCode::Enter if key.modifiers.is_empty() => self.submit_new_run(),
            KeyCode::Char('@') => {
                self.new_run_task.insert_char('@');
                self.new_run_file_ref = true;
            }
            _ => {
                self.new_run_task.input(ratatui_textarea::Input::from(key));
            }
        }
    }

    /// Keys while an `@` completion is open.
    fn handle_file_ref_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        let matches = self.file_ref_matches().len();
        match key.code {
            KeyCode::Esc => self.close_file_ref(),
            KeyCode::Enter | KeyCode::Tab => self.accept_file_ref(),
            KeyCode::Up => {
                self.new_run_file_selected = self.new_run_file_selected.saturating_sub(1);
            }
            KeyCode::Down => {
                if self.new_run_file_selected + 1 < matches {
                    self.new_run_file_selected += 1;
                }
            }
            KeyCode::Backspace => {
                self.new_run_task.delete_char();
                // Backspacing over the `@` itself ends the reference; there is
                // nothing left to complete.
                match self.new_run_file_query.pop() {
                    // The match set changed, so the highlight goes back to the top.
                    Some(_) => self.new_run_file_selected = 0,
                    None => self.close_file_ref(),
                }
            }
            KeyCode::Char(c) => {
                self.new_run_task.insert_char(c);
                self.new_run_file_query.push(c);
                self.new_run_file_selected = 0;
            }
            _ => {}
        }
    }
}

/// Workdir-relative paths of the files under `root`, sorted, at most `cap`.
///
/// There is no `.gitignore` reader in the dependency tree and adding one to
/// populate a completion menu is not worth it, so the walk skips what an
/// ignore file would almost always cover anyway: dot-entries (`.git`,
/// `.venv`, editor state) and `target`. An unreadable directory is skipped
/// rather than reported - this list is a convenience, and half of it beats an
/// error message.
fn collect_workdir_files(root: &Path, cap: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if out.len() >= cap {
                out.sort();
                return out;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "target" {
                continue;
            }
            let relative = match prefix.is_empty() {
                true => name,
                false => format!("{prefix}/{name}"),
            };
            match entry.path().is_dir() {
                true => stack.push((entry.path(), relative)),
                false => out.push(relative),
            }
        }
    }
    out.sort();
    out
}

/// Resolve and spawn each requested run off the UI loop, reporting every
/// result back so a refused spawn is surfaced rather than swallowed.
pub(super) async fn spawn_background_loop(
    control: ControlClient,
    mut cmd_rx: mpsc::UnboundedReceiver<SpawnCommand>,
    outcome_tx: mpsc::UnboundedSender<SpawnOutcome>,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        if outcome_tx.send(run_spawn(&control, cmd).await).is_err() {
            return; // dashboard dropped the receiver
        }
    }
}

/// One spawn: resolve the blueprint locally, then hand it to the daemon.
async fn run_spawn(control: &ControlClient, cmd: SpawnCommand) -> SpawnOutcome {
    let args = match resolve_spawn_args(LaunchRequest {
        path: &cmd.agent_path,
        // Always `Some`, never `None`: `None` opens `$EDITOR`, which would
        // start a second full-screen program inside this one.
        task: Some(&cmd.task),
        stdin_is_terminal: &never_interactive,
        model: None,
        workdir: &cmd.workdir,
        yolo: false,
        allow: Vec::new(),
        max_depth: None,
        // Region seeds are a `lev run` command line; this screen writes a task.
        regions: HashMap::new(),
        no_seed_commands: false,
        output_request: None,
    }) {
        Ok(args) => args,
        Err(e) => {
            return SpawnOutcome {
                message: format!("Could not start '{}': {e}", cmd.agent_path),
                ok: false,
            };
        }
    };
    match control.spawn(args).await {
        Ok(ControlResponse::Spawned { run_id }) => SpawnOutcome {
            message: format!("Started {run_id}"),
            ok: true,
        },
        Ok(ControlResponse::Error { message }) => SpawnOutcome {
            message: format!("The daemon refused the run: {message}"),
            ok: false,
        },
        Ok(other) => SpawnOutcome {
            message: format!("Unexpected daemon response to spawn: {other:?}"),
            ok: false,
        },
        Err(e) => SpawnOutcome {
            message: format!("Could not reach the daemon: {e}"),
            ok: false,
        },
    }
}

/// The production new-run context: the real agents directory, config, and
/// working directory `lev dash` was started in.
pub(super) fn production_new_run_context() -> NewRunContext {
    NewRunContext {
        agents_dir: leviath_core::paths::agents_dir().unwrap_or_default(),
        config_path: Config::config_path(),
        workdir: crate::commands::resolve_cwd().unwrap_or_default(),
    }
}

#[cfg(test)]
impl Dashboard {
    /// The retained spawn-command receiver, for asserting dispatches.
    pub(super) fn spawn_cmd_rx_for_test(&mut self) -> &mut mpsc::UnboundedReceiver<SpawnCommand> {
        &mut self
            .spawn_bg_ends
            .as_mut()
            .expect("background ends retained in tests")
            .0
    }

    /// Inject an outcome as if the background loop had produced it.
    pub(super) fn inject_spawn_outcome_for_test(&self, outcome: SpawnOutcome) {
        self.spawn_bg_ends
            .as_ref()
            .expect("background ends retained in tests")
            .1
            .send(outcome)
            .expect("outcome receiver is alive");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::make_test_dashboard;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// A manifest the real parser accepts, for the agent picker's catalog.
    fn write_agent(dir: &Path, name: &str, description: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("agent.leviath"),
            format!(
                "[agent]\nname = \"{name}\"\nversion = \"0.1.0\"\n\
                 description = \"{description}\"\n\n\
                 [stages.main]\nmode = \"autonomous\"\n\n\
                 [stages.main.model]\n\
                 provider = \"anthropic\"\nmodel = \"claude-sonnet-5\"\n\n\
                 [context.regions]\n\
                 task = {{ kind = \"pinned\", max_tokens = 1000, seed = \"task\" }}\n\
                 conversation = {{ kind = \"sliding_window\", max_items = 20, \
                 max_tokens = 10000 }}\n"
            ),
        )
        .unwrap();
    }

    /// A dashboard whose new-run screen reads from `dir` only.
    fn dash_at(dir: &Path) -> Dashboard {
        let mut dash = make_test_dashboard();
        dash.new_run_ctx = NewRunContext {
            agents_dir: dir.join("agents"),
            config_path: dir.join("config.toml"),
            workdir: dir.join("work"),
        };
        std::fs::create_dir_all(dir.join("work")).unwrap();
        dash
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // ─── catalog ──────────────────────────────────────────────────────────

    #[test]
    fn opening_the_screen_loads_agents_and_files() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents/writer"), "writer", "writes things");
        let mut dash = dash_at(dir.path());
        std::fs::write(dir.path().join("work/notes.md"), b"x").unwrap();

        dash.open_new_run_screen();
        assert!(dash.new_run_screen);
        assert_eq!(dash.new_run_focus, NewRunPane::Agents);
        let names: Vec<&str> = dash
            .new_run_agents
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert!(
            names.contains(&"writer"),
            "installed agent listed: {names:?}"
        );
        assert_eq!(dash.new_run_files, vec!["notes.md".to_string()]);
    }

    #[test]
    fn the_catalog_includes_bundled_blueprints_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        dash.refresh_new_run_agents();
        let bundled: Vec<&NewRunAgent> = dash
            .new_run_agents
            .iter()
            .filter(|a| a.source == "bundled")
            .collect();
        assert!(!bundled.is_empty(), "the binary ships blueprints");

        // Installing one of them under the same name replaces the bundled row
        // rather than adding a second one.
        let name = bundled[0].name.clone();
        write_agent(&dir.path().join("agents").join(&name), &name, "installed");
        dash.refresh_new_run_agents();
        let same: Vec<&NewRunAgent> = dash
            .new_run_agents
            .iter()
            .filter(|a| a.name == name)
            .collect();
        assert_eq!(same.len(), 1, "one row per name: {same:?}");
        assert_eq!(same[0].source, "installed");
    }

    #[test]
    fn a_local_agent_in_the_workdir_is_offered() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("work"), "here", "the cwd agent");
        let mut dash = dash_at(dir.path());
        dash.refresh_new_run_agents();
        let local = dash
            .new_run_agents
            .iter()
            .find(|a| a.name == "here")
            .expect("the cwd agent is listed");
        assert_eq!(local.source, "local");
    }

    #[test]
    fn filtering_narrows_the_list_and_clamps_the_selection() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents/alpha"), "alpha", "first");
        write_agent(&dir.path().join("agents/beta"), "beta", "second");
        let mut dash = dash_at(dir.path());
        dash.refresh_new_run_agents();

        let all = dash.filtered_new_run_agents().len();
        dash.new_run_selected = all - 1;
        dash.new_run_filter = "alpha".to_string();
        dash.clamp_new_run_selection();
        assert_eq!(dash.filtered_new_run_agents().len(), 1);
        assert_eq!(dash.new_run_selected, 0);
        assert_eq!(dash.new_run_selected_agent().unwrap().name, "alpha");

        // The description matches too, not just the name.
        dash.new_run_filter = "second".to_string();
        assert_eq!(dash.new_run_selected_agent().unwrap().name, "beta");
    }

    #[test]
    fn a_filter_matching_nothing_selects_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        dash.refresh_new_run_agents();
        dash.new_run_filter = "no-such-agent-anywhere".to_string();
        dash.clamp_new_run_selection();
        assert!(dash.filtered_new_run_agents().is_empty());
        assert!(dash.new_run_selected_agent().is_none());
    }

    #[test]
    fn an_unreadable_config_still_lists_the_bundled_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        // A directory where the config file should be: load fails, defaults win.
        std::fs::create_dir(dir.path().join("config.toml")).unwrap();
        dash.refresh_new_run_agents();
        assert!(dash.new_run_agents.iter().any(|a| a.source == "bundled"));
    }

    #[test]
    fn a_configured_agent_path_is_scanned() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("extra/scout"), "scout", "from the config");
        let mut dash = dash_at(dir.path());
        let mut config = Config::default();
        config.agent_paths.push(dir.path().join("extra"));
        config
            .save_to_path_public(&dash.new_run_ctx.config_path)
            .unwrap();
        dash.refresh_new_run_agents();
        let scout = dash
            .new_run_agents
            .iter()
            .find(|a| a.name == "scout")
            .expect("the configured agent is listed");
        assert_eq!(scout.source, "configured");
    }

    // ─── file walking ─────────────────────────────────────────────────────

    #[test]
    fn the_walk_recurses_and_skips_dots_and_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("README.md"), b"x").unwrap();
        std::fs::write(root.join("src/main.rs"), b"x").unwrap();
        std::fs::write(root.join("src/deep/mod.rs"), b"x").unwrap();
        std::fs::write(root.join(".env"), b"x").unwrap();
        std::fs::write(root.join(".git/HEAD"), b"x").unwrap();
        std::fs::write(root.join("target/binary"), b"x").unwrap();

        let files = collect_workdir_files(root, 100);
        assert_eq!(
            files,
            vec![
                "README.md".to_string(),
                "src/deep/mod.rs".to_string(),
                "src/main.rs".to_string(),
            ]
        );
    }

    #[test]
    fn the_walk_stops_at_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("f{i}")), b"x").unwrap();
        }
        assert_eq!(collect_workdir_files(dir.path(), 4).len(), 4);
    }

    #[test]
    fn an_unreadable_root_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(collect_workdir_files(&dir.path().join("nope"), 10).is_empty());
    }

    // ─── keys: agent picker ───────────────────────────────────────────────

    #[test]
    fn typing_filters_and_backspace_undoes_it() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents/alpha"), "alpha", "first");
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();

        dash.handle_new_run_key(key(KeyCode::Char('a')));
        dash.handle_new_run_key(key(KeyCode::Char('l')));
        assert_eq!(dash.new_run_filter, "al");
        dash.handle_new_run_key(key(KeyCode::Backspace));
        assert_eq!(dash.new_run_filter, "a");
    }

    #[test]
    fn escape_clears_the_filter_then_closes() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.new_run_filter = "x".to_string();

        dash.handle_new_run_key(key(KeyCode::Esc));
        assert!(dash.new_run_filter.is_empty());
        assert!(dash.new_run_screen, "the first Esc only clears the filter");

        dash.handle_new_run_key(key(KeyCode::Esc));
        assert!(!dash.new_run_screen);
    }

    #[test]
    fn arrows_move_within_the_filtered_list_only() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents/alpha"), "alpha", "first");
        write_agent(&dir.path().join("agents/beta"), "beta", "second");
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        let count = dash.filtered_new_run_agents().len();

        dash.handle_new_run_key(key(KeyCode::Down));
        assert_eq!(dash.new_run_selected, 1);
        dash.handle_new_run_key(key(KeyCode::Up));
        assert_eq!(dash.new_run_selected, 0);
        dash.handle_new_run_key(key(KeyCode::Up));
        assert_eq!(dash.new_run_selected, 0, "up at the top stays put");

        for _ in 0..count + 3 {
            dash.handle_new_run_key(key(KeyCode::Down));
        }
        assert_eq!(dash.new_run_selected, count - 1, "down stops at the end");
    }

    #[test]
    fn tab_enter_and_backtab_all_reach_the_task_editor() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        for code in [KeyCode::Tab, KeyCode::Enter, KeyCode::BackTab] {
            dash.open_new_run_screen();
            dash.handle_new_run_key(key(code));
            assert_eq!(dash.new_run_focus, NewRunPane::Task, "{code:?}");
        }
    }

    #[test]
    fn an_unbound_picker_key_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.handle_new_run_key(key(KeyCode::F(5)));
        assert!(dash.new_run_screen);
        assert!(dash.new_run_filter.is_empty());
    }

    // ─── keys: task editor ────────────────────────────────────────────────

    #[test]
    fn the_task_editor_takes_text_and_alt_enter_breaks_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.new_run_focus = NewRunPane::Task;

        dash.handle_new_run_key(key(KeyCode::Char('h')));
        dash.handle_new_run_key(key(KeyCode::Char('i')));
        dash.handle_new_run_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        dash.handle_new_run_key(key(KeyCode::Char('!')));
        assert_eq!(
            dash.new_run_task.lines(),
            vec!["hi".to_string(), "!".to_string()]
        );
    }

    #[test]
    fn escape_and_tab_hand_focus_back_to_the_picker() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        for code in [KeyCode::Esc, KeyCode::Tab, KeyCode::BackTab] {
            dash.open_new_run_screen();
            dash.new_run_focus = NewRunPane::Task;
            dash.handle_new_run_key(key(code));
            assert_eq!(dash.new_run_focus, NewRunPane::Agents, "{code:?}");
        }
    }

    // ─── keys: `@` completion ─────────────────────────────────────────────

    /// A dashboard sitting in the task editor with three workdir files.
    fn dash_ready_to_type(dir: &Path) -> Dashboard {
        std::fs::create_dir_all(dir.join("work/src")).unwrap();
        std::fs::write(dir.join("work/README.md"), b"x").unwrap();
        std::fs::write(dir.join("work/src/main.rs"), b"x").unwrap();
        std::fs::write(dir.join("work/src/lib.rs"), b"x").unwrap();
        let mut dash = dash_at(dir);
        dash.open_new_run_screen();
        dash.new_run_focus = NewRunPane::Task;
        dash
    }

    #[test]
    fn at_opens_the_popup_and_typing_filters_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_ready_to_type(dir.path());

        dash.handle_new_run_key(key(KeyCode::Char('@')));
        assert!(dash.new_run_file_ref);
        assert_eq!(dash.file_ref_matches().len(), 3, "everything, unfiltered");

        dash.handle_new_run_key(key(KeyCode::Char('l')));
        dash.handle_new_run_key(key(KeyCode::Char('i')));
        assert_eq!(dash.new_run_file_query, "li");
        assert_eq!(dash.file_ref_matches(), vec!["src/lib.rs"]);
        // The typed characters are in the task text too, so an unaccepted
        // reference still reads as what the user wrote.
        assert_eq!(dash.new_run_task.lines(), vec!["@li".to_string()]);
    }

    #[test]
    fn accepting_a_completion_replaces_the_query_with_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_ready_to_type(dir.path());
        dash.handle_new_run_key(key(KeyCode::Char('r')));
        dash.handle_new_run_key(key(KeyCode::Char('e')));
        dash.handle_new_run_key(key(KeyCode::Char('a')));
        dash.handle_new_run_key(key(KeyCode::Char('d')));
        dash.handle_new_run_key(key(KeyCode::Char(' ')));
        dash.handle_new_run_key(key(KeyCode::Char('@')));
        dash.handle_new_run_key(key(KeyCode::Char('m')));
        dash.handle_new_run_key(key(KeyCode::Char('a')));
        dash.handle_new_run_key(key(KeyCode::Enter));

        assert!(!dash.new_run_file_ref, "the popup closes");
        assert_eq!(dash.new_run_task.lines(), vec!["read @src/main.rs"]);
    }

    #[test]
    fn tab_accepts_the_highlighted_completion() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_ready_to_type(dir.path());
        dash.handle_new_run_key(key(KeyCode::Char('@')));
        dash.handle_new_run_key(key(KeyCode::Char('s')));
        dash.handle_new_run_key(key(KeyCode::Down));
        assert_eq!(dash.new_run_file_selected, 1);
        dash.handle_new_run_key(key(KeyCode::Tab));
        assert_eq!(dash.new_run_task.lines(), vec!["@src/main.rs"]);
    }

    #[test]
    fn the_popup_selection_stops_at_both_ends() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_ready_to_type(dir.path());
        dash.handle_new_run_key(key(KeyCode::Char('@')));

        dash.handle_new_run_key(key(KeyCode::Up));
        assert_eq!(dash.new_run_file_selected, 0);
        for _ in 0..10 {
            dash.handle_new_run_key(key(KeyCode::Down));
        }
        assert_eq!(dash.new_run_file_selected, 2);
    }

    #[test]
    fn accepting_with_nothing_matching_leaves_the_text_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_ready_to_type(dir.path());
        dash.handle_new_run_key(key(KeyCode::Char('@')));
        dash.handle_new_run_key(key(KeyCode::Char('z')));
        assert!(dash.file_ref_matches().is_empty());
        dash.handle_new_run_key(key(KeyCode::Enter));
        assert!(!dash.new_run_file_ref);
        assert_eq!(dash.new_run_task.lines(), vec!["@z"]);
    }

    #[test]
    fn escape_closes_the_popup_without_leaving_the_editor() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_ready_to_type(dir.path());
        dash.handle_new_run_key(key(KeyCode::Char('@')));
        dash.handle_new_run_key(key(KeyCode::Esc));
        assert!(!dash.new_run_file_ref);
        assert_eq!(dash.new_run_focus, NewRunPane::Task);
        assert!(dash.new_run_screen);
    }

    #[test]
    fn backspacing_over_the_at_ends_the_reference() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_ready_to_type(dir.path());
        dash.handle_new_run_key(key(KeyCode::Char('@')));
        dash.handle_new_run_key(key(KeyCode::Char('s')));
        dash.handle_new_run_key(key(KeyCode::Down));

        dash.handle_new_run_key(key(KeyCode::Backspace));
        assert_eq!(dash.new_run_file_query, "");
        assert_eq!(
            dash.new_run_file_selected, 0,
            "the match set changed, so the highlight resets"
        );

        dash.handle_new_run_key(key(KeyCode::Backspace));
        assert!(!dash.new_run_file_ref);
        assert_eq!(dash.new_run_task.lines(), vec![""]);
    }

    #[test]
    fn an_unbound_popup_key_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_ready_to_type(dir.path());
        dash.handle_new_run_key(key(KeyCode::Char('@')));
        dash.handle_new_run_key(key(KeyCode::F(5)));
        assert!(dash.new_run_file_ref);
    }

    #[test]
    fn matches_are_empty_with_no_popup_open() {
        let dash = make_test_dashboard();
        assert!(dash.file_ref_matches().is_empty());
    }

    // ─── submitting ───────────────────────────────────────────────────────

    #[test]
    fn submitting_dispatches_the_run_and_closes_the_screen() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents/alpha"), "alpha", "first");
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.new_run_filter = "alpha".to_string();
        dash.new_run_focus = NewRunPane::Task;
        dash.new_run_task.insert_str("  ship it  ");

        dash.handle_new_run_key(key(KeyCode::Enter));

        assert!(!dash.new_run_screen, "the screen closes on dispatch");
        let cmd = dash
            .spawn_cmd_rx_for_test()
            .try_recv()
            .expect("a spawn was dispatched");
        assert_eq!(cmd.task, "ship it", "the task is trimmed");
        assert!(cmd.agent_path.ends_with("alpha"), "got: {}", cmd.agent_path);
        assert_eq!(cmd.workdir, dir.path().join("work").display().to_string());
    }

    #[test]
    fn submitting_without_a_task_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents/alpha"), "alpha", "first");
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.new_run_focus = NewRunPane::Task;
        dash.new_run_task.insert_str("   ");

        dash.handle_new_run_key(key(KeyCode::Enter));
        assert!(dash.new_run_screen, "the screen stays open to fix it");
        assert!(dash.spawn_cmd_rx_for_test().try_recv().is_err());
        let toasts = dash.toast_messages_for_test();
        assert!(toasts.iter().any(|m| m.contains("task")), "got: {toasts:?}");
    }

    #[test]
    fn submitting_without_an_agent_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.new_run_filter = "no-such-agent-anywhere".to_string();
        dash.new_run_task.insert_str("do a thing");

        dash.submit_new_run();
        assert!(dash.spawn_cmd_rx_for_test().try_recv().is_err());
        let toasts = dash.toast_messages_for_test();
        assert!(
            toasts.iter().any(|m| m.contains("agent")),
            "got: {toasts:?}"
        );
    }

    #[test]
    fn spawn_outcomes_drain_into_toasts() {
        let mut dash = make_test_dashboard();
        dash.inject_spawn_outcome_for_test(SpawnOutcome {
            message: "Started run-1".to_string(),
            ok: true,
        });
        dash.inject_spawn_outcome_for_test(SpawnOutcome {
            message: "boom".to_string(),
            ok: false,
        });
        dash.drain_spawn_outcomes();
        assert_eq!(
            dash.toast_messages_for_test(),
            vec!["Started run-1".to_string(), "boom".to_string()]
        );
    }

    #[test]
    fn draining_with_nothing_pending_is_a_noop() {
        let mut dash = make_test_dashboard();
        dash.drain_spawn_outcomes();
        assert!(dash.toast_messages_for_test().is_empty());
    }

    // ─── the spawn lane ───────────────────────────────────────────────────

    /// A daemon that replies `reply` (verbatim, newline added) to one request.
    /// `None` closes the connection without replying.
    fn replying_daemon(
        dir: &Path,
        reply: Option<&'static str>,
    ) -> (ControlClient, tokio::task::JoinHandle<()>) {
        use leviath_runtime::control_socket::{bind_control_listener, control_id};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let id = control_id(dir);
        let mut listener = bind_control_listener(&id).unwrap();
        let handle = tokio::spawn(async move {
            let stream = listener
                .accept()
                .await
                .expect("accept succeeds")
                .expect("our own connection is admitted");
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _ = lines.next_line().await;
            if let Some(reply) = reply {
                let _ = write_half.write_all(format!("{reply}\n").as_bytes()).await;
            }
        });
        (ControlClient::new(id), handle)
    }

    /// Drive one spawn of a real manifest through the loop against a daemon
    /// giving `reply`, and return what the dashboard would toast.
    async fn spawn_outcome(reply: Option<&'static str>) -> SpawnOutcome {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("alpha"), "alpha", "first");
        let (control, server) = replying_daemon(dir.path(), reply);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        tokio::spawn(spawn_background_loop(control, cmd_rx, out_tx));
        cmd_tx
            .send(SpawnCommand {
                agent_path: dir.path().join("alpha").display().to_string(),
                task: "ship it".to_string(),
                workdir: dir.path().display().to_string(),
            })
            .unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), out_rx.recv())
            .await
            .expect("an outcome was reported")
            .expect("the loop is alive");
        let _ = server.await;
        outcome
    }

    #[tokio::test]
    async fn every_daemon_answer_is_reported() {
        let started = spawn_outcome(Some(r#"{"result":"spawned","run_id":"alpha-1"}"#)).await;
        assert!(started.ok);
        assert!(started.message.contains("alpha-1"), "{}", started.message);

        let refused =
            spawn_outcome(Some(r#"{"result":"error","message":"no such blueprint"}"#)).await;
        assert!(!refused.ok);
        assert!(
            refused.message.contains("no such blueprint"),
            "{}",
            refused.message
        );

        let odd = spawn_outcome(Some(r#"{"result":"ok","ok":true}"#)).await;
        assert!(!odd.ok);
        assert!(odd.message.contains("Unexpected"), "{}", odd.message);

        // Connection closed with no reply: a transport error, surfaced as one.
        let broken = spawn_outcome(None).await;
        assert!(!broken.ok);
        assert!(
            broken.message.contains("Could not reach the daemon"),
            "{}",
            broken.message
        );
    }

    #[tokio::test]
    async fn an_unresolvable_agent_never_reaches_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let control = ControlClient::new(leviath_runtime::control_socket::control_id(dir.path()));
        let outcome = run_spawn(
            &control,
            SpawnCommand {
                agent_path: dir.path().join("nope").display().to_string(),
                task: "ship it".to_string(),
                workdir: dir.path().display().to_string(),
            },
        )
        .await;
        assert!(!outcome.ok);
        assert!(
            outcome.message.contains("Could not start"),
            "{}",
            outcome.message
        );
    }

    #[tokio::test]
    async fn the_loop_stops_when_the_dashboard_drops_its_receiver() {
        let dir = tempfile::tempdir().unwrap();
        let control = ControlClient::new(leviath_runtime::control_socket::control_id(dir.path()));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        drop(out_rx);
        let handle = tokio::spawn(spawn_background_loop(control, cmd_rx, out_tx));
        cmd_tx
            .send(SpawnCommand {
                agent_path: dir.path().join("nope").display().to_string(),
                task: "t".to_string(),
                workdir: dir.path().display().to_string(),
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("the loop returns once nothing is listening")
            .unwrap();
    }

    #[test]
    fn the_production_context_points_at_the_real_paths() {
        let ctx = production_new_run_context();
        assert!(ctx.config_path.ends_with("config.toml"));
        assert!(ctx.agents_dir.ends_with("agents"));
    }
}
