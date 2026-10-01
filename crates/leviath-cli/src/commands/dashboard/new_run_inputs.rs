//! The new-run screen's Inputs pane: one row per input the selected
//! blueprint declares beside its task, each drawn for its type.
//!
//! - A `text` input takes what the `--<name>` flag takes: `@file` attaches the
//!   file (or seeds the input with its text when it is text), a bare path that
//!   names a file in the working directory does the same, and anything else is
//!   the input's text, with `@path` tokens inside it attached beside it. One
//!   whose region takes files opens a picker too.
//! - A `choice` is a picker: `←`/`→` or `Space` moves through its options.
//! - A `bool` is a toggle: `Space` flips it.
//! - A `file` input opens the picker; the file is attached and the input names
//!   it.
//! - Anything else (a number, a list, a duration, a URL) is typed as text and
//!   read by its type when the run starts.
//!
//! Every input is checked against its declaration before the run is sent,
//! and each problem is shown beside its row: a number out of range, a list
//! too long, a required input left empty. A problem the daemon finds with an
//! input comes back to the same row.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leviath_core::mime::InboundPart;
use leviath_runtime::spec::inputs::{CheckCtx, InputDecl, InputSlot, InputType, RawInput};
use leviath_runtime::spec::issues::{IssueCode, PathSeg, SpawnIssue, SpawnIssues, SpecPath};
use leviath_runtime::spec::names::ChoiceName;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use super::helpers::{focus_colour, format_tokens, truncate};
use super::state::Dashboard;
use super::theme::*;
use super::types::{ClickTarget, NewRunPane};
use crate::commands::run::attach::{cli_registry, read_region_input};
use crate::commands::run::inputs::typed_value;
use crate::commands::run::request::TASK_INPUT;
use crate::tui::widgets::line_edit::{EditOutcome, LineEdit};
use crate::tui::widgets::list_cursor;

/// One input of the selected blueprint, and what was entered for it.
#[derive(Debug)]
pub(super) struct NewRunInput {
    /// The input's name, what `--input <name>=` names on the command line.
    pub(super) key: String,
    /// The region a text input fills, where the files chosen for it go.
    pub(super) region: String,
    /// The input's declared type, which decides how its row is drawn and how
    /// what is entered is read.
    pub(super) ty: InputType,
    /// The mime type patterns the input takes; empty means anything.
    pub(super) accepts: Vec<String>,
    /// Whether the run refuses to start without it.
    pub(super) required: bool,
    /// The region's token budget, resolved against the entry model's context
    /// window. `0` means none applies.
    pub(super) max_tokens: usize,
    /// The text typed for it (for an input typed as text).
    pub(super) edit: LineEdit,
    /// The files chosen for it through the picker, workdir-relative.
    pub(super) files: Vec<PathBuf>,
    /// A choice's picked option, by index, or a toggle's state (`1` on, `0`
    /// off). `None` until one is picked: an input left alone is not sent.
    pub(super) pick: Option<usize>,
    /// The problem the last check found with this input, shown beside it.
    pub(super) issue: Option<String>,
}

