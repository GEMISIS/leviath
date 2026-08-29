//! New-run screen: the agent catalog, the `@` file completion, and the async
//! spawn lane.
//!
//! `lev dash` could only ever watch runs somebody else had started. This is the
//! other half: pick an agent, write the task, press Ctrl+Enter (or the Start
//! button, on a terminal that cannot tell Ctrl+Enter from Enter). The screen is modal
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
    ConfirmAction, NewRunAgent, NewRunContext, NewRunPane, SpawnCommand, SpawnOutcome, ToastLevel,
};
use crate::commands::list::{ListFilter, build_list_report};
use crate::config::Config;
use crate::daemon::client::{LaunchRequest, never_interactive, resolve_spawn_args};
use crate::tui::widgets::confirm::Confirm;
use crate::tui::widgets::markdown_edit::MarkdownEdit;

/// How many ticks the dashboard waits for a just-started run to appear before
/// giving up on opening its page. At the 250ms tick the loop runs on, this is
/// about fifteen seconds.
pub(super) const OPEN_RUN_TICKS: u32 = 60;
use ratatui::text::Line;

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
        // Filled in below, once the catalog is built: the list has to exist
        // before a name can be found in it.
        self.new_run_task = MarkdownEdit::default().in_mode(self.md_mode());
        self.new_run_task
            .set_placeholder("What should this agent do? Markdown is fine here.");
        // Unattended is off every time the screen opens. It is a consequential
        // setting, and one that survived out of sight is one somebody can
        // leave on and forget.
        self.new_run_yolo = false;
        self.close_file_ref();
        self.refresh_new_run_agents();
        self.select_last_launched_agent();
        self.new_run_files = collect_workdir_files(&self.new_run_ctx.workdir, FILE_CANDIDATE_CAP);
    }

    /// Open on the agent last launched from here, when it is still offered.
    ///
    /// The cursor, not a filter: the whole catalog stays visible and one press
    /// of `↑` reaches everything above it. An agent that has since been removed
    /// or renamed simply is not found, and the list opens at the top as it
    /// always did.
    fn select_last_launched_agent(&mut self) {
        let Some(name) = self.last_launched_agent.as_deref() else {
            return;
        };
        if let Some(index) = self.new_run_agents.iter().position(|a| a.name == name) {
            self.new_run_selected = index;
        }
    }

    /// Close the screen, discarding whatever was typed.
    pub(super) fn close_new_run_screen(&mut self) {
        self.new_run_screen = false;
        self.new_run_preview = None;
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
            self.toast("Pick an agent blueprint first", ToastLevel::Error);
            return;
        };
        let task = self.new_run_task.text().trim().to_string();
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
            yolo: self.new_run_yolo,
        });
        let how = match self.new_run_yolo {
            true => " unattended",
            false => "",
        };
        self.toast(format!("Starting '{}'{how}…", agent.name), ToastLevel::Info);
        self.add_log(format!("run requested: {}", agent.name));
        // Recorded on the launch rather than on the selection: moving the
        // cursor down the list to read a blueprint's preview is not a choice
        // of agent, and starting a run is.
        self.last_launched_agent = Some(agent.name.clone());
        self.save_ui_state();
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
            // Starting a run is a request to watch it, so the dashboard goes
            // to its page. Not here though: the daemon has only just been told,
            // and the run reaches the list on a later sync.
            if let Some(run_id) = outcome.run_id {
                self.pending_open_run = Some((run_id, OPEN_RUN_TICKS));
            }
        }
    }

    /// Open the page of a run started from this screen, once it exists.
    ///
    /// Bounded rather than open-ended: a run that never appears is a run the
    /// daemon dropped, and a dashboard that jumps to a page minutes later,
    /// after the user has moved on, is worse than one that quietly gives up.
    /// The toast already said the run started.
    pub(super) fn open_pending_run(&mut self) {
        let Some((run_id, ticks)) = self.pending_open_run.take() else {
            return;
        };
        // Only from the list. Somewhere else means the user went there
        // themselves, and yanking them out of it is not what they asked for.
        if self.detail_view || self.mcp_screen || self.new_run_screen {
            return;
        }
        match self
            .display_indices
            .iter()
            .position(|i| self.agents.get(*i).is_some_and(|agent| agent.id == run_id))
        {
            Some(row) => {
                self.selected = row;
                self.table_state.select(Some(row));
                self.open_detail_view();
            }
            None => {
                if let Some(left) = ticks.checked_sub(1) {
                    self.pending_open_run = Some((run_id, left));
                }
            }
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
                self.new_run_task.area_mut().delete_char();
            }
            self.new_run_task.area_mut().insert_str(&path);
        }
        self.close_file_ref();
    }

    /// Dismiss the completion, leaving the task text exactly as typed.
    fn close_file_ref(&mut self) {
        self.new_run_file_ref = false;
        self.new_run_file_query.clear();
        self.new_run_file_selected = 0;
    }

    /// Ctrl-Y: turn unattended runs on or off for this screen.
    ///
    /// A chord rather than a letter because both panes here consume printable
    /// characters - one filters the agent list, the other is the task itself -
    /// so there is no letter left that could mean anything but text.
    ///
    /// Turning it *on* asks first, once per sitting. Turning it off never asks:
    /// nothing needs confirming about deciding to be asked more.
    pub(super) fn toggle_new_run_yolo(&mut self) {
        if self.new_run_yolo {
            self.new_run_yolo = false;
            self.toast("Unattended off: you will be asked", ToastLevel::Info);
            return;
        }
        if self.yolo_warning_silenced {
            self.new_run_yolo = true;
            self.toast("Unattended on", ToastLevel::Warning);
            return;
        }
        self.pending_confirm = Some((ConfirmAction::EnableYolo, yolo_warning()));
    }

    /// Apply the answer to that warning.
    pub(super) fn accept_yolo_warning(&mut self, silence: bool) {
        self.new_run_yolo = true;
        self.yolo_warning_silenced = silence;
        self.toast("Unattended on", ToastLevel::Warning);
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
        // The task box's own popup outranks the screen's keys: while it is up,
        // Enter means "insert the link", not "start the run".
        if self.new_run_task.is_modal() {
            self.new_run_focus = NewRunPane::Task;
            let outcome = self.new_run_task.handle_key(&key);
            self.remember_md_mode(outcome);
            return;
        }
        // Ahead of both panes: these belong to the screen, not to whichever
        // half of it currently has the cursor. F1 rather than `?`, which is a
        // question mark in both a filter box and a task.
        if key.code == crossterm::event::KeyCode::F(1) {
            self.show_help = true;
            return;
        }
        if key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
            && key.code == crossterm::event::KeyCode::Char('y')
        {
            self.toggle_new_run_yolo();
            return;
        }
        match self.new_run_focus {
            NewRunPane::Agents => self.handle_new_run_agents_key(key.code),
            NewRunPane::Task => self.handle_new_run_task_key(key),
            NewRunPane::Start => self.handle_new_run_start_key(key.code),
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
            KeyCode::Tab | KeyCode::Enter => self.new_run_focus = NewRunPane::Task,
            KeyCode::BackTab => self.new_run_focus = NewRunPane::Start,
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

    /// Task editor keys. Enter breaks the line, the way it does in any other
    /// text box, and Ctrl+Enter starts the run.
    ///
    /// Ctrl+Enter reaches the program only under the kitty keyboard protocol;
    /// a terminal without it sends Ctrl+Enter as a plain Enter, which is a
    /// newline here. That is what the Start button under the editor is for.
    /// The response pane's editor keeps Enter as send: a reply is one line
    /// far more often than a task is.
    fn handle_new_run_task_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        match key.code {
            KeyCode::Esc => self.new_run_focus = NewRunPane::Agents,
            KeyCode::Tab => self.new_run_focus = NewRunPane::Start,
            KeyCode::BackTab => self.new_run_focus = NewRunPane::Agents,
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.submit_new_run();
            }
            KeyCode::Char('@') => {
                self.new_run_task.area_mut().insert_char('@');
                self.new_run_file_ref = true;
            }
            _ => {
                let outcome = self.new_run_task.handle_key(&key);
                self.remember_md_mode(outcome);
            }
        }
    }

    /// Keys while the Start button holds focus: Enter or Space presses it.
    fn handle_new_run_start_key(&mut self, code: crossterm::event::KeyCode) {
        use crossterm::event::KeyCode;
        match code {
            KeyCode::Enter | KeyCode::Char(' ') => self.submit_new_run(),
            KeyCode::Tab | KeyCode::Esc => self.new_run_focus = NewRunPane::Agents,
            KeyCode::BackTab => self.new_run_focus = NewRunPane::Task,
            _ => {}
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
                self.new_run_task.area_mut().delete_char();
                // Backspacing over the `@` itself ends the reference; there is
                // nothing left to complete.
                match self.new_run_file_query.pop() {
                    // The match set changed, so the highlight goes back to the top.
                    Some(_) => self.new_run_file_selected = 0,
                    None => self.close_file_ref(),
                }
            }
            KeyCode::Char(c) => {
                self.new_run_task.area_mut().insert_char(c);
                self.new_run_file_query.push(c);
                self.new_run_file_selected = 0;
            }
            _ => {}
        }
    }
}

