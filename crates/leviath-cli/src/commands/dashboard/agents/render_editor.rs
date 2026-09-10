//! Drawing the editor: the canvas on the left, the inspector on the right,
//! the problems line under the canvas, a hint bar, and the overlays.
//!
//! On a narrow terminal the two panes take turns: the one with the keys
//! fills the width and `Tab` swaps.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use super::super::state::Dashboard;
use super::super::theme::*;
use super::super::types::PaneId;
use super::editor::{Editor, Focus, InspectorHits, ModelDrag, Overlay};
use super::inspector::{Field, FieldId, FieldValue, Panel, StageTab, panel_title};
use crate::blueprint_edit::check::Severity;
use crate::tui::widgets::footer::{draw_hint_bar, hint};
use crate::tui::widgets::markdown_edit::{MODE_CHORD, MdAction, chord_label};
use crate::tui::widgets::popup::{centered, popup_frame};

/// Under this many columns the panes take turns.
const SIDE_BY_SIDE_MIN_WIDTH: u16 = 120;
/// The inspector's width when both panes are on.
const INSPECTOR_WIDTH: u16 = 74;
/// Rows the expanded problems list takes.
const PROBLEMS_ROWS: u16 = 6;
/// The grip drawn beside a row the mouse can pick up and drag, with the space
/// that separates it from the label. Braille dots: the widest-supported glyph
/// that reads as "handle" rather than as content.
const GRIP: &str = "⠿ ";
/// Cells [`GRIP`] occupies, and so the width of the column reserved for it on
/// every row - a grip that shifted its own row two columns right would be a
/// worse cue than one that lines up with the blanks above it.
const GRIP_W: u16 = 2;

