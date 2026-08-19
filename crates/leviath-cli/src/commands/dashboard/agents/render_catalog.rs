//! Drawing the Agents screen's catalog: the list on the left, the selected
//! agent on the right (graph, description, stages), the chooser over it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};

use super::super::state::Dashboard;
use super::super::theme::*;
use super::super::types::PaneId;
use crate::blueprint_edit::catalog::{CatalogEntry, Source};
use crate::tui::widgets::footer::{draw_hint_bar, hint};
use crate::tui::widgets::popup::{centered, popup_frame};

impl Dashboard {
    /// The whole Agents screen: catalog or editor, plus the chooser.
    pub(in crate::commands::dashboard) fn draw_agents_screen(
        &mut self,
        frame: &mut Frame,
        area: Rect,
    ) {
        self.agents().last_area = area;
        if self.agents().editor.is_some() {
            self.draw_editor(frame, area);
            return;
        }
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(rows[0]);
        self.agents().list_area = panes[0];
        self.draw_catalog_list(frame, panes[0]);
        self.draw_catalog_detail(frame, panes[1]);
        self.draw_catalog_hints(frame, rows[1]);
        if self.agents().chooser.is_some() {
            self.draw_chooser(frame, area);
        }
        self.draw_rename_prompt(frame, area);
    }

