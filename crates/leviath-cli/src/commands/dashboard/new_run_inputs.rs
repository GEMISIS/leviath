//! The new-run screen's Inputs pane: one slot per caller-input region the
//! selected blueprint declares (`seed = "input"`, or a named key), so a file
//! or a line of text can be sent straight to `pictures` or `diff` the way
//! `lev run --pictures @photo.png` does, instead of every `@path` landing in
//! the task region.
//!
//! A slot takes what the `--<region>` flag takes: `@file` attaches the file
//! (or seeds the region with its text when it is text), a bare path that
//! names a file in the working directory does the same, and anything else is
//! the region's text, with `@path` tokens inside it attached beside it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leviath_core::mime::InboundPart;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use super::state::Dashboard;
use super::theme::*;
use super::types::{ClickTarget, NewRunPane};
use crate::commands::run::attach::{cli_registry, read_region_input};
use crate::tui::widgets::line_edit::{EditOutcome, LineEdit};

/// One caller-input region of the selected blueprint, and what was typed
/// for it.
#[derive(Debug)]
pub(super) struct NewRunInput {
    /// The caller key, what `--<key>` names on the command line.
    pub(super) key: String,
    /// The region the key seeds.
    pub(super) region: String,
    /// The mime type patterns the region takes; empty means anything.
    pub(super) accepts: Vec<String>,
    /// Whether the run refuses to start without it.
    pub(super) required: bool,
    /// How many files the region holds; 1 unless the blueprint says more.
    pub(super) max_stored: usize,
    /// The text typed for it (for a region that takes text).
    pub(super) edit: LineEdit,
    /// The files chosen for it through the picker, workdir-relative. Filled
    /// for a region that takes files, and the reason a file no longer needs an
    /// `@` in front of a typed name to attach.
    pub(super) files: Vec<PathBuf>,
}

impl NewRunInput {
    /// Whether the region takes text a person would type: it says so, or it
    /// takes anything.
    pub(super) fn takes_text(&self) -> bool {
        self.accepts.is_empty()
            || self
                .accepts
                .iter()
                .any(|p| p == "*/*" || p.starts_with("text/"))
    }

    /// Whether the region takes a file: it names a non-text type, or it takes
    /// anything.
    pub(super) fn takes_files(&self) -> bool {
        self.accepts.is_empty()
            || self
                .accepts
                .iter()
                .any(|p| p == "*/*" || !p.starts_with("text/"))
    }

    /// The dim note beside the key: what the region takes, how many, and
    /// whether it is required.
    fn note(&self) -> String {
        let mut bits: Vec<String> = Vec::new();
        if !self.accepts.is_empty() {
            bits.push(self.accepts.join(" "));
        }
        if self.max_stored > 1 {
            bits.push(format!("up to {}", self.max_stored));
        }
        if self.required {
            bits.push("required".to_string());
        }
        match bits.is_empty() {
            true => String::new(),
            false => format!(" ({})", bits.join(", ")),
        }
    }

    /// The spans shown for the row's value: the chosen files for a file
    /// region, the typed text otherwise, or a prompt when it is empty.
    fn value_spans(&self, on: bool) -> Vec<Span<'static>> {
        // A file-only region shows the files it holds, never a text cursor.
        if self.takes_files() && !self.takes_text() {
            return self.file_spans(on);
        }
        let mut spans = self.edit.display_spans(true).spans;
        if self.edit.value().is_empty() && !on {
            spans = vec![Span::styled(
                match self.takes_files() {
                    true => "text, or Ctrl+O to choose files",
                    false => "text",
                },
                Style::default().fg(C_DIM),
            )];
        }
        // A region that takes both shows any chosen files after the text.
        if self.takes_files() && !self.files.is_empty() {
            spans.push(Span::styled(
                format!("  +{}", self.file_summary()),
                Style::default().fg(C_ACCENT),
            ));
        }
        spans
    }

    /// The spans for a file region: the chosen names, or a prompt to choose.
    fn file_spans(&self, on: bool) -> Vec<Span<'static>> {
        if self.files.is_empty() {
            let prompt = match on {
                true => "Enter to choose files",
                false => "no files chosen",
            };
            return vec![Span::styled(prompt, Style::default().fg(C_DIM))];
        }
        vec![Span::styled(
            self.file_summary(),
            Style::default().fg(C_ACTIVE),
        )]
    }

    /// The chosen files as a short chip, e.g. `hero.png, villain.png (2/3)`.
    fn file_summary(&self) -> String {
        let names: Vec<String> = self
            .files
            .iter()
            .map(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| p.to_string_lossy().to_string())
            })
            .collect();
        let count = match self.max_stored > 1 {
            true => format!(" ({}/{})", self.files.len(), self.max_stored),
            false => String::new(),
        };
        format!("{}{count}", names.join(", "))
    }
}