impl Dashboard {
    /// The editor screen.
    pub(super) fn draw_editor(&mut self, frame: &mut Frame, area: Rect) {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);
        self.draw_editor_title(frame, rows[0]);
        let focus = self.editor().focus;
        let narrow = area.width < SIDE_BY_SIDE_MIN_WIDTH;
        let (canvas_area, inspector_area) = if narrow {
            match focus {
                Focus::Canvas => (Some(rows[1]), None),
                Focus::Inspector => (None, Some(rows[1])),
            }
        } else {
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(1), Constraint::Length(INSPECTOR_WIDTH)])
                .split(rows[1]);
            (Some(panes[0]), Some(panes[1]))
        };
        if let Some(area) = canvas_area {
            self.draw_editor_canvas(frame, area);
        }
        if let Some(area) = inspector_area {
            self.draw_editor_inspector(frame, area);
        }
        self.draw_editor_hints(frame, rows[2]);
        // The window sits over the editor and under the chooser or the line
        // popup a row of it may open.
        self.draw_editor_modal(frame, rows[1]);
        self.draw_editor_overlays(frame, area);
    }

    fn draw_editor_title(&mut self, frame: &mut Frame, area: Rect) {
        let editor = self.editor();
        let mut spans = vec![
            Span::styled(" Agent editor · ", Style::default().fg(C_DIM)),
            Span::styled(
                editor.name.clone(),
                Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
            ),
        ];
        if editor.dirty {
            spans.push(Span::styled("*", Style::default().fg(C_WARN)));
        }
        if editor.is_new {
            spans.push(Span::styled(
                "  (not saved yet)",
                Style::default().fg(C_DIM),
            ));
        }
        if let Some(message) = &editor.message {
            spans.push(Span::styled(
                format!("   {message}"),
                Style::default().fg(C_MUTED),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// The canvas, with the problems line (or list) under it.
    fn draw_editor_canvas(&mut self, frame: &mut Frame, area: Rect) {
        let open = self.editor().problems_open;
        let problems_h = if open {
            PROBLEMS_ROWS.min(area.height / 2)
        } else {
            1
        };
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(problems_h)])
            .split(area);
        let focused = self.editor().focus == Focus::Canvas;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if focused { C_BORDER_FOCUS } else { C_BORDER }))
            .title(" Graph · a add · c connect · x delete · drag a ● to connect ");
        let canvas = self.editor().view.render(frame, split[0], block);
        self.pane_rects.push((PaneId::AgentEditorGraph, canvas));
        self.draw_editor_problems(frame, split[1]);
    }

    fn draw_editor_problems(&mut self, frame: &mut Frame, area: Rect) {
        let editor = self.editor();
        let problems = &editor.problems;
        let errors = problems.error_count();
        let warnings = problems.warning_count();
        let summary = if problems.items.is_empty() {
            Span::styled(" ✓ no problems", Style::default().fg(C_SUCCESS))
        } else {
            let colour = if errors > 0 { C_ERROR } else { C_WARN };
            let first = problems
                .first()
                .map(|p| match &p.stage {
                    Some(stage) => format!("{stage}: {}", p.message),
                    None => p.message.clone(),
                })
                .unwrap_or_default();
            Span::styled(
                format!(
                    " ! {errors} error{} · {warnings} warning{} · {first}",
                    if errors == 1 { "" } else { "s" },
                    if warnings == 1 { "" } else { "s" }
                ),
                Style::default().fg(colour),
            )
        };
        if !editor.problems_open || area.height <= 1 {
            frame.render_widget(Paragraph::new(Line::from(summary)), area);
            return;
        }
        let mut lines = vec![Line::from(summary)];
        for p in problems.items.iter().take(area.height as usize - 1) {
            let colour = match p.severity {
                Severity::Error => C_ERROR,
                Severity::Warning => C_WARN,
                Severity::Note => C_DIM,
            };
            let mut text = format!("   {} ", p.severity.tag());
            if let Some(stage) = &p.stage {
                text.push_str(&format!("{stage}: "));
            }
            text.push_str(&p.message);
            if let Some(fix) = &p.fix {
                text.push_str(&format!("  ({fix})"));
            }
            lines.push(Line::from(Span::styled(text, Style::default().fg(colour))));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// The inspector: the panel's title, its tabs when a stage, its rows,
    /// and the focused row's help at the bottom.
    fn draw_editor_inspector(&mut self, frame: &mut Frame, area: Rect) {
        let editor = self.editor();
        // Under a window the inspector shows the panel the window was
        // opened over, with the keys elsewhere.
        let (panel, cursor, focused) = match &editor.modal {
            Some(base) => (base.panel.clone(), base.cursor, false),
            None => (
                editor.panel.clone(),
                editor.cursor,
                editor.focus == Focus::Inspector,
            ),
        };
        let title = panel_title(&panel);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if focused { C_BORDER_FOCUS } else { C_BORDER }))
            .title(format!(" {title} "));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut lines: Vec<Line> = Vec::new();
        let mut hits = InspectorHits {
            area,
            ..InspectorHits::default()
        };
        if let Panel::Stage { tab, .. } = &panel {
            let mut spans = Vec::new();
            let mut x = inner.x;
            let mut tabs = Vec::new();
            for (i, t, text) in tab_strip(inner.width) {
                let w = text.chars().count() as u16;
                tabs.push((x, x + w));
                x += w;
                spans.push(Span::styled(
                    text,
                    if t == *tab {
                        Style::default()
                            .fg(C_ACTIVE)
                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                    } else {
                        Style::default().fg(C_DIM)
                    },
                ));
                let _ = i;
            }
            hits.tabs = Some((inner.y, tabs));
            lines.push(Line::from(spans));
            lines.push(Line::from(""));
        }
        if let Panel::External(name) = &panel {
            lines.push(Line::from(Span::styled(
                format!("{name} is a separate agent; edit it from the catalog."),
                Style::default().fg(C_MUTED),
            )));
        }
        let fields = drag_order(
            super::inspector::fields(&editor.doc, &panel),
            editor.model_drag,
        );
        let painted = field_lines(
            editor,
            &fields,
            cursor,
            focused,
            inner,
            inner.y + lines.len() as u16,
            true,
        );
        lines.extend(painted.lines);
        hits.rows = painted.rows;
        hits.grips = painted.grips;
        let help = fields
            .get(cursor)
            .filter(|_| focused)
            .map(|f| f.help.to_string())
            .unwrap_or_else(|| match panel {
                Panel::Agent => {
                    "Select a stage or a path on the canvas to edit it; Tab moves here.".to_string()
                }
                Panel::Stage { .. } => {
                    "↑↓ pick a row, Enter edits it, ←→ change it in place; Tab and Shift-Tab switch tabs, Esc goes back to the graph."
                        .to_string()
                }
                _ => {
                    "↑↓ pick a row, Enter edits it, ←→ change it in place; Tab or Esc goes back to the graph."
                        .to_string()
                }
            });
        let body_h = inner.height.saturating_sub(3);
        // Rows under the help area are not drawn, so they are not clickable.
        hits.rows.retain(|y| *y < inner.y + body_h);
        hits.grips.retain(|(_, r)| r.y < inner.y + body_h);
        editor.hit = hits;
        // Never wrapped: every line was cut to the width when it was made,
        // and a wrapped line would put every row under it one line below
        // where the click map says it is.
        frame.render_widget(
            Paragraph::new(lines),
            Rect {
                height: body_h,
                ..inner
            },
        );
        let help_area = Rect {
            y: inner.y + inner.height.saturating_sub(3),
            height: inner.height.min(3),
            ..inner
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(help, Style::default().fg(C_DIM))))
                .wrap(Wrap { trim: true }),
            help_area,
        );
    }

    /// The window a region, a declared file or a loop's path is edited in:
    /// the same rows the inspector would show, over the editor, with the
    /// keys until Esc closes it.
    fn draw_editor_modal(&mut self, frame: &mut Frame, area: Rect) {
        let editor = self.editor();
        if editor.modal.is_none() {
            return;
        }
        let popup = centered(64, 76, area);
        let popup = Rect {
            width: popup.width.max(area.width.min(60)),
            height: popup.height.max(area.height.min(14)),
            ..popup
        };
        let inner = popup_frame(frame, popup, &panel_title(&editor.panel), C_BORDER_FOCUS);
        let fields = editor.fields();
        let painted = field_lines(editor, &fields, editor.cursor, true, inner, inner.y, false);
        let help = fields
            .get(editor.cursor)
            .map(|f| f.help.to_string())
            .unwrap_or_default();
        let body_h = inner.height.saturating_sub(3);
        let mut hits = InspectorHits {
            area: popup,
            rows: painted.rows,
            ..InspectorHits::default()
        };
        hits.rows.retain(|y| *y < inner.y + body_h);
        editor.modal_hit = hits;
        frame.render_widget(
            Paragraph::new(painted.lines),
            Rect {
                height: body_h,
                ..inner
            },
        );
        let help_area = Rect {
            y: inner.y + inner.height.saturating_sub(3),
            height: inner.height.min(3),
            ..inner
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{help}  Esc closes this window."),
                Style::default().fg(C_DIM),
            )))
            .wrap(Wrap { trim: true }),
            help_area,
        );
    }

    fn draw_editor_hints(&mut self, frame: &mut Frame, area: Rect) {
        let editor = self.editor();
        // Priority order: on a narrow terminal the tail falls off, and `?`
        // always has the full list.
        let hints = if editor.menu.is_some() {
            vec![
                hint("esc", "close"),
                hint("↑↓", "move"),
                hint("enter", "do it"),
                hint("click", "pick / close"),
            ]
        } else if let Some((_, picker)) = &editor.picker {
            let mut hints = vec![
                hint("esc", "cancel"),
                hint("type", "search"),
                hint("↑↓", "move"),
            ];
            if picker.multi.is_some() {
                hints.push(hint("space", "pick / drop"));
                hints.push(hint("enter", "keep"));
            } else {
                hints.push(hint("enter", "choose"));
            }
            hints
        } else if editor.line.is_some()
            || editor.add_stage.is_some()
            || editor.add_region.is_some()
            || editor.add_artifact.is_some()
        {
            vec![
                hint("esc", "cancel"),
                hint("type", "edit"),
                hint("enter", "apply"),
            ]
        } else if editor.modal.is_some() {
            vec![
                hint("esc", "close"),
                hint("^s", "save"),
                hint("↑↓", "row"),
                hint("enter", "edit"),
                hint("←→", "change"),
                hint("x", "remove"),
                hint("^z", "undo"),
                hint("click", "pick a row"),
            ]
        } else if matches!(editor.overlay, Some(Overlay::Prompts(_))) {
            vec![
                hint("esc / ^s", "apply"),
                hint("^q", "discard"),
                hint("tab", "other prompt"),
                hint("F2", "$EDITOR"),
                hint(chord_label(MdAction::Bold), "bold"),
                hint(MODE_CHORD, "preview"),
                hint("toolbar", "click to format"),
                hint("F1", "every chord"),
            ]
        } else if editor.overlay.is_some() {
            vec![
                hint("esc", "close"),
                hint("↑↓", "scroll"),
                hint("y", "copy"),
            ]
        } else {
            match editor.focus {
                Focus::Canvas => vec![
                    hint("esc", "close"),
                    hint("^s", "save"),
                    hint("?", "help"),
                    hint("tab", "inspector"),
                    hint("←→↑↓", "select"),
                    hint("enter", "edit"),
                    hint("right-click", "menu"),
                    hint("a", "add stage"),
                    hint("c", "connect"),
                    hint("x", "delete"),
                    hint("^z", "undo"),
                    hint("^y", "redo"),
                    hint("v", "definition"),
                    hint("p", "problems"),
                    hint("r", "rotate"),
                    hint("f", "fit"),
                    hint("+ -", "zoom"),
                    hint("drag", "move / connect"),
                ],
                Focus::Inspector => {
                    let mut hints = vec![
                        hint("esc", "canvas"),
                        hint("^s", "save"),
                        hint("?", "help"),
                        hint("↑↓", "row"),
                        hint("enter", "edit"),
                    ];
                    hints.push(hint("←→", "change"));
                    // On a stage Tab walks the tabs; anywhere else there are
                    // none to walk and it goes back to the canvas.
                    if editor.panel_tab().is_some() {
                        hints.push(hint("tab ⇧tab 1-4", "tab"));
                    } else {
                        hints.push(hint("tab", "canvas"));
                    }
                    hints.extend([
                        hint("x", "remove"),
                        hint("^z", "undo"),
                        hint("click", "pick a row"),
                        hint("drag ⠿", "reorder"),
                    ]);
                    hints
                }
            }
        };
        draw_hint_bar(frame, area, None, &hints, false);
    }

    /// The picker, the name prompts, the prompts overlay, and the
    /// definition, on top.
    fn draw_editor_overlays(&mut self, frame: &mut Frame, area: Rect) {
        if self.draw_editor_prompts(frame, area) {
            return;
        }
        if let Some(menu) = self.editor().menu.as_mut() {
            menu.draw(frame, area);
            return;
        }
        let editor = self.editor();
        if let Some((_, picker)) = &editor.picker {
            picker.draw(frame, area);
            return;
        }
        if self.draw_editor_name_popups(frame, area) {
            return;
        }
        let editor = self.editor();
        if let Some(Overlay::Definition { scroll }) = &editor.overlay {
            let text = editor.doc.to_toml();
            let popup = centered(90, 90, area);
            let inner = popup_frame(
                frame,
                popup,
                "Definition · the file this editor will save",
                C_BORDER_FOCUS,
            );
            let lines: Vec<Line> = text.lines().map(|l| Line::from(l.to_string())).collect();
            let max_scroll = lines.len().saturating_sub(inner.height as usize);
            let scroll = (*scroll).min(max_scroll);
            frame.render_widget(Clear, inner);
            frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), inner);
        }
    }
}