impl NewRunInput {
    /// A row for `decl`, with nothing entered: a declared default is
    /// pre-picked for a choice or a toggle and pre-typed for anything else.
    pub(super) fn new(decl: &InputDecl) -> Self {
        let region = decl
            .binds
            .iter()
            .find_map(|slot| match slot {
                InputSlot::Region(binding) => Some(binding.region.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| decl.name.to_string());
        let accepts = match &decl.ty {
            InputType::File { accepts } => accepts.iter().map(ToString::to_string).collect(),
            _ => Vec::new(),
        };
        let mut row = Self {
            key: decl.name.to_string(),
            region,
            ty: decl.ty.clone(),
            accepts,
            required: decl.required && decl.default.is_none(),
            max_tokens: 0,
            edit: LineEdit::new(String::new(), false),
            files: Vec::new(),
            pick: None,
            issue: None,
        };
        if let Some(default) = &decl.default {
            match (&decl.ty, default.to_raw()) {
                (InputType::Bool, RawInput::Bool(on)) => row.pick = Some(usize::from(on)),
                (InputType::Choice { options }, RawInput::Text(name)) => {
                    row.pick = options.iter().position(|o| o.as_str() == name);
                }
                (_, _) => row.edit = LineEdit::new(default.render_text(), false),
            }
        }
        row
    }

    /// Whether the row is a picker or a toggle rather than a text field.
    fn picks(&self) -> bool {
        matches!(self.ty, InputType::Choice { .. } | InputType::Bool)
    }

    /// Whether the input is a file, or a list of files.
    fn is_file(&self) -> bool {
        match &self.ty {
            InputType::File { .. } => true,
            InputType::List { item, .. } => matches!(**item, InputType::File { .. }),
            _ => false,
        }
    }

    /// Whether the row takes text a person would type: a text input whose
    /// region says it takes text (or anything), or a value typed as text.
    pub(super) fn takes_text(&self) -> bool {
        match &self.ty {
            InputType::Text { .. } => {
                self.accepts.is_empty()
                    || self
                        .accepts
                        .iter()
                        .any(|p| p == "*/*" || p.starts_with("text/"))
            }
            _ => !self.picks() && !self.is_file(),
        }
    }

    /// Whether the row takes a file: a file input, or a text input whose
    /// region names a non-text type or takes anything.
    pub(super) fn takes_files(&self) -> bool {
        match &self.ty {
            InputType::Text { .. } => {
                self.accepts.is_empty()
                    || self
                        .accepts
                        .iter()
                        .any(|p| p == "*/*" || !p.starts_with("text/"))
            }
            _ => self.is_file(),
        }
    }

    /// The dim note beside the name: the type (for anything but text), what
    /// files it takes, the token room it has, and whether it is required.
    fn note(&self) -> String {
        let mut bits: Vec<String> = Vec::new();
        if !matches!(self.ty, InputType::Text { .. } | InputType::File { .. }) {
            bits.push(self.ty.describe());
        }
        if !self.accepts.is_empty() {
            bits.push(self.accepts.join(" "));
        }
        if self.max_tokens > 0 {
            bits.push(format!("≤{} tok", format_tokens(self.max_tokens)));
        }
        if self.required {
            bits.push("required".to_string());
        }
        match bits.is_empty() {
            true => String::new(),
            false => format!(" ({})", bits.join(", ")),
        }
    }

    /// Move a choice to the next (`forward`) or previous option, or flip a
    /// toggle. Nothing picked yet starts at the first option, or on.
    fn cycle(&mut self, forward: bool) {
        // A toggle is a choice of off (0) and on (1) that starts on.
        let (count, first) = match &self.ty {
            InputType::Choice { options } => (options.len().max(1), 0),
            _ => (2, 1),
        };
        let next = match (self.pick, forward) {
            (None, _) => first,
            (Some(i), true) => (i + 1) % count,
            (Some(i), false) => (i + count - 1) % count,
        };
        self.pick = Some(next);
        self.issue = None;
    }

    /// The spans shown for the row's value, then its problem when it has one.
    fn value_spans(&self, on: bool) -> Vec<Span<'static>> {
        let mut spans = match &self.ty {
            InputType::Choice { options } => self.choice_spans(options, on),
            InputType::Bool => self.toggle_spans(),
            _ if self.takes_files() && !self.takes_text() => self.file_spans(on),
            _ => self.text_spans(on),
        };
        if let Some(issue) = &self.issue {
            spans.push(Span::styled(
                format!("  ✗ {issue}"),
                Style::default().fg(C_ERROR),
            ));
        }
        spans
    }

    /// A choice: the picked option between arrows, or a prompt to pick.
    fn choice_spans(&self, options: &[ChoiceName], on: bool) -> Vec<Span<'static>> {
        match self.pick.and_then(|i| options.get(i)) {
            Some(option) => vec![Span::styled(
                format!("‹ {option} ›"),
                Style::default().fg(C_ACTIVE),
            )],
            None => vec![Span::styled(
                match on {
                    true => "← → to choose",
                    false => "not chosen",
                },
                Style::default().fg(C_DIM),
            )],
        }
    }