/// What the slots resolve to when the run starts.
#[derive(Debug, Default)]
pub(super) struct ResolvedInputs {
    /// Text seeds by caller key, what `--<key> text` sends.
    pub(super) regions: HashMap<String, String>,
    /// Files, each already naming its region.
    pub(super) parts: Vec<InboundPart>,
    /// `@path` tokens that looked like files but named none.
    pub(super) unresolved: Vec<String>,
}

impl Dashboard {
    /// Rebuild the slots for the selected agent when the selection moved.
    /// Kept per agent path, so moving the cursor away and back keeps what was
    /// typed only while the same agent is selected.
    pub(super) fn sync_new_run_inputs(&mut self) {
        let Some((path, source, name)) = self
            .new_run_selected_agent()
            .map(|a| (a.path.clone(), a.source.clone(), a.name.clone()))
        else {
            self.new_run_inputs.clear();
            self.new_run_inputs_key.clear();
            return;
        };
        if self.new_run_inputs_key == path {
            return;
        }
        self.new_run_input_selected = 0;
        let blueprint = match source == "bundled" {
            true => super::graph::bundled_blueprint(&name),
            false => super::graph::load_blueprint(&path),
        };
        self.new_run_inputs_key = path;
        self.new_run_inputs = blueprint
            .map(|bp| {
                bp.context_layout
                    .regions
                    .iter()
                    .filter_map(|r| match &r.seed {
                        // The `task` key is the task box; every other key gets a slot.
                        Some(leviath_core::layout::RegionSeed::CallerInput { name })
                            if name != "task" =>
                        {
                            Some(NewRunInput {
                                key: name.clone(),
                                region: r.name.clone(),
                                accepts: r.accepts.clone(),
                                required: r.required,
                                max_stored: r.max_stored.unwrap_or(1).max(1),
                                edit: LineEdit::new(String::new(), false),
                                files: Vec::new(),
                            })
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
    }

    /// Whether the screen has an Inputs pane to move the keys to.
    pub(super) fn new_run_has_inputs(&self) -> bool {
        !self.new_run_inputs.is_empty()
    }

    /// Keys while the Inputs pane has them: `↑`/`↓` pick a slot, `Enter`
    /// moves down and on to the task after the last, `Tab` goes to the task,
    /// `Shift+Tab` and `Esc` back to the agents; anything else types into the
    /// slot.
    pub(super) fn handle_new_run_inputs_key(&mut self, key: KeyEvent) {
        let last = self.new_run_inputs.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::BackTab => self.new_run_focus = NewRunPane::Agents,
            KeyCode::Tab => self.new_run_focus = NewRunPane::Task,
            KeyCode::Up => {
                self.new_run_input_selected = self.new_run_input_selected.saturating_sub(1);
            }
            KeyCode::Down => {
                self.new_run_input_selected = (self.new_run_input_selected + 1).min(last);
            }
            _ => {
                let idx = self.new_run_input_selected;
                let Some(slot) = self.new_run_inputs.get(idx) else {
                    return;
                };
                let takes_text = slot.takes_text();
                let takes_files = slot.takes_files();
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                // A file region opens the picker: on Enter or Space when it is
                // file-only (there is nothing to type), and always on Ctrl+O.
                let open_picker = takes_files
                    && ((ctrl && matches!(key.code, KeyCode::Char('o' | 'O')))
                        || (!takes_text
                            && matches!(key.code, KeyCode::Enter | KeyCode::Char(' '))));
                if open_picker {
                    self.open_new_run_picker(idx);
                    return;
                }
                if !takes_text {
                    return;
                }
                // `idx` was just proven valid by the `get` above, and no key
                // changes the rows here, so it still is.
                let slot = &mut self.new_run_inputs[idx];
                match slot.edit.handle_key(&key) {
                    EditOutcome::Commit if idx >= last => {
                        self.new_run_focus = NewRunPane::Task;
                    }
                    EditOutcome::Commit => self.new_run_input_selected += 1,
                    EditOutcome::Cancel | EditOutcome::Pending => {}
                }
            }
        }
    }

    /// A click on slot `index`: the pane takes the keys and the slot is the
    /// one under the cursor.
    pub(super) fn click_new_run_input(&mut self, index: usize) {
        if index < self.new_run_inputs.len() {
            self.new_run_focus = NewRunPane::Inputs;
            self.new_run_input_selected = index;
        }
    }

    /// Resolve every filled slot into what the spawn sends. A slot the file
    /// tools cannot read is an error naming it, so a typo is a toast here
    /// rather than a run that started without its picture.
    pub(super) fn new_run_input_values(&self) -> Result<ResolvedInputs, String> {
        let workdir: &Path = &self.new_run_ctx.workdir;
        let registry = cli_registry();
        let mut out = ResolvedInputs::default();
        for slot in &self.new_run_inputs {
            // Files chosen through the picker attach straight to the region,
            // no `@` and no typed name.
            for rel in &slot.files {
                let rel = rel.to_string_lossy();
                let part = crate::commands::run::attach::read_part(&rel, workdir)
                    .map_err(|e| format!("{}: {e}", slot.key))?
                    .in_region(&slot.region);
                out.parts.push(part);
            }
            let raw = slot.edit.value().trim().to_string();
            if raw.is_empty() {
                continue;
            }
            // A bare path that names a file is the file, as it is on the
            // command line's `--<region> @file`; a slot is for one input, so
            // the `@` is implied.
            let value = match !raw.starts_with('@') && workdir.join(&raw).is_file() {
                true => format!("@{raw}"),
                false => raw,
            };
            let read = read_region_input(&slot.region, &value, workdir, &registry)
                .map_err(|e| format!("{}: {e}", slot.key))?;
            if !read.text.is_empty() {
                out.regions.insert(slot.key.clone(), read.text);
            }
            out.parts.extend(read.parts);
            out.unresolved.extend(read.unresolved);
        }
        Ok(out)
    }

    /// The pane's height when it is drawn: a border and one row per slot.
    pub(super) fn new_run_inputs_height(&self) -> u16 {
        match self.new_run_inputs.len() {
            0 => 0,
            n => (n as u16).saturating_add(2),
        }
    }

    /// Draw the slots into `area`, registering each row for the mouse.
    pub(super) fn draw_new_run_inputs(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.new_run_focus == NewRunPane::Inputs;
        let agent = self
            .new_run_selected_agent()
            .map(|a| a.name.clone())
            .unwrap_or_default();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(focus_colour(focused)))
            .title(Span::styled(
                format!(" Inputs for {agent} "),
                Style::default()
                    .fg(focus_colour(focused))
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let label_w = self
            .new_run_inputs
            .iter()
            .map(|s| s.key.chars().count() + s.note().chars().count())
            .max()
            .unwrap_or(0)
            .min(inner.width.saturating_sub(12) as usize)
            + 2;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut rows: Vec<(Rect, ClickTarget)> = Vec::new();
        for (i, slot) in self.new_run_inputs.iter().enumerate() {
            let on = focused && i == self.new_run_input_selected;
            let label = format!("{}{}", slot.key, slot.note());
            let label = fit(&label, label_w.saturating_sub(2));
            let mut spans = vec![
                Span::styled(if on { "› " } else { "  " }, Style::default().fg(C_ACCENT)),
                Span::styled(
                    format!("{label:<width$}", width = label_w),
                    if on {
                        Style::default().fg(C_ACTIVE).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(C_MUTED)
                    },
                ),
            ];
            spans.extend(slot.value_spans(on));
            lines.push(Line::from(spans));
            // The pane is sized to hold every slot (`new_run_inputs_height`),
            // and it is not drawn at all when the column cannot give it that
            // many rows, so each slot's row is always inside `inner`.
            let row = Rect {
                x: inner.x,
                y: inner.y.saturating_add(i as u16),
                width: inner.width,
                height: 1,
            };
            rows.push((row, ClickTarget::NewRunInput(i)));
        }
        for (row, target) in rows {
            self.register_click(row, target);
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// The border and title colour of a pane, lit when it has the keys.
fn focus_colour(focused: bool) -> ratatui::style::Color {
    match focused {
        true => C_BORDER_FOCUS,
        false => C_BORDER,
    }
}

/// `text` cut to `room` cells with an ellipsis.
fn fit(text: &str, room: usize) -> String {
    if text.chars().count() <= room {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(room.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::make_test_dashboard;
    use crate::commands::dashboard::types::NewRunContext;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// An agent with two caller inputs beside its task: a typed picture slot
    /// and a plain notes slot.
    fn write_agent(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("agent.leviath"),
            "[agent]\nname = \"looker\"\nversion = \"0.1.0\"\ndescription = \"looks\"\n\n\
             [stages.main]\nmode = \"autonomous\"\n\n\
             [stages.main.model]\nprovider = \"anthropic\"\nmodel = \"claude-sonnet-5\"\n\n\
             [context.regions]\n\
             task = { kind = \"pinned\", max_tokens = 1000, seed = \"task\" }\n\
             pictures = { kind = \"pinned\", max_tokens = 1000, seed = \"input\", accepts = [\"image/*\"], required = true }\n\
             notes = { kind = \"pinned\", max_tokens = 1000, seed = \"input\" }\n\
             conversation = { kind = \"sliding_window\", max_items = 20, max_tokens = 10000 }\n",
        )
        .unwrap();
    }

    fn dash_at(dir: &Path) -> Dashboard {
        let mut dash = make_test_dashboard();
        dash.new_run_ctx = NewRunContext {
            agents_dir: dir.join("agents"),
            config_path: dir.join("config.toml"),
            workdir: dir.join("work"),
        };
        std::fs::create_dir_all(dir.join("work")).unwrap();
        // The catalog lists the bundled blueprints too, ahead of `looker`
        // alphabetically; the screen opens on the agent last launched.
        dash.last_launched_agent = Some("looker".to_string());
        dash
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(dash: &mut Dashboard, s: &str) {
        for c in s.chars() {
            dash.handle_new_run_key(key(KeyCode::Char(c)));
        }
    }

    fn screen(dash: &mut Dashboard) -> String {
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect()
    }

    /// The slots follow the selected agent: one per caller-input region, in
    /// the manifest's order, with the task left to the task box.
    #[test]
    fn the_slots_are_the_blueprints_caller_inputs() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        let keys: Vec<&str> = dash.new_run_inputs.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["pictures", "notes"]);
        assert_eq!(dash.new_run_inputs[0].region, "pictures");
        assert_eq!(dash.new_run_inputs[0].accepts, ["image/*"]);
        assert!(dash.new_run_inputs[0].required);
        assert_eq!(dash.new_run_inputs[0].note(), " (image/*, required)");
        assert_eq!(dash.new_run_inputs[1].note(), "");
        assert!(dash.new_run_has_inputs());
        assert_eq!(dash.new_run_inputs_height(), 4);
        // The same agent again keeps the slots; no agent clears them.
        dash.sync_new_run_inputs();
        assert_eq!(dash.new_run_inputs.len(), 2);
        dash.new_run_agents.clear();
        dash.sync_new_run_inputs();
        assert!(!dash.new_run_has_inputs());
        assert_eq!(dash.new_run_inputs_height(), 0);
    }

    /// Tab walks agents → inputs → task → start and back; Enter in the last
    /// slot moves on to the task; Esc goes back to the agents.
    #[test]
    fn the_keys_walk_the_slots_and_the_panes() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.handle_new_run_key(key(KeyCode::Tab));
        assert_eq!(dash.new_run_focus, NewRunPane::Inputs);
        dash.handle_new_run_key(key(KeyCode::Down));
        assert_eq!(dash.new_run_input_selected, 1);
        dash.handle_new_run_key(key(KeyCode::Down));
        assert_eq!(dash.new_run_input_selected, 1, "stops at the last");
        dash.handle_new_run_key(key(KeyCode::Up));
        assert_eq!(dash.new_run_input_selected, 0);
        // Row 0 (pictures) takes images only: Enter opens the picker, not text.
        assert_eq!(dash.new_run_input_selected, 0);
        dash.handle_new_run_key(key(KeyCode::Enter));
        assert!(dash.new_run_picker_open(), "a file row opens the picker");
        dash.handle_new_run_key(key(KeyCode::Esc));
        assert!(!dash.new_run_picker_open());
        // Row 1 (notes) takes text: typing lands there and Enter on the last
        // slot moves on to the task.
        dash.new_run_input_selected = 1;
        type_str(&mut dash, "be brief");
        assert_eq!(dash.new_run_inputs[1].edit.value(), "be brief");
        dash.handle_new_run_key(key(KeyCode::Enter));
        assert_eq!(dash.new_run_focus, NewRunPane::Task, "and on from the last");
        dash.handle_new_run_key(key(KeyCode::BackTab));
        assert_eq!(dash.new_run_focus, NewRunPane::Inputs);
        dash.handle_new_run_key(key(KeyCode::Tab));
        assert_eq!(dash.new_run_focus, NewRunPane::Task);
        dash.new_run_focus = NewRunPane::Inputs;
        dash.handle_new_run_key(key(KeyCode::Esc));
        assert_eq!(dash.new_run_focus, NewRunPane::Agents);
        dash.new_run_focus = NewRunPane::Inputs;
        dash.handle_new_run_key(key(KeyCode::BackTab));
        assert_eq!(dash.new_run_focus, NewRunPane::Agents);
        // A slot's own Esc is the pane's Esc, never a cancel that eats text.
        dash.new_run_focus = NewRunPane::Inputs;
        dash.new_run_input_selected = 1;
        assert_eq!(dash.new_run_inputs[1].edit.value(), "be brief");
        // With no slots the pane is skipped both ways.
        dash.new_run_agents.clear();
        dash.sync_new_run_inputs();
        dash.new_run_focus = NewRunPane::Agents;
        dash.handle_new_run_key(key(KeyCode::Tab));
        assert_eq!(dash.new_run_focus, NewRunPane::Task);
        dash.handle_new_run_key(key(KeyCode::BackTab));
        assert_eq!(dash.new_run_focus, NewRunPane::Agents);
        // Keys on an empty Inputs pane do nothing.
        dash.new_run_focus = NewRunPane::Inputs;
        dash.handle_new_run_key(key(KeyCode::Char('x')));
        assert!(dash.new_run_inputs.is_empty());
    }

    /// A bare file name attaches the file to its region, `@file` does too,
    /// text seeds the region under its caller key, a text file seeds it with
    /// the file's text, and a path that names nothing is an error naming the
    /// slot.
    #[test]
    fn the_slots_resolve_the_way_the_region_flags_do() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        let work = dir.path().join("work");
        std::fs::write(work.join("hero.png"), b"\x89PNG\r\n\x1a\nbody").unwrap();
        std::fs::write(work.join("notes.md"), "read me").unwrap();
        dash.open_new_run_screen();
        // Nothing typed: nothing sent.
        let none = dash.new_run_input_values().unwrap();
        assert!(none.regions.is_empty() && none.parts.is_empty());

        dash.new_run_inputs[0].edit = LineEdit::new("hero.png", false);
        dash.new_run_inputs[1].edit = LineEdit::new("look at @hero.png and @ghost.png", false);
        let got = dash.new_run_input_values().unwrap();
        assert_eq!(got.parts.len(), 2);
        assert_eq!(got.parts[0].region.as_deref(), Some("pictures"));
        assert_eq!(got.parts[0].name, "hero.png");
        assert_eq!(got.parts[1].region.as_deref(), Some("notes"));
        assert_eq!(
            got.regions.get("notes").map(String::as_str),
            Some("look at @hero.png and @ghost.png")
        );
        assert_eq!(got.unresolved, ["ghost.png"]);

        dash.new_run_inputs[0].edit = LineEdit::new("@hero.png", false);
        dash.new_run_inputs[1].edit = LineEdit::new("@notes.md", false);
        let got = dash.new_run_input_values().unwrap();
        assert_eq!(got.parts.len(), 1);
        assert_eq!(
            got.regions.get("notes").map(String::as_str),
            Some("read me")
        );

        dash.new_run_inputs[0].edit = LineEdit::new("@missing.png", false);
        let err = dash.new_run_input_values().unwrap_err();
        assert!(err.starts_with("pictures:"), "{err}");
    }

    /// Starting the run sends the slots: the picture as a part in its region,
    /// the notes as a seed, beside whatever the task named.
    #[test]
    fn the_run_carries_the_slots() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        let work = dir.path().join("work");
        std::fs::write(work.join("hero.png"), b"\x89PNG\r\n\x1a\nbody").unwrap();
        dash.open_new_run_screen();
        dash.new_run_inputs[0].edit = LineEdit::new("hero.png", false);
        dash.new_run_inputs[1].edit = LineEdit::new("be brief", false);
        dash.new_run_task.area_mut().insert_str("describe it");
        dash.submit_new_run();
        let cmd = dash.spawn_cmd_rx_for_test().try_recv().unwrap();
        assert_eq!(cmd.parts.len(), 1);
        assert_eq!(cmd.parts[0].region.as_deref(), Some("pictures"));
        assert_eq!(
            cmd.regions.get("notes").map(String::as_str),
            Some("be brief")
        );
        // A slot that cannot be read stops the start with a toast naming it.
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.new_run_inputs[0].edit = LineEdit::new("@nope.png", false);
        dash.new_run_task.area_mut().insert_str("describe it");
        dash.submit_new_run();
        assert!(dash.spawn_cmd_rx_for_test().try_recv().is_err());
        let toasts = dash.toast_messages_for_test();
        assert!(toasts.iter().any(|t| t.contains("pictures:")), "{toasts:?}");
    }

    /// The pane draws between the preview and the task with a row per slot,
    /// says what each takes, and a click on a row picks it.
    #[test]
    fn the_pane_draws_its_rows_and_takes_a_click() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        let text = screen(&mut dash);
        assert!(text.contains("Inputs for looker"), "{text}");
        assert!(text.contains("pictures (image/*, required)"), "{text}");
        // The pictures row takes images only, so it prompts for a file rather
        // than a line of text.
        assert!(text.contains("no files chosen"), "{text}");
        let rect = dash
            .click_targets
            .iter()
            .find(|(_, t)| *t == ClickTarget::NewRunInput(1))
            .map(|(r, _)| *r)
            .expect("the second slot is clickable");
        assert!(dash.handle_click(rect.x + 2, rect.y));
        assert_eq!(dash.new_run_focus, NewRunPane::Inputs);
        assert_eq!(dash.new_run_input_selected, 1);
        dash.click_new_run_input(9);
        assert_eq!(
            dash.new_run_input_selected, 1,
            "a row that is not there is ignored"
        );
        // A focused slot shows its text where the placeholder was.
        dash.new_run_inputs[1].edit = LineEdit::new("be brief", false);
        let text = screen(&mut dash);
        assert!(text.contains("be brief"), "{text}");
        assert_eq!(fit("abcdef", 4), "abc…");
    }

    /// A file slot shows the files it holds: the names, the count against the
    /// cap for a many-file slot, and a chip of extra files beside typed text on
    /// a slot that takes both.
    #[test]
    fn a_file_row_shows_its_chosen_files() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        // Focused and empty, the file slot prompts to choose.
        dash.new_run_focus = NewRunPane::Inputs;
        dash.new_run_input_selected = 0;
        let text = screen(&mut dash);
        assert!(text.contains("Enter to choose files"), "{text}");
        // A chosen path with no final component still renders its name.
        dash.new_run_inputs[0].files = vec![PathBuf::from("..")];
        let text = screen(&mut dash);
        assert!(text.contains("pictures"), "{text}");
        // The pictures slot (image/*) holds one file: the name shows, no count.
        dash.new_run_inputs[0].files = vec![PathBuf::from("out/hero.png")];
        let text = screen(&mut dash);
        assert!(text.contains("hero.png"), "{text}");
        // Bumped to a many-file slot: the count against the cap shows.
        dash.new_run_inputs[0].max_stored = 3;
        dash.new_run_inputs[0].files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
        let text = screen(&mut dash);
        assert!(text.contains("(2/3)"), "{text}");
        // The notes slot takes anything, so text and a file chip sit together.
        dash.new_run_inputs[1].edit = LineEdit::new("look", false);
        dash.new_run_inputs[1].files = vec![PathBuf::from("c.png")];
        let text = screen(&mut dash);
        assert!(text.contains("look"), "{text}");
        assert!(text.contains("+c.png"), "{text}");
    }

    /// A file slot opens the picker by key: Space (or Enter) on a file-only
    /// slot, and Ctrl+O on one that also takes text, which still types
    /// otherwise.
    #[test]
    fn a_file_row_opens_the_picker_by_key() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.new_run_focus = NewRunPane::Inputs;
        // Space on the file-only pictures slot opens the picker.
        dash.new_run_input_selected = 0;
        dash.handle_new_run_key(key(KeyCode::Char(' ')));
        assert!(dash.new_run_picker_open());
        dash.handle_new_run_key(key(KeyCode::Esc));
        // Ctrl+O on the notes slot (which takes anything) opens it too.
        dash.new_run_input_selected = 1;
        dash.handle_new_run_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(dash.new_run_picker_open());
        dash.handle_new_run_key(key(KeyCode::Esc));
        // A plain letter on that slot still types.
        dash.handle_new_run_key(key(KeyCode::Char('h')));
        assert!(!dash.new_run_picker_open());
        assert_eq!(dash.new_run_inputs[1].edit.value(), "h");
        // On the file-only slot, a control chord that is not Ctrl+O, and a
        // plain letter, both do nothing: no picker, no text.
        dash.new_run_input_selected = 0;
        dash.handle_new_run_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert!(!dash.new_run_picker_open());
        dash.handle_new_run_key(key(KeyCode::Char('z')));
        assert!(!dash.new_run_picker_open());
        assert!(dash.new_run_inputs[0].edit.value().is_empty());
    }

    /// Enter on a text slot that is not the last moves to the next slot.
    #[test]
    fn enter_advances_between_text_slots() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agents").join("noter");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("agent.leviath"),
            "[agent]\nname = \"noter\"\nversion = \"0.1.0\"\ndescription = \"notes\"\n\n\
             [stages.main]\nmode = \"autonomous\"\n\n\
             [stages.main.model]\nprovider = \"anthropic\"\nmodel = \"claude-sonnet-5\"\n\n\
             [context.regions]\n\
             task = { kind = \"pinned\", max_tokens = 1000, seed = \"task\" }\n\
             one = { kind = \"pinned\", max_tokens = 1000, seed = \"input\", accepts = [\"text/*\"] }\n\
             two = { kind = \"pinned\", max_tokens = 1000, seed = \"input\", accepts = [\"text/*\"] }\n",
        )
        .unwrap();
        let mut dash = make_test_dashboard();
        dash.new_run_ctx = NewRunContext {
            agents_dir: dir.path().join("agents"),
            config_path: dir.path().join("config.toml"),
            workdir: dir.path().join("work"),
        };
        std::fs::create_dir_all(dir.path().join("work")).unwrap();
        dash.last_launched_agent = Some("noter".to_string());
        dash.open_new_run_screen();
        assert_eq!(dash.new_run_inputs.len(), 2);
        dash.new_run_focus = NewRunPane::Inputs;
        dash.new_run_input_selected = 0;
        dash.handle_new_run_key(key(KeyCode::Char('a')));
        dash.handle_new_run_key(key(KeyCode::Enter));
        assert_eq!(
            dash.new_run_input_selected, 1,
            "Enter on a non-last text slot advances"
        );
    }

    /// A chosen file that has gone missing stops the start with an error naming
    /// the slot, rather than a run that began without it.
    #[test]
    fn a_missing_file_slot_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(&dir.path().join("agents").join("looker"));
        let mut dash = dash_at(dir.path());
        dash.open_new_run_screen();
        dash.new_run_inputs[0].files = vec![PathBuf::from("gone.png")];
        let err = dash.new_run_input_values().unwrap_err();
        assert!(err.starts_with("pictures:"), "{err}");
    }
}