/// The rows of a panel as drawn: their lines, and where each landed.
struct PaintedRows {
    lines: Vec<Line<'static>>,
    /// The screen row of each field, in field order.
    rows: Vec<u16>,
    /// The drag grip beside each model entry.
    grips: Vec<(usize, Rect)>,
}

/// Paint `fields` one row each from `start_y`, the cursor's row lit when
/// `focused`, a line editor drawn in place on the row being typed into.
/// The inspector and the window share this, so a region reads the same in
/// both.
fn field_lines(
    editor: &Editor,
    fields: &[Field],
    cursor: usize,
    focused: bool,
    inner: Rect,
    start_y: u16,
    with_grips: bool,
) -> PaintedRows {
    let label_w = label_width(fields);
    // Two columns wider than the label and its gutter: the grip sits in
    // the gap, blank on rows nothing can be done with.
    let value_w = (inner.width as usize).saturating_sub(label_w + GRIP_W as usize + 3);
    let mut painted = PaintedRows {
        lines: Vec::new(),
        rows: Vec::new(),
        grips: Vec::new(),
    };
    for (i, field) in fields.iter().enumerate() {
        let row_y = start_y + i as u16;
        painted.rows.push(row_y);
        if with_grips && let FieldId::ModelEntry(m) = field.id {
            painted.grips.push((
                m,
                Rect {
                    x: inner.x + 2,
                    y: row_y,
                    width: GRIP_W,
                    height: 1,
                },
            ));
        }
        let on = focused && i == cursor;
        let editing = editor
            .line
            .as_ref()
            .filter(|(id, _)| focused && *id == field.id);
        // Dim only when the row cannot be edited right now: a value left at
        // its default is dimmed by `value_text`, and a label that dimmed with
        // it read as a row that could not be touched.
        let label_style = if !field.enabled {
            Style::default().fg(C_DIM)
        } else if on {
            Style::default().fg(C_ACTIVE).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let grabbable = with_grips && matches!(field.id, FieldId::ModelEntry(_));
        let mut spans = vec![
            Span::styled(if on { "› " } else { "  " }, Style::default().fg(C_ACCENT)),
            Span::styled(
                if grabbable { GRIP } else { "  " },
                Style::default().fg(if on { C_ACCENT } else { C_DIM }),
            ),
        ];
        // A button is its label: the whole row is the action, so it has
        // no value column.
        let is_button = matches!(field.value, FieldValue::Button);
        if !is_button {
            spans.push(Span::styled(
                format!("{:<label_w$}", fit(&field.label, label_w)),
                label_style,
            ));
        }
        match editing {
            Some((_, line)) => spans.extend(line.display_spans(true).spans),
            None => {
                let (text, style) = value_text(field, on);
                let room = if is_button {
                    value_w + label_w
                } else {
                    value_w
                };
                spans.push(Span::styled(fit(&text, room), style));
            }
        }
        painted.lines.push(Line::from(spans));
    }
    painted
}

/// The stage tabs as one line that fits `width`: the full titles when they
/// do, the short ones otherwise, each with its number and the tab it is.
///
/// The strip used to be the full titles whatever the width, and at the
/// inspector's usual width it wrapped onto a second line, which cut the
/// last tab in two and put every row one line below where the click map
/// had it.
fn tab_strip(width: u16) -> Vec<(usize, StageTab, String)> {
    let strip = |short: bool| -> Vec<(usize, StageTab, String)> {
        StageTab::ALL
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let title = if short { t.short_title() } else { t.title() };
                (i, *t, format!(" {} {title} ", i + 1))
            })
            .collect()
    };
    let full = strip(false);
    let fits = full
        .iter()
        .map(|(_, _, text)| text.chars().count())
        .sum::<usize>()
        <= width as usize;
    if fits { full } else { strip(true) }
}