    /// The rename prompt, when `r` opened one: the name being typed, and
    /// why it will not do when it will not.
    fn draw_rename_prompt(&mut self, frame: &mut Frame, area: Rect) {
        let Some(rename) = self.agents().catalog.renaming.clone() else {
            return;
        };
        let popup = centered(50, 20, area);
        let popup = Rect {
            height: popup.height.clamp(4, 6),
            ..popup
        };
        let inner = popup_frame(
            frame,
            popup,
            &format!("Rename {}", rename.from),
            C_BORDER_FOCUS,
        );
        let mut name = vec![Span::styled("Name  ", Style::default().fg(C_DIM))];
        name.extend(rename.name.display_spans(true).spans);
        let problem = rename.problem.clone().unwrap_or_default();
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(name),
                Line::from(Span::styled(problem, Style::default().fg(C_WARN))),
                Line::from(Span::styled(
                    "The directory and the manifest's name change; its arrangement comes along.",
                    Style::default().fg(C_MUTED),
                )),
            ]),
            inner,
        );
    }

    fn draw_catalog_list(&mut self, frame: &mut Frame, area: Rect) {
        let catalog = &self.agents().catalog;
        let visible = catalog.visible();
        let header = Row::new(["Agent", "Source", "Description"].into_iter().map(|h| {
            Cell::from(Span::styled(
                h,
                Style::default().add_modifier(Modifier::BOLD),
            ))
        }));
        let rows: Vec<Row> = visible
            .iter()
            .filter_map(|i| catalog.entries.get(*i))
            .map(|e| {
                // An installed bundled agent that was edited says so in
                // place of "installed": that it is installed goes without
                // saying, and the column is narrow.
                let source = if e.differs_from_bundled {
                    "edited".to_string()
                } else {
                    e.source.as_str().to_string()
                };
                Row::new(vec![
                    Cell::from(e.name.clone()),
                    Cell::from(Span::styled(source, Style::default().fg(source_colour(e)))),
                    Cell::from(Span::styled(
                        e.description.clone(),
                        Style::default().fg(C_DIM),
                    )),
                ])
            })
            .collect();
        let title = if catalog.filter.is_empty() && !catalog.filtering {
            format!(" Agents ({}) ", visible.len())
        } else {
            format!(
                " Agents  /{}{}  {}/{} ",
                catalog.filter,
                if catalog.filtering { "▌" } else { "" },
                visible.len(),
                catalog.entries.len()
            )
        };
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(30),
                Constraint::Percentage(24),
                Constraint::Percentage(46),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(C_BORDER_FOCUS))
                .title(title),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
        let mut state = TableState::default();
        if !visible.is_empty() {
            state.select(Some(catalog.selected.min(visible.len() - 1)));
        }
        let empty = visible.is_empty();
        let filter = catalog.filter.clone();
        frame.render_stateful_widget(table, area, &mut state);
        if empty {
            // The bundled agents are always in the catalog, so an empty list
            // is always a filter that matched nothing.
            let text = format!("No agents match \"{filter}\".");
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(text, Style::default().fg(C_DIM)))),
                Rect {
                    x: area.x + 2,
                    y: area.y + 2,
                    width: area.width.saturating_sub(4),
                    height: 1,
                },
            );
        }
    }

    /// The right half: the graph on top, then what the agent is and its
    /// stages.
    fn draw_catalog_detail(&mut self, frame: &mut Frame, area: Rect) {
        self.agents().catalog.sync_preview();
        let Some(entry) = self.agents().catalog.selected_entry().cloned() else {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    " Pick an agent to see it.",
                    Style::default().fg(C_DIM),
                )))
                .block(detail_block(" Agent ")),
                area,
            );
            return;
        };
        let graph_h = (area.height * 55 / 100)
            .max(8)
            .min(area.height.saturating_sub(6));
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(graph_h), Constraint::Min(1)])
            .split(area);
        let title = format!(
            " {} · v{} · {} stage{} ",
            entry.name,
            entry.version,
            entry.stages.len(),
            if entry.stages.len() == 1 { "" } else { "s" }
        );
        let (_, preview) = self
            .agents()
            .catalog
            .preview
            .as_mut()
            .expect("synced for the selected entry just above");
        match preview {
            Ok(view) => {
                let canvas = view.render(frame, split[0], detail_block(&title));
                self.pane_rects.push((PaneId::AgentsPreview, canvas));
            }
            Err(why) => {
                let why = why.clone();
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        format!(" {why}"),
                        Style::default().fg(C_WARN),
                    )))
                    .wrap(Wrap { trim: false })
                    .block(detail_block(&title)),
                    split[0],
                );
            }
        }
        let mut lines: Vec<Line> = vec![
            Line::from(Span::styled(
                entry.description.clone(),
                Style::default().fg(C_WHITE),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("where  ", Style::default().fg(C_DIM)),
                Span::raw(
                    entry
                        .dir
                        .as_ref()
                        .map(|d| d.display().to_string())
                        .unwrap_or_else(|| {
                            "bundled in this binary; saving installs it".to_string()
                        }),
                ),
            ]),
            Line::from(vec![
                Span::styled("stages ", Style::default().fg(C_DIM)),
                Span::raw(entry.stages.join(" → ")),
            ]),
        ];
        if entry.differs_from_bundled {
            lines.push(Line::from(Span::styled(
                "Edited since it was installed: `r` puts the bundled copy back.",
                Style::default().fg(C_WARN),
            )));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(detail_block(" About ")),
            split[1],
        );
    }

    fn draw_catalog_hints(&self, frame: &mut Frame, area: Rect) {
        let screen = self.agent_builder.as_deref().expect("callers check");
        // Priority order: on a narrow terminal the tail falls off, and `?`
        // always has the full list.
        let hints = if screen.catalog.renaming.is_some() {
            vec![
                hint("esc", "keep the name"),
                hint("type", "the new name"),
                hint("enter", "rename"),
            ]
        } else if screen.chooser.is_some() {
            vec![
                hint("esc", "back"),
                hint("↑↓", "template"),
                hint("type", "the name"),
                hint("enter", "open the editor"),
                hint("?", "help"),
            ]
        } else if screen.catalog.filtering {
            vec![
                hint("esc", "clear"),
                hint("type", "filter"),
                hint("enter", "keep"),
            ]
        } else {
            vec![
                hint("esc", "back"),
                hint("↑↓", "select"),
                hint("enter", "edit"),
                hint("n", "new"),
                hint("l", "launch"),
                hint("?", "help"),
                hint("r", "rename"),
                hint("d", "delete"),
                hint("R", "reset"),
                hint("/", "filter"),
                hint("wheel", "scroll"),
            ]
        };
        draw_hint_bar(frame, area, None, &hints, false);
    }

    /// The template chooser: a list, and the name under it.
    fn draw_chooser(&mut self, frame: &mut Frame, area: Rect) {
        let chooser = self.agents().chooser.as_ref().expect("callers check");
        let popup = centered(70, 70, area);
        let inner = popup_frame(frame, popup, "New agent", C_BORDER_FOCUS);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(3),
                Constraint::Length(4),
            ])
            .split(inner);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "Start from the two-stage starter, or from a copy of an agent you have.",
                    Style::default().fg(C_MUTED),
                )),
                Line::from(""),
            ])
            .wrap(Wrap { trim: false }),
            chunks[0],
        );
        let height = chunks[1].height as usize;
        let offset = chooser.cursor.saturating_sub(height.saturating_sub(1));
        let rows: Vec<Line> = chooser
            .templates
            .iter()
            .enumerate()
            .skip(offset)
            .take(height)
            .map(|(i, t)| {
                let selected = i == chooser.cursor;
                Line::from(vec![
                    Span::styled(
                        if selected { "› " } else { "  " },
                        Style::default().fg(C_ACCENT),
                    ),
                    Span::styled(
                        format!("{:<26}", t.label),
                        if selected {
                            Style::default().fg(C_ACTIVE).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(C_WHITE)
                        },
                    ),
                    Span::styled(t.detail.clone(), Style::default().fg(C_DIM)),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(rows), chunks[1]);
        let mut name = vec![Span::styled("Name  ", Style::default().fg(C_DIM))];
        name.extend(chooser.name.display_spans(true).spans);
        let problem = chooser.problem.clone().unwrap_or_default();
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(name),
                Line::from(Span::styled(problem, Style::default().fg(C_WARN))),
                Line::from(Span::styled(
                    "↑↓ template · type the name · Enter open in the editor · Esc back",
                    Style::default().fg(C_MUTED),
                )),
            ]),
            chunks[2],
        );
    }
}

fn detail_block(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(C_BORDER))
        .title(title.to_string())
}

/// Colour an agent's source: bundled-but-not-installed is the one that
/// needs a step before it runs; edited stands out too.
fn source_colour(entry: &CatalogEntry) -> Color {
    match entry.source {
        Source::Bundled => C_WARN,
        _ if entry.differs_from_bundled => C_WARN,
        _ => C_ACCENT,
    }
}
