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
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent};
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
    /// The path or text typed for it.
    pub(super) edit: LineEdit,
}

impl NewRunInput {
    /// The dim note beside the key: what the region takes, and whether it
    /// is required.
    fn note(&self) -> String {
        let mut bits: Vec<String> = Vec::new();
        if !self.accepts.is_empty() {
            bits.push(self.accepts.join(" "));
        }
        if self.required {
            bits.push("required".to_string());
        }
        match bits.is_empty() {
            true => String::new(),
            false => format!(" ({})", bits.join(", ")),
        }
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
                                edit: LineEdit::new(String::new(), false),
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
                let Some(slot) = self.new_run_inputs.get_mut(self.new_run_input_selected) else {
                    return;
                };
                match slot.edit.handle_key(&key) {
                    EditOutcome::Commit if self.new_run_input_selected >= last => {
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
            let value = slot.edit.display_spans(true);
            if slot.edit.value().is_empty() && !on {
                spans.push(Span::styled(
                    "a file in the working directory, or text",
                    Style::default().fg(C_DIM),
                ));
            } else {
                spans.extend(value.spans);
            }
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
        type_str(&mut dash, "hero.png");
        dash.handle_new_run_key(key(KeyCode::Enter));
        assert_eq!(dash.new_run_input_selected, 1, "Enter moves down a slot");
        assert_eq!(dash.new_run_inputs[0].edit.value(), "hero.png");
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
        dash.new_run_input_selected = 0;
        assert_eq!(dash.new_run_inputs[0].edit.value(), "hero.png");
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
        assert!(
            text.contains("a file in the working directory, or text"),
            "{text}"
        );
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
}