/// The label column's width for these rows: the widest label plus a gap,
/// within bounds, so a long label is cut rather than pushing its value into
/// the next line.
fn label_width(fields: &[Field]) -> usize {
    fields
        .iter()
        .filter(|f| !matches!(f.value, FieldValue::Button))
        .map(|f| f.label.chars().count() + 2)
        .max()
        .unwrap_or(12)
        .clamp(12, 26)
}

/// The fields as they should be drawn while a model is in the air: the chain's
/// values permuted into the order a release would commit.
///
/// The *values* move and the rows stay, so the labels still read "Model" then
/// "then" down the list - the first row is whatever the chain would start
/// with, which is the question the labels are answering. It also keeps the
/// row under the pointer the row that will hold the entry, so the drop is
/// what it looked like rather than an inference from a caret.
///
/// A drag whose indices no longer fit the chain draws untouched rather than
/// panicking: the fields are rebuilt from the document every frame, and the
/// document can change under a held button (an undo, a reload).
fn drag_order(mut fields: Vec<Field>, drag: Option<ModelDrag>) -> Vec<Field> {
    let Some(drag) = drag else {
        return fields;
    };
    let rows: Vec<usize> = fields
        .iter()
        .enumerate()
        .filter(|(_, f)| matches!(f.id, FieldId::ModelEntry(_)))
        .map(|(i, _)| i)
        .collect();
    if drag.from >= rows.len() || drag.to >= rows.len() {
        return fields;
    }
    let mut values: Vec<FieldValue> = rows.iter().map(|&i| fields[i].value.clone()).collect();
    let held = values.remove(drag.from);
    values.insert(drag.to, held);
    for (row, value) in rows.into_iter().zip(values) {
        fields[row].value = value;
    }
    fields
}