/// The warning shown before the first unattended run of a sitting.
///
/// It says what `--yolo` does *and* what it does not: a run that still stops
/// for a person reads as a hang to somebody who was told it would not stop, and
/// that misunderstanding is worse than the setting itself.
fn yolo_warning() -> Confirm {
    Confirm::new(
        "Run unattended?",
        vec![
            Line::from(
                "Agent runs started from this screen will approve their own tool calls: \
                 editing files, running shell commands, fetching URLs, whatever \
                 the agent's permissions allow, without stopping to ask you.",
            ),
            Line::from(""),
            Line::from(
                "It does not skip checkpoints a blueprint asks a person for. \
                 Those still stop, and one nobody answers ends the run when the \
                 interaction timeout expires.",
            ),
            Line::from(""),
            Line::from("Ctrl-Y turns it off again."),
        ],
        "Run unattended",
        "Keep asking me",
    )
    .danger()
    .with_remember("Don't ask again while this dashboard is open")
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
        yolo: cmd.yolo,
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
                run_id: None,
            };
        }
    };
    match control.spawn(args).await {
        Ok(ControlResponse::Spawned { run_id }) => SpawnOutcome {
            message: format!("Started {run_id}"),
            ok: true,
            run_id: Some(run_id),
        },
        Ok(ControlResponse::Error { message }) => SpawnOutcome {
            message: format!("The daemon refused the run: {message}"),
            ok: false,
            run_id: None,
        },
        Ok(other) => SpawnOutcome {
            message: format!("Unexpected daemon response to spawn: {other:?}"),
            ok: false,
            run_id: None,
        },
        Err(e) => SpawnOutcome {
            message: format!("Could not reach the daemon: {e}"),
            ok: false,
            run_id: None,
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

    // ─── the agent the screen opens on ────────────────────────────────────

    /// Someone who launches the same agent all day should find the cursor on
    /// it, not scroll past everything alphabetically ahead of it.
    #[test]
    fn the_screen_opens_on_the_agent_last_launched() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("alpha"), "alpha", "first");
        write_agent(&dir.path().join("agents").join("zulu"), "zulu", "last");
        let mut dash = dash_at(dir.path());

        dash.open_new_run_screen();
        assert_eq!(
            dash.new_run_selected_agent().map(|a| a.name.as_str()),
            Some("alpha"),
            "with no memory the list opens at the top"
        );

        dash.last_launched_agent = Some("zulu".to_string());
        dash.open_new_run_screen();
        assert_eq!(
            dash.new_run_selected_agent().map(|a| a.name.as_str()),
            Some("zulu")
        );

        // An agent that has since been removed is simply not found, and the
        // list opens where it always did rather than on nothing.
        dash.last_launched_agent = Some("deleted-since".to_string());
        dash.open_new_run_screen();
        assert_eq!(
            dash.new_run_selected_agent().map(|a| a.name.as_str()),
            Some("alpha")
        );
    }

    /// Recorded on the launch, not on the browse: moving the cursor to read a
    /// blueprint's preview is not choosing it.
    #[test]
    fn the_last_agent_is_recorded_when_a_run_actually_starts() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("alpha"), "alpha", "first");
        write_agent(&dir.path().join("agents").join("zulu"), "zulu", "last");
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();

        // By position, found rather than assumed: the catalog carries the
        // bundled blueprints too, so the index of a written one is not 1.
        dash.new_run_selected = dash
            .new_run_agents
            .iter()
            .position(|a| a.name == "zulu")
            .expect("the written agent is in the catalog");
        assert_eq!(dash.last_launched_agent, None, "browsing decides nothing");

        dash.new_run_task.area_mut().insert_str("do the thing");
        dash.submit_new_run();
        assert_eq!(dash.last_launched_agent.as_deref(), Some("zulu"));
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
    fn tab_and_enter_reach_the_task_editor() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        for code in [KeyCode::Tab, KeyCode::Enter] {
            dash.open_new_run_screen();
            dash.handle_new_run_key(key(code));
            assert_eq!(dash.new_run_focus, NewRunPane::Task, "{code:?}");
        }
    }

    /// Tab walks agents, task, Start and round again; Shift+Tab walks the
    /// same ring the other way.
    #[test]
    fn tab_cycles_agents_task_start_and_backtab_reverses() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        assert_eq!(dash.new_run_focus, NewRunPane::Agents);
        for expected in [
            NewRunPane::Task,
            NewRunPane::Start,
            NewRunPane::Agents,
            NewRunPane::Task,
        ] {
            dash.handle_new_run_key(key(KeyCode::Tab));
            assert_eq!(dash.new_run_focus, expected);
        }
        for expected in [
            NewRunPane::Agents,
            NewRunPane::Start,
            NewRunPane::Task,
            NewRunPane::Agents,
        ] {
            dash.handle_new_run_key(key(KeyCode::BackTab));
            assert_eq!(dash.new_run_focus, expected);
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

    /// Enter and Alt+Enter both break the line: with an agent picked and text
    /// in the box, a plain Enter still leaves the screen open with nothing
    /// dispatched.
    #[test]
    fn the_task_editor_takes_text_and_enter_breaks_the_line() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents/alpha"), "alpha", "first");
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.new_run_filter = "alpha".to_string();
        dash.new_run_focus = NewRunPane::Task;

        dash.handle_new_run_key(key(KeyCode::Char('h')));
        dash.handle_new_run_key(key(KeyCode::Char('i')));
        dash.handle_new_run_key(key(KeyCode::Enter));
        dash.handle_new_run_key(key(KeyCode::Char('!')));
        dash.handle_new_run_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        dash.handle_new_run_key(key(KeyCode::Char('?')));
        assert_eq!(
            dash.new_run_task.lines(),
            vec!["hi".to_string(), "!".to_string(), "?".to_string()]
        );
        assert!(dash.new_run_screen, "Enter did not start the run");
        assert!(dash.spawn_cmd_rx_for_test().try_recv().is_err());
    }

    #[test]
    fn escape_and_backtab_hand_focus_back_to_the_picker_and_tab_reaches_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        for code in [KeyCode::Esc, KeyCode::BackTab] {
            dash.open_new_run_screen();
            dash.new_run_focus = NewRunPane::Task;
            dash.handle_new_run_key(key(code));
            assert_eq!(dash.new_run_focus, NewRunPane::Agents, "{code:?}");
        }
        dash.new_run_focus = NewRunPane::Task;
        dash.handle_new_run_key(key(KeyCode::Tab));
        assert_eq!(dash.new_run_focus, NewRunPane::Start);
    }

    // ─── keys: Start button ───────────────────────────────────────────────

    /// A screen with an agent picked and a task written, focus on the button.
    fn dash_on_the_start_button(dir: &Path) -> Dashboard {
        write_agent(&dir.join("agents/alpha"), "alpha", "first");
        let mut dash = dash_at(dir);
        dash.open_new_run_screen();
        dash.new_run_filter = "alpha".to_string();
        dash.new_run_task.area_mut().insert_str("ship it");
        dash.new_run_focus = NewRunPane::Start;
        dash
    }

    #[test]
    fn enter_and_space_on_the_start_button_start_the_run() {
        for code in [KeyCode::Enter, KeyCode::Char(' ')] {
            let dir = tempfile::tempdir().unwrap();
            let mut dash = dash_on_the_start_button(dir.path());
            dash.handle_new_run_key(key(code));
            assert!(!dash.new_run_screen, "{code:?} pressed the button");
            let cmd = dash
                .spawn_cmd_rx_for_test()
                .try_recv()
                .expect("a spawn was dispatched");
            assert_eq!(cmd.task, "ship it");
        }
    }

    #[test]
    fn other_keys_on_the_start_button_move_focus_or_do_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_on_the_start_button(dir.path());
        for (code, expected) in [
            (KeyCode::Esc, NewRunPane::Agents),
            (KeyCode::Tab, NewRunPane::Agents),
            (KeyCode::BackTab, NewRunPane::Task),
        ] {
            dash.new_run_focus = NewRunPane::Start;
            dash.handle_new_run_key(key(code));
            assert_eq!(dash.new_run_focus, expected, "{code:?}");
        }
        dash.new_run_focus = NewRunPane::Start;
        dash.handle_new_run_key(key(KeyCode::Char('x')));
        assert_eq!(dash.new_run_focus, NewRunPane::Start, "a letter is ignored");
        assert_eq!(
            dash.new_run_task.text(),
            "ship it",
            "and not typed anywhere"
        );
        assert!(dash.new_run_screen);
        assert!(dash.spawn_cmd_rx_for_test().try_recv().is_err());
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

    // ─── formatting ───────────────────────────────────────────────────────

    /// The task box is the shared long-form editor, so its formatting chords
    /// have to survive the screen's own key routing (which takes Enter, Esc,
    /// Tab, `@` and Ctrl-Y before anything else sees them).
    #[test]
    fn formatting_chords_reach_the_task_box() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.new_run_focus = NewRunPane::Task;

        dash.handle_new_run_key(ctrl(KeyCode::Char('b')));
        for c in "ship it".chars() {
            dash.handle_new_run_key(key(KeyCode::Char(c)));
        }
        assert_eq!(dash.new_run_task.text(), "**ship it**");

        // Ctrl-Y still toggles unattended rather than pasting: the screen's
        // own chords are matched first, and formatting does not claim `y`.
        assert!(!dash.new_run_yolo);
        dash.handle_new_run_key(ctrl(KeyCode::Char('y')));
        assert_eq!(dash.new_run_task.text(), "**ship it**");
    }

    /// While the task box has a popup up, the screen's own keys are the
    /// popup's: Enter finishes the link rather than starting the run.
    #[test]
    fn the_task_boxs_popup_outranks_the_screens_keys() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents/alpha"), "alpha", "first");
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.new_run_filter = "alpha".to_string();
        dash.new_run_focus = NewRunPane::Task;

        dash.handle_new_run_key(ctrl(KeyCode::Char('k')));
        assert!(dash.new_run_task.is_modal());

        for c in "docs".chars() {
            dash.handle_new_run_key(key(KeyCode::Char(c)));
        }
        dash.handle_new_run_key(key(KeyCode::Enter));
        assert!(dash.new_run_screen, "Enter did not start the run");
        for c in "u".chars() {
            dash.handle_new_run_key(key(KeyCode::Char(c)));
        }
        dash.handle_new_run_key(key(KeyCode::Enter));
        assert!(!dash.new_run_task.is_modal());
        assert_eq!(dash.new_run_task.text(), "[docs](u)");
        assert!(dash.new_run_screen, "still on the screen");
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
        dash.new_run_task.area_mut().insert_str("  ship it  ");

        dash.handle_new_run_key(ctrl(KeyCode::Enter));

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
        dash.new_run_task.area_mut().insert_str("   ");

        dash.handle_new_run_key(ctrl(KeyCode::Enter));
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
        dash.new_run_task.area_mut().insert_str("do a thing");

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
            run_id: Some("run-1".to_string()),
        });
        dash.inject_spawn_outcome_for_test(SpawnOutcome {
            message: "boom".to_string(),
            ok: false,
            run_id: None,
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
                yolo: false,
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
                yolo: false,
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
                yolo: false,
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

    // ─── unattended runs ─────────────────────────────────────────────────────

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    /// The first Ctrl-Y of a sitting warns rather than arming anything, and the
    /// warning has to be accepted before the setting changes.
    #[test]
    fn the_first_unattended_toggle_asks_before_it_arms() {
        let mut dash = make_test_dashboard();
        dash.new_run_screen = true;
        assert!(!dash.new_run_yolo, "off until somebody says otherwise");

        dash.handle_key(ctrl(KeyCode::Char('y')));
        assert!(dash.pending_confirm.is_some(), "it asks first");
        assert!(!dash.new_run_yolo, "and arms nothing until answered");

        // Declining leaves it off.
        dash.handle_key(key(KeyCode::Esc));
        assert!(dash.pending_confirm.is_none());
        assert!(!dash.new_run_yolo);

        // Accepting turns it on.
        dash.handle_key(ctrl(KeyCode::Char('y')));
        dash.handle_key(key(KeyCode::Char('y')));
        assert!(dash.new_run_yolo);

        // Turning it back off never asks: nothing needs confirming about
        // choosing to be asked more.
        dash.handle_key(ctrl(KeyCode::Char('y')));
        assert!(dash.pending_confirm.is_none());
        assert!(!dash.new_run_yolo);
    }

    /// The sequence that lost a six-hour run its file writes: unattended on
    /// (confirmed), off again, on again, and Enter on the re-asked warning.
    /// The dialog's focus starts on "Keep asking me", so Enter declines, and
    /// before this the only trace was a "Cancelled" log line nobody reads.
    /// The setting stays off (that is the dialog's safe default), but the
    /// screen now says so where the person is looking.
    #[test]
    fn declining_the_re_asked_warning_with_enter_says_unattended_stayed_off() {
        let mut dash = make_test_dashboard();
        dash.new_run_screen = true;

        dash.handle_key(ctrl(KeyCode::Char('y')));
        dash.handle_key(key(KeyCode::Char('y')));
        assert!(dash.new_run_yolo, "on, confirmed");
        dash.handle_key(ctrl(KeyCode::Char('y')));
        assert!(!dash.new_run_yolo, "off again");

        dash.toasts.clear();
        dash.handle_key(ctrl(KeyCode::Char('y')));
        assert!(
            dash.pending_confirm.is_some(),
            "not silenced, so it asks again"
        );
        dash.handle_key(key(KeyCode::Enter));
        assert!(dash.pending_confirm.is_none());
        assert!(
            !dash.new_run_yolo,
            "Enter on the focused Keep-asking button declines"
        );
        let toast = dash
            .toasts
            .last()
            .map(|t| t.message.clone())
            .unwrap_or_default();
        assert!(
            toast.contains("stays off") && toast.contains("Ctrl-Y"),
            "the decline is said out loud: {toast:?}"
        );
        let help = dash.new_run_help_bar_text();
        assert!(help.contains("unattended: off"), "{help}");

        dash.handle_key(ctrl(KeyCode::Char('y')));
        dash.handle_key(key(KeyCode::Char('y')));
        assert!(dash.new_run_yolo);
        assert!(dash.new_run_help_bar_text().contains("unattended: on"));
    }

    /// The box silences the warning for the rest of the sitting, and only for the
    /// warning: the setting itself still has to be turned on each time.
    #[test]
    fn dont_ask_again_lasts_the_session_but_does_not_arm_anything() {
        let mut dash = make_test_dashboard();
        dash.new_run_screen = true;

        dash.handle_key(ctrl(KeyCode::Char('y')));
        // Space ticks the box, then Enter on the focused button would decline, so
        // answer explicitly.
        dash.handle_key(key(KeyCode::Char(' ')));
        dash.handle_key(key(KeyCode::Char('y')));
        assert!(dash.new_run_yolo);
        assert!(dash.yolo_warning_silenced);

        // Off, then on again: no second warning.
        dash.handle_key(ctrl(KeyCode::Char('y')));
        dash.handle_key(ctrl(KeyCode::Char('y')));
        assert!(dash.pending_confirm.is_none(), "it was told not to ask");
        assert!(dash.new_run_yolo);

        // Re-opening the screen still resets the setting, silence or not: this is
        // the half that stops somebody leaving it armed and forgetting.
        dash.open_new_run_screen();
        assert!(!dash.new_run_yolo);
        assert!(dash.yolo_warning_silenced, "but the silence holds");
    }

    /// A fresh dashboard has forgotten the silence entirely, which is what makes
    /// it a session and not a preference.
    #[test]
    fn a_new_dashboard_asks_again() {
        let mut dash = make_test_dashboard();
        dash.new_run_screen = true;
        dash.handle_key(ctrl(KeyCode::Char('y')));
        dash.handle_key(key(KeyCode::Char(' ')));
        dash.handle_key(key(KeyCode::Char('y')));
        assert!(dash.yolo_warning_silenced);

        let fresh = make_test_dashboard();
        assert!(
            !fresh.yolo_warning_silenced,
            "closing the dashboard is what expires it"
        );
        assert!(!fresh.new_run_yolo);
    }

    /// The setting reaches the spawn rather than only the screen.
    #[test]
    fn the_setting_travels_with_the_run() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents/alpha"), "alpha", "first");
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.new_run_focus = NewRunPane::Task;
        dash.new_run_task.area_mut().insert_str("do the thing");
        dash.new_run_yolo = true;

        dash.submit_new_run();

        let cmd = dash
            .spawn_cmd_rx_for_test()
            .try_recv()
            .expect("a run was sent");
        assert!(cmd.yolo, "the run carries what the screen was set to");
        assert_eq!(cmd.task, "do the thing");
    }
}
