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
use super::editor::{Focus, InspectorHits, Overlay};
use super::inspector::{Field, FieldValue, Panel, StageTab, panel_title};
use crate::blueprint_edit::check::Severity;
use crate::tui::widgets::footer::{draw_hint_bar, hint};
use crate::tui::widgets::popup::{centered, popup_frame};

/// Under this many columns the panes take turns.
const SIDE_BY_SIDE_MIN_WIDTH: u16 = 110;
/// The inspector's width when both panes are on.
const INSPECTOR_WIDTH: u16 = 58;
/// Rows the expanded problems list takes.
const PROBLEMS_ROWS: u16 = 6;

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
        let focused = editor.focus == Focus::Inspector;
        let title = panel_title(&editor.panel);
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
        if let Panel::Stage { tab, .. } = &editor.panel {
            let mut spans = Vec::new();
            let mut x = inner.x;
            let mut tabs = Vec::new();
            for (i, t) in StageTab::ALL.iter().enumerate() {
                let on = t == tab;
                let text = format!(" {} {} ", i + 1, t.title());
                let w = text.chars().count() as u16;
                tabs.push((x, x + w));
                x += w;
                spans.push(Span::styled(
                    text,
                    if on {
                        Style::default()
                            .fg(C_ACTIVE)
                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                    } else {
                        Style::default().fg(C_DIM)
                    },
                ));
            }
            hits.tabs = Some((inner.y, tabs));
            lines.push(Line::from(spans));
            lines.push(Line::from(""));
        }
        if let Panel::External(name) = &editor.panel {
            lines.push(Line::from(Span::styled(
                format!("{name} is a separate agent; edit it from the catalog."),
                Style::default().fg(C_MUTED),
            )));
        }
        let fields = editor.fields();
        let label_w = 23usize;
        let value_w = (inner.width as usize).saturating_sub(label_w + 3);
        for (i, field) in fields.iter().enumerate() {
            hits.rows.push(inner.y + lines.len() as u16);
            let on = focused && i == editor.cursor;
            let editing = editor.line.as_ref().filter(|(id, _)| *id == field.id);
            let label_style = if !field.enabled {
                Style::default().fg(C_DIM)
            } else if on {
                Style::default().fg(C_ACTIVE).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(C_MUTED)
            };
            let mut spans = vec![Span::styled(
                if on { "› " } else { "  " },
                Style::default().fg(C_ACCENT),
            )];
            // A button is its label: the whole row is the action, so it has
            // no value column.
            let is_button = matches!(field.value, FieldValue::Button);
            if !is_button {
                spans.push(Span::styled(
                    format!("{:<label_w$}", field.label),
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
            lines.push(Line::from(spans));
        }
        let help = fields
            .get(editor.cursor)
            .filter(|_| focused)
            .map(|f| f.help.to_string())
            .unwrap_or_else(|| match editor.panel {
                Panel::Agent => {
                    "Select a stage or a path on the canvas to edit it; Tab moves here.".to_string()
                }
                _ => {
                    "Tab moves the keys here; ↑↓ pick a row, Enter edits it, ←→ change it in place."
                        .to_string()
                }
            });
        let body_h = inner.height.saturating_sub(3);
        // Rows under the help area are not drawn, so they are not clickable.
        hits.rows.retain(|y| *y < inner.y + body_h);
        editor.hit = hits;
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
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
        } else if editor.line.is_some() || editor.add_stage.is_some() || editor.add_region.is_some()
        {
            vec![
                hint("esc", "cancel"),
                hint("type", "edit"),
                hint("enter", "apply"),
            ]
        } else if matches!(editor.overlay, Some(Overlay::Prompts(_))) {
            vec![
                hint("esc / ^s", "apply"),
                hint("^q", "discard"),
                hint("tab", "other prompt"),
                hint("^e", "$EDITOR"),
                hint("?", "help"),
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
                Focus::Inspector => vec![
                    hint(
                        "esc",
                        if editor.panel_anchor.is_some() {
                            "back"
                        } else {
                            "canvas"
                        },
                    ),
                    hint("^s", "save"),
                    hint("?", "help"),
                    hint("↑↓", "row"),
                    hint("enter", "edit"),
                    hint("←→", "change"),
                    hint("x", "remove"),
                    hint("1-3", "tab"),
                    hint("tab", "canvas"),
                    hint("^z", "undo"),
                    hint("click", "pick a row"),
                ],
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
            format!("▸ {}", field.label),
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