    /// A toggle: a box and its state, or a prompt when it is not set.
    fn toggle_spans(&self) -> Vec<Span<'static>> {
        match self.pick {
            Some(1) => vec![Span::styled("[x] yes", Style::default().fg(C_ACTIVE))],
            Some(_) => vec![Span::styled("[ ] no", Style::default().fg(C_ACTIVE))],
            None => vec![Span::styled("[ ] not set", Style::default().fg(C_DIM))],
        }
    }

    /// A text field: what was typed, or a hint at what it wants.
    fn text_spans(&self, on: bool) -> Vec<Span<'static>> {
        let hint = match (&self.ty, self.takes_files()) {
            (InputType::Text { .. }, true) => "text, or ^O for files".to_string(),
            (InputType::Text { .. }, false) => "text".to_string(),
            (ty, _) => ty.describe(),
        };
        // Keep the hint visible whenever the field is empty, focused or not,
        // so a row never looks blank and nobody forgets what it wants.
        let mut spans = match (self.edit.value().is_empty(), on) {
            (true, true) => {
                let mut spans = self.edit.display_spans(true).spans;
                spans.push(Span::styled(format!(" {hint}"), Style::default().fg(C_DIM)));
                spans
            }
            (true, false) => vec![Span::styled(hint, Style::default().fg(C_DIM))],
            (false, _) => self.edit.display_spans(true).spans,
        };
        // A row that takes both shows any chosen files after the text.
        if self.takes_files() && !self.files.is_empty() {
            spans.push(Span::styled(
                format!("  +{}", self.file_summary()),
                Style::default().fg(C_ACCENT),
            ));
        }
        spans
    }

    /// The spans for a file row: the chosen names, or a prompt to choose.
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

    /// The chosen files as a short chip, e.g. `hero.png, villain.png (2)`.
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
        // More than one file shows the count; a single file just shows its name.
        let count = match self.files.len() > 1 {
            true => format!(" ({})", self.files.len()),
            false => String::new(),
        };
        format!("{}{count}", names.join(", "))
    }
}

/// What the rows resolve to when the run starts.
#[derive(Debug, Default)]
pub(super) struct ResolvedInputs {
    /// Each input's value, read by its type, by name: what `--input
    /// name=value` sends.
    pub(super) values: BTreeMap<String, RawInput>,
    /// Files, each already naming its region (for a text input) or named by
    /// its input (for a file input).
    pub(super) parts: Vec<InboundPart>,
    /// `@path` tokens that looked like files but named none.
    pub(super) unresolved: Vec<String>,
}

/// What a region tells a row of the input that fills it: its name, the
/// types it accepts, whether it is required, and its token room.
pub(super) type RegionRoom = (String, Vec<String>, bool, usize);

/// The rows for a graph's `inputs`, the task left to the task box. `regions`
/// are the layout's regions in order: a text input takes its region's
/// settings, and the rows follow the order of the regions they fill, with
/// any other input after them in the order declared.
pub(super) fn rows_for(inputs: &[InputDecl], regions: &[RegionRoom]) -> Vec<NewRunInput> {
    let mut rows: Vec<(usize, NewRunInput)> = inputs
        .iter()
        .filter(|decl| decl.name.as_str() != TASK_INPUT)
        .map(|decl| {
            let mut row = NewRunInput::new(decl);
            let found = regions.iter().position(|(name, ..)| *name == row.region);
            if let (InputType::Text { .. }, Some(i)) = (&row.ty, found) {
                let (_, accepts, required, max_tokens) = &regions[i];
                row.accepts = accepts.clone();
                row.required |= *required;
                row.max_tokens = *max_tokens;
            }
            (found.unwrap_or(usize::MAX), row)
        })
        .collect();
    rows.sort_by_key(|(order, _)| *order);
    rows.into_iter().map(|(_, row)| row).collect()
}