/// How a field's value reads on its row (a button reads as its label).
fn value_text(field: &Field, on: bool) -> (String, Style) {
    let (value, enabled) = (&field.value, field.enabled);
    let base = if !enabled {
        Style::default().fg(C_DIM)
    } else if on {
        Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(C_WHITE)
    };
    match value {
        FieldValue::Text(t) if t.is_empty() => ("(empty)".to_string(), Style::default().fg(C_DIM)),
        FieldValue::Text(t) => (t.clone(), base),
        FieldValue::Number(None) => ("(default)".to_string(), Style::default().fg(C_DIM)),
        FieldValue::Number(Some(n)) => (n.to_string(), base),
        // A toggle that is on is never a disabled one (the two toggles are
        // always live), so it always shows in the success colour.
        FieldValue::Toggle(true) => ("[x] on".to_string(), base.fg(C_SUCCESS)),
        FieldValue::Toggle(false) => ("[ ] off".to_string(), base),
        FieldValue::Choice(c) => (format!("‹ {c} ›"), base),
        FieldValue::Row(r) => (r.clone(), base),
        FieldValue::Segment { options, index } => (
            options
                .iter()
                .enumerate()
                .map(|(i, o)| {
                    if Some(i) == *index {
                        format!("[{o}]")
                    } else {
                        format!(" {o} ")
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
            base,
        ),
        FieldValue::Button => (
            field.label.clone(),
            if !enabled {
                Style::default().fg(C_DIM)
            } else if on {
                Style::default().fg(C_ACTIVE).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(C_MUTED)
            },
        ),
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