impl Dashboard {
    /// Rebuild the rows for the selected agent when the selection moved.
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
        let config_path = self.new_run_ctx.config_path.clone();
        self.new_run_inputs = blueprint
            .and_then(|bp| {
                let graph = leviath_runtime::spec::graph::RunGraph::from_blueprint(&bp).ok();
                graph.map(|graph| (bp, graph))
            })
            .map(|(bp, graph)| {
                // Resolve each region's percentage budget against the entry
                // stage's effective (smallest) model window, so a row knows
                // the token room it really has.
                let cache_path = leviath_core::paths::capability_cache_path();
                let window = entry_stage_window(&bp, &config_path, cache_path.as_deref());
                let regions: Vec<RegionRoom> = bp
                    .context_layout
                    .resolved(window)
                    .regions
                    .into_iter()
                    .map(|r| (r.name, r.accepts, r.required, r.max_tokens))
                    .collect();
                rows_for(&graph.inputs, &regions)
            })
            .unwrap_or_default();
    }

    /// Whether the screen has an Inputs pane to move the keys to.
    pub(super) fn new_run_has_inputs(&self) -> bool {
        !self.new_run_inputs.is_empty()
    }

    /// Keys while the Inputs pane has them: `↑`/`↓` pick a row, `Enter`
    /// moves down and on to the task after the last, `Tab` goes to the task,
    /// `Shift+Tab` and `Esc` back to the agents; a choice or a toggle takes
    /// `←`/`→` and `Space`, and anything else types into a text row.
    pub(super) fn handle_new_run_inputs_key(&mut self, key: KeyEvent) {
        let last = self.new_run_inputs.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::BackTab => self.new_run_focus = NewRunPane::Agents,
            KeyCode::Tab => self.new_run_focus = NewRunPane::Task,
            KeyCode::Up | KeyCode::Down => {
                let delta = if key.code == KeyCode::Up { -1 } else { 1 };
                self.new_run_input_selected = list_cursor::move_cursor(
                    self.new_run_input_selected,
                    delta,
                    self.new_run_inputs.len(),
                );
            }
            _ => {
                let idx = self.new_run_input_selected;
                let Some(slot) = self.new_run_inputs.get_mut(idx) else {
                    return;
                };
                if slot.picks() {
                    match key.code {
                        KeyCode::Left => slot.cycle(false),
                        KeyCode::Right | KeyCode::Char(' ') => slot.cycle(true),
                        KeyCode::Enter if idx >= last => self.new_run_focus = NewRunPane::Task,
                        KeyCode::Enter => self.new_run_input_selected += 1,
                        _ => {}
                    }
                    return;
                }
                let takes_text = slot.takes_text();
                let takes_files = slot.takes_files();
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                // A file row opens the picker: on Enter or Space when it is
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
                slot.issue = None;
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

    /// A click on row `index`: the pane takes the keys and the row is the
    /// one under the cursor.
    pub(super) fn click_new_run_input(&mut self, index: usize) {
        if index < self.new_run_inputs.len() {
            self.new_run_focus = NewRunPane::Inputs;
            self.new_run_input_selected = index;
        }
    }

    /// Resolve every filled row into what the spawn sends. A row whose files
    /// cannot be read is an error naming it, so a typo is a toast here rather
    /// than a run that started without its picture.
    pub(super) fn new_run_input_values(&self) -> Result<ResolvedInputs, String> {
        let workdir: &Path = &self.new_run_ctx.workdir;
        let registry = cli_registry();
        let mut out = ResolvedInputs::default();
        for slot in &self.new_run_inputs {
            let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", slot.key);
            if slot.picks() {
                let picked = match (&slot.ty, slot.pick) {
                    (InputType::Bool, Some(i)) => Some(RawInput::Bool(i == 1)),
                    (InputType::Choice { options }, Some(i)) => {
                        options.get(i).map(|o| RawInput::Text(o.to_string()))
                    }
                    _ => None,
                };
                out.values.extend(picked.map(|v| (slot.key.clone(), v)));
                continue;
            }
            // The token cost of everything this row puts in its region, so a
            // choice that would not fit the region's budget is refused here
            // rather than at spawn.
            let mut slot_tokens = 0usize;
            let mut names = Vec::new();
            // Files chosen through the picker attach without an `@` or a
            // typed name: to a text input's region, or named by a file input.
            for rel in &slot.files {
                let full = workdir.join(rel);
                let part = crate::commands::run::attach::read_part(&rel.to_string_lossy(), workdir)
                    .map_err(|e| fail(&e))?;
                let part = match slot.is_file() {
                    true => part,
                    false => part.in_region(&slot.region),
                };
                names.push(RawInput::Text(part.name.clone()));
                out.parts.push(part);
                slot_tokens += super::new_run_picker::estimate_file_tokens(&full, &registry);
            }
            if slot.is_file() {
                let value = match (&slot.ty, names.len()) {
                    (InputType::List { .. }, _) => Some(RawInput::List(names)),
                    (_, 0) => None,
                    (_, _) => names.into_iter().next(),
                };
                out.values.extend(value.map(|v| (slot.key.clone(), v)));
                continue;
            }
            let raw = slot.edit.value().trim().to_string();
            if !raw.is_empty() && !matches!(slot.ty, InputType::Text { .. }) {
                let value = typed_value(&slot.ty, &raw).map_err(|e| fail(&e))?;
                out.values.insert(slot.key.clone(), value);
                continue;
            }
            if !raw.is_empty() {
                // A bare path that names a file is the file, as it is on the
                // command line's `--<name> @file`; a row is for one input, so
                // the `@` is implied.
                let value = match !raw.starts_with('@') && workdir.join(&raw).is_file() {
                    true => format!("@{raw}"),
                    false => raw,
                };
                let read = read_region_input(&slot.region, &value, workdir, &registry)
                    .map_err(|e| fail(&e))?;
                if !read.text.is_empty() {
                    slot_tokens += leviath_core::text::estimate_tokens(&read.text);
                    out.values
                        .insert(slot.key.clone(), RawInput::Text(read.text));
                }
                out.parts.extend(read.parts);
                out.unresolved.extend(read.unresolved);
            }
            if slot.max_tokens > 0 && slot_tokens > slot.max_tokens {
                return Err(format!(
                    "{}: what you chose needs about {} tokens, but region '{}' holds {}. Remove a file or choose a smaller one.",
                    slot.key, slot_tokens, slot.region, slot.max_tokens
                ));
            }
        }
        Ok(out)
    }

    /// Check what the rows resolved to against each input's declared type,
    /// before anything is sent, and put each problem beside its row. Returns
    /// how many rows have one.
    pub(super) fn check_new_run_inputs(&mut self, resolved: &ResolvedInputs) -> usize {
        let names: Vec<String> = resolved.parts.iter().map(|p| p.name.clone()).collect();
        let cx = CheckCtx {
            attachments: &names,
        };
        let mut issues = SpawnIssues::new();
        let at = SpecPath::root().field("inputs");
        for slot in &self.new_run_inputs {
            let path = at.key(&slot.key);
            match resolved.values.get(&slot.key) {
                Some(raw) => {
                    slot.ty.check(raw, &path, &cx, &mut issues);
                }
                // A text input whose files went to its region has been given
                // something, even with no text of its own.
                None if slot.required
                    && !resolved
                        .parts
                        .iter()
                        .any(|p| p.region.as_deref() == Some(slot.region.as_str())) =>
                {
                    issues.push(SpawnIssue::new(
                        path,
                        IssueCode::Missing,
                        "this input is required",
                    ))
                }
                None => {}
            }
        }
        self.show_new_run_issues(&issues)
    }

    /// Put each issue about one of the rows' inputs beside its row, clearing
    /// the rest, and select the first row with one. Returns how many rows
    /// have one.
    pub(super) fn show_new_run_issues(&mut self, issues: &SpawnIssues) -> usize {
        let mut first = None;
        for (i, slot) in self.new_run_inputs.iter_mut().enumerate() {
            slot.issue = issues
                .iter()
                .find(|issue| issue_input(&issue.path) == Some(slot.key.as_str()))
                .map(issue_text);
            if slot.issue.is_some() && first.is_none() {
                first = Some(i);
            }
        }
        if let Some(i) = first {
            self.new_run_focus = NewRunPane::Inputs;
            self.new_run_input_selected = i;
        }
        self.new_run_inputs
            .iter()
            .filter(|slot| slot.issue.is_some())
            .count()
    }

    /// The pane's height when it is drawn: a border and one row per input.
    pub(super) fn new_run_inputs_height(&self) -> u16 {
        match self.new_run_inputs.len() {
            0 => 0,
            n => (n as u16).saturating_add(2),
        }
    }

    /// Draw the rows into `area`, registering each for the mouse.
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
            let label = truncate(&label, label_w.saturating_sub(2));
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
            // The pane is sized to hold every row (`new_run_inputs_height`),
            // and it is not drawn at all when the column cannot give it that
            // many rows, so each row is always inside `inner`.
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

/// The input an issue is about, when its path is `inputs.<name>` or below it.
pub(super) fn issue_input(path: &SpecPath) -> Option<&str> {
    match path.0.as_slice() {
        [PathSeg::Field(inputs), PathSeg::Key(name), ..] if inputs == "inputs" => {
            Some(name.as_str())
        }
        _ => None,
    }
}

/// An issue as it reads beside its row: what is wrong, and what was expected.
fn issue_text(issue: &SpawnIssue) -> String {
    match &issue.expected {
        Some(expected) => format!("{}; expected {expected}", issue.message),
        None => issue.message.clone(),
    }
}

/// The effective context window of the blueprint's entry stage, resolved
/// offline: the **smallest** window across the stage's declared models, since a
/// region's percentage budget must fit the tightest of them. Each model
/// resolves through [`offline_model_window`]. Region percentage budgets resolve
/// against this, so the picker's token room matches the tightest a run will get.
fn entry_stage_window(
    blueprint: &leviath_runtime::spec::blueprint::Blueprint,
    config_path: &Path,
    cache_path: Option<&Path>,
) -> usize {
    const DEFAULT_WINDOW: usize = 8192;
    let entry = blueprint.resolve_entry_stage_name();
    let config = crate::config::Config::load_from_path_public(config_path).ok();
    let cache = cache_path.and_then(leviath_providers::CapabilityCache::load);
    // A missing entry stage and a stage that names no models both fold into an
    // empty iterator, so both take the `unwrap_or` default without a dead arm.
    blueprint
        .stages
        .iter()
        .find(|s| s.name == entry)
        .map(|s| &s.model.models)
        .into_iter()
        .flatten()
        .map(|m| offline_model_window(&m.provider, &m.model, config.as_ref(), cache.as_ref()))
        .min()
        .unwrap_or(DEFAULT_WINDOW)
}

/// One model's context window, resolved without a network call, preferring the
/// most authoritative source available:
/// 1. a `[model_capabilities]` override in config (under `provider/model` or the
///    bare `model` key),
/// 2. the shared capability cache the daemon wrote from live listings (this is
///    how an OpenRouter model absent from the compiled table gets its real
///    window offline),
/// 3. the compiled catalogue,
/// 4. the same 8192-token default the runtime falls back to.
fn offline_model_window(
    provider: &str,
    model: &str,
    config: Option<&crate::config::Config>,
    cache: Option<&leviath_providers::CapabilityCache>,
) -> usize {
    const DEFAULT_WINDOW: usize = 8192;
    if let Some(config) = config {
        let qualified = format!("{provider}/{model}");
        for key in [qualified.as_str(), model] {
            if let Some(window) = config
                .model_capabilities
                .get(key)
                .and_then(|o| o.max_context_tokens)
            {
                return window;
            }
        }
    }
    if let Some(window) = cache.and_then(|c| c.context_window(provider, model)) {
        return window;
    }
    crate::commands::models::builtin_model_windows()
        .get(&(provider.to_string(), model.to_string()))
        .copied()
        .unwrap_or(DEFAULT_WINDOW)
}

#[cfg(test)]
#[path = "new_run_inputs_tests.rs"]
mod tests;
