//! Content pane rendering: output, logs, context view, search highlighting.

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use crate::commands::dashboard::helpers::format_tokens;
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;
use crate::commands::dashboard::types::*;
use crate::runstate;
use crate::tui::widgets::markdown_edit::MdEditView;

/// Replace a leading `home` prefix in `raw` with `~`. Split out so both the
/// shortened and the raw (path-is-outside-home) branches are unit-testable on
/// every platform: reaching the raw branch through the real render path needs a
/// runs dir outside `$HOME`, which isn't portable (`std::env::temp_dir()` lives
/// *under* the home directory on Windows, so it only ever hits the shortened
/// branch there).
fn shorten_home_path(raw: String, home: &str) -> String {
    // `strip_prefix` rather than `starts_with` plus `&raw[home.len()..]`: it does
    // the test and the cut as one operation, so they cannot disagree about where
    // the prefix ended. The empty-home guard stays because `strip_prefix("")`
    // matches everything.
    match raw.strip_prefix(home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => raw,
    }
}

impl Dashboard {
    pub(in crate::commands::dashboard) fn render_context_bar(
        &self,
        frame: &mut Frame,
        ctx_area: Rect,
        agent: &DashboardAgent,
    ) {
        // When browsing archived history, show that point; else the live window.
        let snap_opt = self
            .browsed_context_point()
            .map(|p| p.context.clone())
            .or_else(|| runstate::read_stage_context(&agent.id, self.selected_stage))
            .or_else(|| agent.context_snapshot.as_deref().cloned());

        // The card title shows the browsed history position - which point, of
        // how many, in which stage, recorded when - or a plain " ctx ".
        let title = match (self.context_history_idx, self.selected_history()) {
            (Some(i), Some(h)) => {
                let total = h.points.len();
                match h.points.get(i) {
                    Some(p) => {
                        let when = chrono::DateTime::from_timestamp(p.at, 0)
                            .map(|t| {
                                t.with_timezone(&chrono::Local)
                                    .format("%H:%M:%S")
                                    .to_string()
                            })
                            .unwrap_or_default();
                        format!(
                            " ⏪ point {}/{} · {} · {} ",
                            i + 1,
                            total,
                            p.meta.current_stage,
                            when
                        )
                    }
                    None => format!(" ⏪ point {}/{} ", i + 1, total),
                }
            }
            _ => " ctx ".to_string(),
        };

        // Constrain context card to at most 60 cols, left-aligned
        let card_w = ctx_area.width.min(64);
        let card_area = Rect {
            width: card_w,
            ..ctx_area
        };

        if let Some(snap) = snap_opt {
            let total_pct = (snap.total_tokens * 100)
                .checked_div(snap.max_tokens)
                .unwrap_or(0)
                .min(100);
            let bar_color = if total_pct >= 90 {
                C_ERROR
            } else if total_pct >= 70 {
                C_WARN
            } else {
                C_SUCCESS
            };

            let inner_w = (card_w as usize).saturating_sub(4).max(8);
            let bar_w = inner_w.min(32);
            let filled = bar_w * total_pct / 100;
            let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_w - filled));

            let regions_str: String = snap
                .regions
                .iter()
                .take(6)
                // Both spellings of each: the blueprint's word, which is
                // what a snapshot writes now, and the short one older
                // snapshots on this disk still carry.
                .map(|r| match r.kind.as_str() {
                    "pinned" => "P",
                    "sliding_window" | "sliding" => "S",
                    "compacting" | "compact_history" | "history" => "H",
                    _ => "·",
                })
                .collect::<Vec<_>>()
                .join(" ");

            let bar_line = Line::from(vec![
                Span::styled(bar, Style::default().fg(bar_color)),
                Span::styled(
                    format!("  {}%", total_pct),
                    Style::default().fg(C_DIM).add_modifier(Modifier::BOLD),
                ),
            ]);
            let info_line = Line::from(vec![
                Span::styled(
                    format!(
                        "{} / {} tokens",
                        format_tokens(snap.total_tokens),
                        format_tokens(snap.max_tokens)
                    ),
                    Style::default().fg(C_MUTED),
                ),
                Span::styled(
                    if regions_str.is_empty() {
                        String::new()
                    } else {
                        format!("   [{}]", regions_str)
                    },
                    Style::default().fg(C_DIM),
                ),
            ]);

            frame.render_widget(
                Paragraph::new(vec![bar_line, info_line]).block(
                    Block::default()
                        .title(Span::styled(title.clone(), Style::default().fg(C_DIM)))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(C_BORDER)),
                ),
                card_area,
            );
        } else {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "no context snapshot yet",
                    Style::default().fg(C_DIM),
                )))
                .block(
                    Block::default()
                        .title(Span::styled(title.clone(), Style::default().fg(C_DIM)))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(C_BORDER)),
                ),
                card_area,
            );
        }
    }

    pub(in crate::commands::dashboard) fn render_content_pane(
        &mut self,
        frame: &mut Frame,
        content_area: Rect,
        agent: &DashboardAgent,
        _area_width: u16,
    ) {
        // Editing a document (an `EditText` interaction) takes over the content
        // pane: the editable textarea is rendered here, over the current text,
        // instead of the read-only stage output - so the user revises the plan
        // in place rather than in the bottom input bar.
        if self.editing_document() {
            let view = MdEditView::new(
                " ✎ Editing this document - your changes replace it  ·  [^Enter] save  \
                 [Enter] newline  [Tab] Save button  [Esc] cancel ",
                C_SUCCESS,
                !self.response_focus_send,
            );
            let (editor_area, button_row) = super::button::editor_and_button_rows(content_area);
            self.input_textarea.render(frame, editor_area, &view);
            self.draw_action_button(
                frame,
                button_row,
                super::input::SAVE_BUTTON,
                self.response_focus_send,
                ClickTarget::ResponseSend,
            );
            return;
        }

        // The run's submitted answer, read through the same function the
        // HTTP API serves it from, so the Final view and
        // `GET /api/agents/{id}/result` cannot show different text. Read once
        // per frame: it decides whether the `[f] final` chip is offered and
        // what that view shows.
        let final_output = runstate::read_final_output(&agent.id);
        // The Final view of a run without an answer (the selection moved to
        // another run) falls back to Output rather than sitting on an empty
        // pane whose chip is no longer on offer.
        if self.stage_content_mode == StageContentMode::FinalOutput && final_output.is_none() {
            self.stage_content_mode = StageContentMode::Output;
        }

        let inner_h = content_area.height.saturating_sub(2) as usize;
        let render_width = content_area.width.saturating_sub(2);
        let is_output = self.stage_content_mode == StageContentMode::Output;

        // Build content lines. `showing_final_output` records whether the
        // pane is showing the run's final answer (the Final view, or the
        // Output pane's fallback to it), so the file-path hint below can point
        // at the file actually being shown.
        let (all_lines, context_row_lines, showing_final_output): (Vec<Line>, Vec<usize>, bool) =
            match (self.stage_content_mode, &final_output) {
                (StageContentMode::Context, _) => {
                    let (lines, rows) = self.build_context_lines(agent, render_width);
                    (lines, rows, false)
                }
                (StageContentMode::FinalOutput, Some(answer)) => {
                    (final_output_lines(answer, render_width), Vec::new(), true)
                }
                _ => {
                    let (lines, showing_final) =
                        self.build_output_lines(agent, is_output, render_width);
                    (lines, Vec::new(), showing_final)
                }
            };
        let context_cursor_line = context_row_lines.get(self.context_tree.cursor).copied();

        // ── Error / Cancelled banner ─────────────────────────────────────
        let mut all_lines = all_lines;
        match &agent.status {
            AgentDisplayStatus::Error(msg) if !msg.is_empty() => {
                all_lines.push(Line::from(vec![
                    Span::styled(
                        " ✗ Error  ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(C_ERROR)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!(" {}", msg), Style::default().fg(C_ERROR)),
                ]));
            }
            AgentDisplayStatus::Error(_) => {
                all_lines.push(Line::from(Span::styled(
                    " ✗ Agent terminated with an error.",
                    Style::default().fg(C_ERROR),
                )));
            }
            AgentDisplayStatus::Cancelled => {
                all_lines.push(Line::from(Span::styled(
                    " ⊘ Run was cancelled.",
                    Style::default().fg(C_DIM),
                )));
            }
            _ => {}
        }

        let total = all_lines.len();

        // ── Search: compute match indices + navigate ──────────────────────
        let query_lc = self.search_query.to_lowercase();
        let match_indices: Vec<usize> = if query_lc.is_empty() {
            Vec::new()
        } else {
            all_lines
                .iter()
                .enumerate()
                .filter_map(|(i, line)| {
                    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                    if text.to_lowercase().contains(&query_lc) {
                        Some(i)
                    } else {
                        None
                    }
                })
                .collect()
        };

        // Scrolling operates in *display rows*: long lines wrap at draw time,
        // so counting logical lines undercounts what is on screen and leaves
        // the wrapped tail clipped past the pane bottom. `line_count` measures
        // exactly what `Paragraph` will render at this width.
        let total_rows = wrapped_rows(&all_lines, render_width);

        // Which display row each interactive tree row starts on, so a click
        // can be turned back into the row it landed on. One cumulative pass
        // over the document; counting from the top per row would be quadratic
        // in the size of a context window, ten times a second.
        let context_row_offsets: Vec<usize> = if context_row_lines.is_empty() {
            Vec::new()
        } else {
            let mut cumulative = Vec::with_capacity(all_lines.len() + 1);
            let mut acc = 0usize;
            cumulative.push(0);
            for line in &all_lines {
                acc += wrapped_rows(std::slice::from_ref(line), render_width);
                cumulative.push(acc);
            }
            context_row_lines
                .iter()
                .map(|&line| cumulative[line.min(all_lines.len())])
                .collect()
        };

        // Clamp search_match_idx and jump to current match (centred by the
        // match's display row, since that is what the viewport scrolls by).
        if !match_indices.is_empty() {
            self.search_match_idx = self.search_match_idx.min(match_indices.len() - 1);
            let match_line = match_indices[self.search_match_idx];
            let rows_before = wrapped_rows(&all_lines[..match_line], render_width);
            self.detail_scroll = total_rows.saturating_sub(rows_before + inner_h / 2);
        } else if self.context_tree.follow_cursor
            && let Some(cursor_line) = context_cursor_line
        {
            // A tree-cursor move scrolls the view to the cursor once - the
            // same centering the search jump uses - then releases it so plain
            // scrolling still works.
            self.context_tree.follow_cursor = false;
            let rows_before =
                wrapped_rows(&all_lines[..cursor_line.min(all_lines.len())], render_width);
            self.detail_scroll = total_rows.saturating_sub(rows_before + inner_h / 2);
        }

        let max_scroll = total_rows.saturating_sub(inner_h);
        if self.detail_scroll > max_scroll {
            self.detail_scroll = max_scroll;
        }
        // Rows hidden above the viewport; 0 = top, max_scroll = bottom pinned.
        let scroll_y = max_scroll - self.detail_scroll;

        let visible: Vec<Line> = if total == 0 {
            let stage_name = agent
                .stages
                .get(self.selected_stage)
                .map(|s| s.name.as_str())
                .unwrap_or("this stage");
            vec![Line::from(Span::styled(
                format!(
                    " No {} yet for {}.",
                    if is_output { "output" } else { "logs" },
                    stage_name
                ),
                Style::default().fg(C_DIM),
            ))]
        } else {
            let current_match_line = match_indices.get(self.search_match_idx).copied();
            all_lines
                .iter()
                .enumerate()
                .map(|(abs_idx, line)| {
                    let is_current_match = current_match_line == Some(abs_idx);
                    let is_any_match = !query_lc.is_empty() && match_indices.contains(&abs_idx);
                    if is_current_match {
                        Line::from(
                            line.spans
                                .iter()
                                .map(|s| {
                                    Span::styled(
                                        s.content.clone(),
                                        Style::default()
                                            .fg(Color::Black)
                                            .bg(Color::Yellow)
                                            .add_modifier(Modifier::BOLD),
                                    )
                                })
                                .collect::<Vec<_>>(),
                        )
                    } else if is_any_match {
                        Line::from(
                            line.spans
                                .iter()
                                .map(|s| {
                                    Span::styled(
                                        s.content.clone(),
                                        Style::default().fg(C_WHITE).bg(Color::Rgb(80, 60, 0)),
                                    )
                                })
                                .collect::<Vec<_>>(),
                        )
                    } else {
                        line.clone()
                    }
                })
                .collect()
        };

        // Tool count badge for logs tab
        let tool_count = if self.stage_content_mode == StageContentMode::Logs {
            let raw = runstate::tail_stage_log(&agent.id, self.selected_stage, 131_072);
            let tc = raw.lines().filter(|l| l.starts_with("[tool]")).count();
            if tc > 0 {
                format!(" · {} tools", tc)
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        // Search indicator in the title
        let search_indicator = if !query_lc.is_empty() {
            if match_indices.is_empty() {
                format!(" 🔍/{}/  0 matches", self.search_query)
            } else {
                format!(
                    " /{}/  {}/{}",
                    self.search_query,
                    self.search_match_idx + 1,
                    match_indices.len()
                )
            }
        } else if self.search_mode {
            format!(" /{}▌", self.search_query)
        } else {
            String::new()
        };

        // The Final chip is offered only while the run has an answer to show.
        let final_chip = match (self.stage_content_mode, &final_output) {
            (StageContentMode::FinalOutput, _) | (_, None) => "",
            _ => "  [f] final",
        };
        let mode_label = match self.stage_content_mode {
            StageContentMode::Output => {
                format!(" Output  [l] logs  [c] ctx{final_chip}{tool_count}{search_indicator} ")
            }
            StageContentMode::Logs => {
                format!(" Logs  [o] output  [c] ctx{final_chip}{tool_count}{search_indicator} ")
            }
            StageContentMode::Context => {
                format!(" Context Window  [o] output  [l] logs{final_chip}{search_indicator} ")
            }
            StageContentMode::FinalOutput => {
                format!(" Final output  [o] output  [l] logs  [c] ctx{search_indicator} ")
            }
        };
        let scroll_info = if total_rows > inner_h {
            let pct = 100
                - (self.detail_scroll.min(max_scroll) * 100)
                    .checked_div(max_scroll)
                    .unwrap_or(0);
            format!(" {}% ({}/{}) ", pct, scroll_y + inner_h, total_rows)
        } else {
            String::new()
        };

        // Bottom-left file path hint
        let file_path_hint = {
            // The stage file each view reads; the Final view reads none, and
            // the Output view's fallback to the run's answer reads a run file
            // instead of its stage's.
            let stage_file = match self.stage_content_mode {
                StageContentMode::Output => Some("output.log"),
                StageContentMode::Logs => Some("logs.log"),
                StageContentMode::Context => Some(leviath_core::files::CONTEXT_FILE),
                StageContentMode::FinalOutput => None,
            };
            let raw = match stage_file.filter(|_| !showing_final_output) {
                Some(file_name) => runstate::stage_dir(&agent.id, self.selected_stage)
                    .join(file_name)
                    .to_string_lossy()
                    .to_string(),
                // The run's final answer lives in the `final_output` sidecar
                // beside `meta.json`, not in any stage's directory.
                None => runstate::final_output_path(&runstate::run_dir(&agent.id))
                    .to_string_lossy()
                    .to_string(),
            };
            // Display-only `~` abbreviation of the OS home directory;
            // deliberately NOT the LEVIATH_HOME-aware resolver (see the
            // header's workdir line for the same choice).
            let home = dirs::home_dir()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_default();
            let shortened = shorten_home_path(raw, &home);
            format!(" {} ", shortened)
        };

        // The mode chips in the title are buttons: `[l] logs` and friends do
        // by click exactly what their letter does by key.
        self.register_mode_chip_clicks(content_area, &mode_label);

        let content_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(C_BORDER_FOCUS))
            .title(Span::styled(
                mode_label.clone(),
                Style::default().fg(C_ACCENT),
            ))
            .title_bottom(
                Line::from(Span::styled(file_path_hint, Style::default().fg(C_DIM))).left_aligned(),
            )
            .title_bottom(Span::styled(scroll_info, Style::default().fg(C_DIM)));

        // The full text renders with a row offset (`scroll` applies after
        // wrapping), so the viewport is exact: the bottom row of the pane is
        // the bottom row of the document when detail_scroll is 0.
        let content_widget = Paragraph::new(visible)
            .block(content_block)
            .wrap(Wrap { trim: false })
            .scroll((scroll_y.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(content_widget, content_area);

        // Scrollbar, in display rows.
        if total_rows > inner_h {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓"));
            let mut sb_state = ScrollbarState::new(max_scroll).position(scroll_y);
            frame.render_stateful_widget(
                scrollbar,
                content_area.inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut sb_state,
            );
        }

        // Every tree row the viewport is actually showing becomes clickable,
        // on the row it was drawn on. Rows scrolled off screen register
        // nothing: a click cannot land on them.
        for (idx, &offset) in context_row_offsets.iter().enumerate() {
            let Some(y) = offset.checked_sub(scroll_y).filter(|y| *y < inner_h) else {
                continue;
            };
            self.register_click(
                Rect::new(
                    content_area.x + 1,
                    content_area.y + 1 + y as u16,
                    render_width,
                    1,
                ),
                ClickTarget::ContextRow(idx),
            );
        }
    }

    /// Make the content pane's title chips (`[l] logs`, `[o] output`,
    /// `[c] ctx`, `[f] final`) clickable, over the exact columns they were
    /// drawn on.
    ///
    /// The title renders inside the top border, starting one cell in, so the
    /// character offsets in the label are the screen columns. Only the chips
    /// actually in this label are registered - the current mode has no chip of
    /// its own, and registering a rect for text that is not there would put a
    /// button over the run's search indicator.
    fn register_mode_chip_clicks(&mut self, content_area: Rect, mode_label: &str) {
        if content_area.width < 2 {
            return;
        }
        for (chip, mode) in [
            ("[l] logs", StageContentMode::Logs),
            ("[o] output", StageContentMode::Output),
            ("[c] ctx", StageContentMode::Context),
            ("[f] final", StageContentMode::FinalOutput),
        ] {
            let Some(byte) = mode_label.find(chip) else {
                continue;
            };
            // Characters, not bytes: the label carries a search indicator that
            // can hold anything the user typed.
            let before = mode_label
                .char_indices()
                .take_while(|(at, _)| *at < byte)
                .count() as u16;
            let column = content_area.x + 1 + before;
            let width = chip.chars().count() as u16;
            if column + width <= content_area.x + content_area.width {
                self.register_click(
                    Rect::new(column, content_area.y, width, 1),
                    ClickTarget::ContentMode(mode),
                );
            }
        }
    }

    /// The Context view's lines, plus the line each interactive tree row
    /// (region header, entry stub) starts on - what the renderer needs to
    /// scroll to the cursor and to hand a click the row it landed on.
    fn build_context_lines(
        &self,
        agent: &DashboardAgent,
        render_width: u16,
    ) -> (Vec<Line<'static>>, Vec<usize>) {
        // When browsing the run's archived context history, show that point's
        // window; otherwise the live current window for the selected stage.
        let snap_opt = self
            .browsed_context_point()
            .map(|p| p.context.clone())
            .or_else(|| runstate::read_stage_context(&agent.id, self.selected_stage))
            .or_else(|| agent.context_snapshot.as_deref().cloned());
        if let Some(snap) = snap_opt {
            let mut lines: Vec<Line> = Vec::new();

            // ── Graph transition details ──
            // A linear blueprint's chain is a graph too, but "Transitions:
            // -> next" on every stage would be noise; this block is for the
            // ones that branch.
            if let Some(graph) = agent.graph.as_ref().filter(|g| g.is_branching) {
                let sel_name = agent
                    .stages
                    .get(self.selected_stage)
                    .map(|s| s.name.as_str())
                    .or_else(|| graph.ids().nth(self.selected_stage))
                    .unwrap_or(&agent.stage);

                lines.push(Line::from(vec![
                    Span::styled(
                        "▌ ",
                        Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("Stage: {}", sel_name),
                        Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                    ),
                ]));

                let vc = agent.stages.iter().filter(|s| s.name == sel_name).count();
                if vc > 0 {
                    lines.push(Line::from(Span::styled(
                        format!("  Visited {} time{}", vc, if vc != 1 { "s" } else { "" }),
                        Style::default().fg(C_MUTED),
                    )));
                }

                // Outgoing transitions
                let edges: Vec<_> = graph.outgoing(sel_name).collect();
                if edges.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "  Transitions: (terminal - no outgoing edges)",
                        Style::default().fg(C_DIM),
                    )));
                } else {
                    lines.push(Line::from(Span::styled(
                        "  Transitions:",
                        Style::default().fg(C_MUTED),
                    )));
                    for edge in edges {
                        let label = edge.condition_label();
                        let cond_part = if label.is_empty() {
                            String::new()
                        } else {
                            format!(" [{label}]")
                        };
                        let hint_part = edge
                            .hint
                            .as_deref()
                            .map(|h| format!(" - {}", h))
                            .unwrap_or_default();
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("    → {}", edge.to),
                                Style::default().fg(C_ACCENT),
                            ),
                            Span::styled(cond_part, Style::default().fg(C_WARN)),
                            Span::styled(hint_part, Style::default().fg(C_DIM)),
                        ]));
                    }
                }

                // Incoming transitions
                let incoming: Vec<_> = graph.edges.iter().filter(|e| e.to == sel_name).collect();
                if !incoming.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "  Incoming from:",
                        Style::default().fg(C_MUTED),
                    )));
                    for edge in &incoming {
                        let transform_part = format!(" [transform: {}]", edge.transform);
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("    ← {}", edge.from),
                                Style::default().fg(C_SUCCESS),
                            ),
                            Span::styled(transform_part, Style::default().fg(C_DIM)),
                        ]));
                    }
                }

                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "─".repeat(32),
                    Style::default().fg(C_DIM),
                )));
                lines.push(Line::from(""));
            }

            // Overall usage header
            let total_pct = (snap.total_tokens * 100)
                .checked_div(snap.max_tokens)
                .unwrap_or(0)
                .min(100);
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} regions  ", snap.regions.len()),
                    Style::default().fg(C_DIM),
                ),
                Span::styled(
                    format!(
                        "{}/{} tokens total  {}%",
                        format_tokens(snap.total_tokens),
                        format_tokens(snap.max_tokens),
                        total_pct
                    ),
                    Style::default().fg(C_MUTED),
                ),
            ]));

            // Detect old runs
            let has_tokens = snap.regions.iter().any(|r| r.current_tokens > 0);
            let has_entries = snap.regions.iter().any(|r| !r.entries.is_empty());
            if has_tokens && !has_entries {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    " ℹ  This run predates context content capture.",
                    Style::default().fg(C_WARN),
                )));
                lines.push(Line::from(Span::styled(
                    "    Token counts are shown but entry content is unavailable.",
                    Style::default().fg(C_DIM),
                )));
                lines.push(Line::from(Span::styled(
                    "    Re-run the agent to see full context details.",
                    Style::default().fg(C_DIM),
                )));
                lines.push(Line::from(""));
            }

            lines.push(Line::from(""));
            // The collapsible region → entry tree. An active search forces
            // everything visible so matches inside entries stay reachable.
            let searching = self.search_mode || !self.search_query.is_empty();
            let flat = crate::commands::dashboard::context_tree::flatten(
                &snap,
                &self.context_tree,
                self.context_tree.cursor,
                searching,
                render_width,
            );
            let base = lines.len();
            let row_lines = flat.cursor_lines.iter().map(|line| base + line).collect();
            lines.extend(flat.lines);
            (lines, row_lines)
        } else {
            (
                vec![Line::from(Span::styled(
                    " no context snapshot available for this stage",
                    Style::default().fg(C_DIM),
                ))],
                Vec::new(),
            )
        }
    }

    /// The run's final answer, when the selected stage is the one that
    /// submitted it.
    ///
    /// A `mode = "output"` stage answers through `submit_output`, which lands
    /// in the run's `final_output` sidecar rather than the stage's
    /// `output.log`. The descriptor records which stage submitted, so the
    /// answer only stands in for that stage's otherwise empty Output pane -
    /// every other stage keeps its honest empty state.
    fn final_output_for_selected_stage(&self, agent: &DashboardAgent) -> Option<String> {
        let final_output = crate::runstate::read_final_output(&agent.id)?;
        let selected_name = agent.stages.get(self.selected_stage)?.name.as_str();
        if final_output.stage == selected_name {
            Some(final_output.content)
        } else {
            None
        }
    }

    /// The Output or Logs view's lines, plus whether the Output view fell back
    /// to the run's final answer (so the file-path hint can name that file
    /// instead of the stage's `output.log`).
    fn build_output_lines(
        &self,
        agent: &DashboardAgent,
        is_output: bool,
        render_width: u16,
    ) -> (Vec<Line<'static>>, bool) {
        let mut showing_final_output = false;
        let content = if is_output {
            // When a document is up for review (a pending interaction's body,
            // e.g. the plan being approved), show just that current instance -
            // not the full accumulated output history. `[l]` still shows logs.
            match self.reviewing_body() {
                Some(body) => body,
                None => {
                    let tail = runstate::tail_stage_output(&agent.id, self.selected_stage, 131_072);
                    if tail.is_empty() {
                        // Nothing in the stage's output.log. If this stage
                        // submitted the run's final answer, show that instead
                        // of a permanent "No output yet" (issue #410).
                        match self.final_output_for_selected_stage(agent) {
                            Some(answer) => {
                                showing_final_output = true;
                                answer
                            }
                            None => tail,
                        }
                    } else {
                        tail
                    }
                }
            }
        } else {
            runstate::tail_stage_log(&agent.id, self.selected_stage, 131_072)
        };

        let lines = if is_output && !content.is_empty() {
            crate::render::markdown_to_text(&content, render_width).lines
        } else if !is_output {
            content
                .lines()
                .map(|l| {
                    // The tag is carried as the literal itself rather than its
                    // length, so the two can never disagree.
                    let (color, tag) = if l.starts_with("[tool]") {
                        (C_ACCENT, "[tool]")
                    } else if l.starts_with("[error]") {
                        (C_ERROR, "[error]")
                    } else if l.starts_with("[denied]") {
                        (C_WARN, "[denied]")
                    } else if l.starts_with("---") || l.starts_with("[All") {
                        (C_DIM, "")
                    } else {
                        (C_MUTED, "")
                    };
                    // Every arm above picked `tag` off a `starts_with` that just
                    // matched, and the empty tag strips nothing.
                    let rest = l.strip_prefix(tag).unwrap_or(l);
                    if !tag.is_empty() && !rest.is_empty() {
                        // A tagged line reads as a bold tag plus muted body.
                        Line::from(vec![
                            Span::styled(
                                format!(" {tag}"),
                                Style::default().fg(color).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(rest.to_string(), Style::default().fg(C_MUTED)),
                        ])
                    } else {
                        Line::from(Span::styled(format!(" {}", l), Style::default().fg(color)))
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        (lines, showing_final_output)
    }
}

/// The Final view's lines: one header naming the stage that submitted the
/// answer and the format it was produced under, then the answer rendered as
/// markdown, the way the Output view renders a stage's output.
fn final_output_lines(answer: &leviath_core::FinalOutput, width: u16) -> Vec<Line<'static>> {
    let format = answer.format.as_deref().unwrap_or("no format");
    let truncated = match answer.truncated {
        true => " · truncated",
        false => "",
    };
    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                " submitted by stage {} · format: {format}{truncated}",
                answer.stage
            ),
            Style::default().fg(C_DIM),
        )),
        Line::from(""),
    ];
    lines.extend(crate::render::markdown_to_text(&answer.content, width).lines);
    lines
}

/// The number of display rows `lines` occupy at `width` once `Paragraph`
/// wraps them - the same measurement the renderer itself uses, so the scroll
/// math can never disagree with what is on screen.
pub(in crate::commands::dashboard) fn wrapped_rows(lines: &[Line<'static>], width: u16) -> usize {
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::{make_test_dashboard, rendered_buffer};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    fn make_test_agent(id: &str, status: AgentDisplayStatus) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "test-agent".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 1,
            status,
            tokens_in: 100,
            tokens_out: 50,
            cached_tokens: 10,
            iteration: 3,
            broken_scripts: Vec::new(),
            waiting_prompt: None,
            wait_reason: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            workdir: "/tmp/test".to_string(),
            task: "test task".to_string(),
            title: Some("My Test".to_string()),
            model: None,
            parent_id: None,
            started_at: chrono::Utc::now().timestamp() - 60,
            last_progress_at: None,
            runtime_secs: 0,
            clock_now: 0,
            graph: None,
            accepts_messages: true,
        }
    }

    fn make_context_snapshot(total: usize, max: usize) -> runstate::ContextSnapshot {
        runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: total,
            max_tokens: max,
            regions: vec![runstate::RegionSnapshot {
                name: "system".to_string(),
                kind: "pinned".to_string(),
                current_tokens: total / 2,
                max_tokens: max / 2,
                entries: vec![],
                description: None,
            }],
        }
    }

    #[test]
    fn render_context_bar_with_snapshot() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-ctx", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(make_context_snapshot(4000, 8000)));
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("ctx"), "{buf}");
    }

    #[test]
    fn render_context_bar_without_snapshot() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-noctx", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(!buf.contains("%"), "{buf}");
    }

    #[test]
    fn render_context_bar_high_fill() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-high", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(make_context_snapshot(7500, 8000))); // 93% = red
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("ctx"), "{buf}");
    }

    #[test]
    fn render_context_bar_medium_fill() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-med", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(make_context_snapshot(6000, 8000))); // 75% = yellow
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("ctx"), "{buf}");
    }

    #[test]
    fn render_context_bar_multiple_regions() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-multi", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 5000,
            max_tokens: 8000,
            regions: vec![
                runstate::RegionSnapshot {
                    name: "system".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                    description: None,
                },
                runstate::RegionSnapshot {
                    name: "history".to_string(),
                    // The word a snapshot writes now. The older `sliding` has
                    // to draw the same letter, which is what
                    // `render_context_bar_regions_string_with_many_region_types`
                    // covers with a snapshot spelled the old way.
                    kind: "sliding_window".to_string(),
                    current_tokens: 3000,
                    max_tokens: 4000,
                    entries: vec![],
                    description: None,
                },
                runstate::RegionSnapshot {
                    name: "context".to_string(),
                    kind: "compacting".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                    description: None,
                },
            ],
        }));
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        // The bar shows one initial per region, so three regions is the thing
        // this case adds over the single-region ones above.
        assert!(buf.contains("[P S H]"), "{buf}");
    }

    #[test]
    fn render_content_pane_output_mode() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        let agent = make_test_agent("run-out", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Output"), "{buf}");
    }

    #[test]
    fn render_content_pane_shows_pending_review_body() {
        // Output mode with a pending interaction body shows just that document
        // (the current plan) instead of the accumulated output history.
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        let mut agent = make_test_agent("run-review", AgentDisplayStatus::Waiting);
        let mut req = leviath_core::interaction::InteractionRequest::multiple_choice(
            "mc1",
            "Approve?",
            vec!["Approve".to_string()],
            "plan_approval",
        );
        req.body = Some("## Plan\n1. write the script".to_string());
        agent.pending_request = Some(req);
        dash.agents.push(agent.clone());
        dash.update_display_indices();
        assert!(dash.reviewing_body().is_some());
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    #[test]
    fn render_content_pane_inline_document_edit() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-edit", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::edit_text(
            "et1",
            "Edit",
            "plan",
            "line A\nline B",
        ));
        dash.agents.push(agent.clone());
        dash.update_display_indices();
        dash.input_mode = true;
        // The editable textarea takes over the content pane instead of output.
        assert!(dash.editing_document());
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        assert!(
            dash.click_targets
                .iter()
                .any(|(_, t)| *t == ClickTarget::ResponseSend),
            "the Save button is drawn under the editor"
        );
    }

    #[test]
    fn render_content_pane_logs_mode() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Logs;
        let agent = make_test_agent("run-logs", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Logs"), "{buf}");
    }

    #[test]
    fn render_content_pane_context_mode() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Context;
        let agent = make_test_agent("run-ctxm", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Context"), "{buf}");
    }

    #[test]
    fn render_content_pane_error_banner() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        let agent = make_test_agent(
            "run-err",
            AgentDisplayStatus::Error("something broke".to_string()),
        );
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Error"), "{buf}");
        assert!(buf.contains("something broke"), "{buf}");
    }

    #[test]
    fn render_content_pane_error_empty_message() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        let agent = make_test_agent("run-err2", AgentDisplayStatus::Error(String::new()));
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(!buf.contains("Error  "), "{buf}");
    }

    #[test]
    fn render_content_pane_cancelled_banner() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        let agent = make_test_agent("run-cancel", AgentDisplayStatus::Cancelled);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Run was cancelled."), "{buf}");
    }

    #[test]
    fn render_content_pane_with_search() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        dash.search_query = "test".to_string();
        let agent = make_test_agent("run-search", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("test"), "{buf}");
    }

    #[test]
    fn render_content_pane_search_mode_active() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        dash.search_mode = true;
        dash.search_query = "find".to_string();
        let agent = make_test_agent("run-sm", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("find"), "{buf}");
    }

    #[test]
    fn render_content_pane_search_mode_active_empty_query_shows_cursor() {
        // search_mode on but no query typed yet -> the "▌" cursor indicator
        // branch (query_lc.is_empty() && self.search_mode).
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        dash.search_mode = true;
        dash.search_query = String::new();
        let agent = make_test_agent("run-sm-empty", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();

        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains('▌'));
    }

    #[test]
    fn render_content_pane_clamps_scroll_beyond_available_lines() {
        // detail_scroll set far beyond the (empty) content's max_scroll must
        // be clamped rather than underflowing/panicking.
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        dash.detail_scroll = 9999;
        let agent = make_test_agent("run-clamp", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        assert_eq!(dash.detail_scroll, 0);
    }

    fn setup_run_state_agent_with_logs(
        run_id: &str,
        log_lines: &[&str],
        output_text: Option<&str>,
    ) -> DashboardAgent {
        let dir = runstate::run_dir(run_id);
        // Defensive cleanup: if a previous run of a test using this fixed
        // run_id panicked before reaching its own cleanup (e.g. a failed
        // assertion), stale log/output files from that run would otherwise
        // accumulate here across every subsequent `cargo test` invocation --
        // this bit us for real (a stale `logs.log` with dozens of duplicated
        // `[tool]` lines, some corrupted from concurrent-append races,
        // silently broke `render_content_pane_logs_mode_shows_tool_count_badge`'s
        // exact-count assertion on every run until this was added).
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/p".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        runstate::create_run(&meta).unwrap();
        for line in log_lines {
            runstate::append_stage_log(run_id, 0, line);
        }
        if let Some(text) = output_text {
            runstate::append_stage_output(run_id, 0, text);
        }

        make_test_agent(run_id, AgentDisplayStatus::Active)
    }

    /// Seed a run whose answer arrived through `submit_output`: the
    /// `final_output` descriptor in `meta.json` plus the sidecar beside it,
    /// submitted by `stage`, with no `output.log` anywhere. Returns an agent
    /// pointed at that run.
    fn setup_run_state_agent_with_final_output(
        run_id: &str,
        stage: &str,
        content: &str,
    ) -> DashboardAgent {
        crate::commands::dashboard::test_support::seed_run_with_final_output(
            run_id, stage, content,
        );
        make_test_agent(run_id, AgentDisplayStatus::Complete)
    }

    // ─── the Final view ──────────────────────────────────────────────────

    /// The `[f] final` chip is on the title only while the run has an answer,
    /// and its click target with it.
    #[test]
    fn the_final_chip_is_offered_only_when_the_run_has_an_answer() {
        runstate::with_isolated_runs_dir(
            "the_final_chip_is_offered_only_when_the_run_has_an_answer",
            |_d| {
                let mut dash = make_test_dashboard();
                dash.stage_content_mode = StageContentMode::Output;

                let without = setup_run_state_agent_with_logs("final-chip-no", &[], Some("out"));
                let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
                terminal
                    .draw(|f| dash.render_content_pane(f, Rect::new(0, 0, 100, 20), &without, 100))
                    .unwrap();
                let buf = rendered_buffer(&terminal);
                assert!(!buf.contains("[f] final"), "{buf}");
                assert!(
                    !dash
                        .click_targets
                        .iter()
                        .any(|(_, t)| *t == ClickTarget::ContentMode(StageContentMode::FinalOutput)),
                    "no chip, no button"
                );

                dash.click_targets.clear();
                let with = setup_run_state_agent_with_final_output(
                    "final-chip-yes",
                    "present",
                    "the answer",
                );
                let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
                terminal
                    .draw(|f| dash.render_content_pane(f, Rect::new(0, 0, 100, 20), &with, 100))
                    .unwrap();
                let buf = rendered_buffer(&terminal);
                assert!(buf.contains("[f] final"), "{buf}");
                assert!(
                    dash.click_targets
                        .iter()
                        .any(|(_, t)| *t == ClickTarget::ContentMode(StageContentMode::FinalOutput)),
                    "the chip is a button"
                );
                // Every other view offers it too, and none of them offers its
                // own chip.
                for mode in [StageContentMode::Logs, StageContentMode::Context] {
                    dash.stage_content_mode = mode;
                    let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
                    terminal
                        .draw(|f| dash.render_content_pane(f, Rect::new(0, 0, 100, 20), &with, 100))
                        .unwrap();
                    let buf = rendered_buffer(&terminal);
                    assert!(buf.contains("[f] final"), "{mode:?}: {buf}");
                }
            },
        );
    }

    /// The view shows the bytes `runstate::read_final_output` returns, which
    /// is what the HTTP API serves, under a header naming the submitting
    /// stage and the format, with the sidecar named in the file-path hint.
    #[test]
    fn the_final_view_shows_exactly_what_the_api_serves() {
        runstate::with_isolated_runs_dir(
            "the_final_view_shows_exactly_what_the_api_serves",
            |_d| {
                let run_id = "final-view-content";
                let agent = setup_run_state_agent_with_final_output(
                    run_id,
                    "present",
                    "The **submitted** answer, not the chat.",
                );
                // The same stage also chatted something else into its output.log,
                // which is what the Output view shows and this view must not.
                runstate::append_stage_output(run_id, 0, "a chatty draft nobody submitted");
                let served = runstate::read_final_output(run_id).expect("the sidecar was written");
                assert!(
                    served.content.contains("**submitted**"),
                    "the probe's input is real"
                );

                let mut dash = make_test_dashboard();
                dash.stage_content_mode = StageContentMode::FinalOutput;
                let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
                terminal
                    .draw(|f| dash.render_content_pane(f, Rect::new(0, 0, 100, 20), &agent, 100))
                    .unwrap();
                let buf = rendered_buffer(&terminal);
                assert_eq!(dash.stage_content_mode, StageContentMode::FinalOutput);
                assert!(
                    buf.contains(" Final output  [o] output  [l] logs  [c] ctx "),
                    "{buf}"
                );
                // Rendered as markdown: the emphasis markers become styling.
                assert!(buf.contains("The submitted answer, not the chat."), "{buf}");
                assert!(!buf.contains("chatty draft"), "{buf}");
                assert!(buf.contains("submitted by stage present"), "{buf}");
                assert!(buf.contains("format: markdown"), "{buf}");
                assert!(buf.contains(leviath_core::FINAL_OUTPUT_FILE), "{buf}");
                assert!(!buf.contains("output.log"), "{buf}");
                let chips: Vec<ClickTarget> = dash.click_targets.iter().map(|(_, t)| *t).collect();
                for mode in [
                    StageContentMode::Output,
                    StageContentMode::Logs,
                    StageContentMode::Context,
                ] {
                    assert!(chips.contains(&ClickTarget::ContentMode(mode)), "{mode:?}");
                }
                assert!(
                    !chips.contains(&ClickTarget::ContentMode(StageContentMode::FinalOutput)),
                    "the view showing has no chip of its own"
                );
            },
        );
    }

    /// Selecting a run without an answer while in the Final view lands on
    /// Output, the view the chip would otherwise leave unreachable.
    #[test]
    fn the_final_view_falls_back_to_output_for_a_run_without_an_answer() {
        runstate::with_isolated_runs_dir(
            "the_final_view_falls_back_to_output_for_a_run_without_an_answer",
            |_d| {
                let agent = setup_run_state_agent_with_logs("final-fallback", &[], Some("plain"));
                let mut dash = make_test_dashboard();
                dash.stage_content_mode = StageContentMode::FinalOutput;
                let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
                terminal
                    .draw(|f| dash.render_content_pane(f, Rect::new(0, 0, 100, 20), &agent, 100))
                    .unwrap();
                assert_eq!(dash.stage_content_mode, StageContentMode::Output);
                let buf = rendered_buffer(&terminal);
                assert!(buf.contains(" Output  [l] logs  [c] ctx "), "{buf}");
                assert!(buf.contains("plain"), "{buf}");
                assert!(buf.contains("output.log"), "{buf}");
            },
        );
    }

    #[test]
    fn final_output_lines_name_a_missing_format_and_truncation() {
        let mut answer = leviath_core::FinalOutput::new("body", None, "last".to_string(), 1);
        answer.truncated = true;
        let lines = final_output_lines(&answer, 80);
        let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            header,
            " submitted by stage last · format: no format · truncated"
        );
        let body: String = lines
            .iter()
            .skip(2)
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(body.contains("body"), "{body}");
    }

    /// A minimal stage record carrying just the name the final-output fallback
    /// compares against.
    fn make_stage_record(name: &str) -> crate::runstate::StageRecord {
        crate::runstate::StageRecord {
            status: crate::runstate::StageRunStatus::Active,
            entered: true,
            ..crate::runstate::StageRecord::new(name.to_string(), 0)
        }
    }

    #[test]
    fn render_content_pane_logs_mode_shows_tool_count_badge() {
        crate::runstate::with_isolated_runs_dir(
            "render_content_pane_logs_mode_shows_tool_count_badge",
            |_d| {
                let run_id = "test-content-tool-badge";
                let agent = setup_run_state_agent_with_logs(
                    run_id,
                    &["[tool] read_file(x.rs)", "[tool] write_file(y.rs)"],
                    None,
                );

                let backend = TestBackend::new(120, 40);
                let mut terminal = Terminal::new(backend).unwrap();
                let mut dash = make_test_dashboard();
                dash.stage_content_mode = StageContentMode::Logs;
                terminal
                    .draw(|f| {
                        let area = Rect::new(0, 0, 100, 20);
                        dash.render_content_pane(f, area, &agent, 100);
                    })
                    .unwrap();

                let content: String = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                assert!(content.contains("2 tools"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
            },
        );
    }

    #[test]
    fn render_content_pane_output_mode_run_state_shows_file_path_hint() {
        crate::runstate::with_isolated_runs_dir(
            "render_content_pane_output_mode_run_state_shows_file_path_hint",
            |_d| {
                let run_id = "test-content-output-hint";
                let agent = setup_run_state_agent_with_logs(run_id, &[], Some("hello output"));

                let backend = TestBackend::new(120, 40);
                let mut terminal = Terminal::new(backend).unwrap();
                let mut dash = make_test_dashboard();
                dash.stage_content_mode = StageContentMode::Output;
                terminal
                    .draw(|f| {
                        let area = Rect::new(0, 0, 100, 20);
                        dash.render_content_pane(f, area, &agent, 100);
                    })
                    .unwrap();

                let content: String = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                assert!(content.contains("output.log"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
            },
        );
    }

    // The file-path hint's home-shortening logic is exercised directly against
    // `shorten_home_path` rather than through a full render. Reaching the raw
    // (path-outside-home) branch through the render path requires a runs dir
    // outside `$HOME`, which isn't portable: `std::env::temp_dir()` lives *under*
    // the home directory on Windows, and hard-coding `/tmp` is Unix-only.
    #[test]
    fn shorten_home_path_replaces_home_prefix_with_tilde() {
        assert_eq!(
            shorten_home_path("/home/u/.leviath/runs/x".to_string(), "/home/u"),
            "~/.leviath/runs/x"
        );
    }

    #[test]
    fn shorten_home_path_keeps_raw_path_when_outside_home() {
        // Path does not start with home -> raw branch.
        assert_eq!(
            shorten_home_path("/var/other/x".to_string(), "/home/u"),
            "/var/other/x"
        );
    }

    #[test]
    fn shorten_home_path_keeps_raw_path_when_home_empty() {
        // Empty home (dirs::home_dir() returned None) -> raw branch.
        assert_eq!(
            shorten_home_path("/anything/x".to_string(), ""),
            "/anything/x"
        );
    }

    #[test]
    fn build_output_lines_logs_mode_colors_by_line_prefix() {
        crate::runstate::with_isolated_runs_dir(
            "build_output_lines_logs_mode_colors_by_line_prefix",
            |_d| {
                let run_id = "test-content-log-prefixes";
                let agent = setup_run_state_agent_with_logs(
                    run_id,
                    &[
                        "[tool] did a thing",
                        "[error] it broke",
                        "[denied] not allowed",
                        "--- separator ---",
                        "[All stages complete]",
                        "a plain message",
                    ],
                    None,
                );

                let dash = make_test_dashboard();
                let (lines, _showing_final) = dash.build_output_lines(&agent, false, 100);
                let text: String = lines
                    .iter()
                    .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("did a thing"));
                assert!(text.contains("it broke"));
                assert!(text.contains("not allowed"));
                assert!(text.contains("separator"));
                assert!(text.contains("All stages complete"));
                assert!(text.contains("a plain message"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
            },
        );
    }

    #[test]
    fn build_output_lines_output_mode_renders_markdown_when_non_empty() {
        crate::runstate::with_isolated_runs_dir(
            "build_output_lines_output_mode_renders_markdown_when_non_empty",
            |_d| {
                let run_id = "test-content-output-markdown";
                let agent =
                    setup_run_state_agent_with_logs(run_id, &[], Some("# Heading\n\nbody text"));

                let dash = make_test_dashboard();
                let (lines, showing_final) = dash.build_output_lines(&agent, true, 100);
                assert!(!lines.is_empty());
                // A non-empty output.log is the real stage output, not the
                // final-answer fallback.
                assert!(!showing_final);
                let text: String = lines
                    .iter()
                    .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("Heading"));
                assert!(text.contains("body text"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
            },
        );
    }

    #[test]
    fn build_output_lines_falls_back_to_final_output_for_the_submitting_stage() {
        crate::runstate::with_isolated_runs_dir(
            "build_output_lines_falls_back_to_final_output_for_the_submitting_stage",
            |_d| {
                let run_id = "test-content-final-output-match";
                // A `mode = "output"` stage writes nothing to output.log; its
                // answer lives only in the final_output sidecar (issue #410).
                let mut agent = setup_run_state_agent_with_final_output(
                    run_id,
                    "present",
                    "# The Answer\n\nall done",
                );
                agent.stages = vec![make_stage_record("present")];

                let dash = make_test_dashboard();
                let (lines, showing_final) = dash.build_output_lines(&agent, true, 100);
                assert!(showing_final);
                let text: String = lines
                    .iter()
                    .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("The Answer"));
                assert!(text.contains("all done"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
            },
        );
    }

    #[test]
    fn build_output_lines_ignores_final_output_submitted_by_another_stage() {
        crate::runstate::with_isolated_runs_dir(
            "build_output_lines_ignores_final_output_submitted_by_another_stage",
            |_d| {
                let run_id = "test-content-final-output-other-stage";
                let mut agent =
                    setup_run_state_agent_with_final_output(run_id, "present", "the answer");
                // The selected stage (index 0) is not the one that submitted,
                // so its Output pane keeps the honest empty state.
                agent.stages = vec![make_stage_record("draft")];

                let backend = TestBackend::new(120, 40);
                let mut terminal = Terminal::new(backend).unwrap();
                let mut dash = make_test_dashboard();
                let (lines, showing_final) = dash.build_output_lines(&agent, true, 100);
                assert!(lines.is_empty());
                assert!(!showing_final);

                dash.stage_content_mode = StageContentMode::Output;
                terminal
                    .draw(|f| {
                        let area = Rect::new(0, 0, 100, 20);
                        dash.render_content_pane(f, area, &agent, 100);
                    })
                    .unwrap();
                let buf = rendered_buffer(&terminal);
                assert!(buf.contains("No output yet for draft"), "{buf}");

                let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
            },
        );
    }

    #[test]
    fn build_output_lines_ignores_final_output_without_a_stage_record() {
        crate::runstate::with_isolated_runs_dir(
            "build_output_lines_ignores_final_output_without_a_stage_record",
            |_d| {
                let run_id = "test-content-final-output-no-record";
                // No stage records at all: the selected stage has no name to
                // compare against the descriptor's, so nothing substitutes.
                let agent =
                    setup_run_state_agent_with_final_output(run_id, "present", "the answer");
                assert!(agent.stages.is_empty());

                let dash = make_test_dashboard();
                let (lines, showing_final) = dash.build_output_lines(&agent, true, 100);
                assert!(lines.is_empty());
                assert!(!showing_final);

                let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
            },
        );
    }

    #[test]
    fn build_output_lines_prefers_stage_output_over_final_output() {
        crate::runstate::with_isolated_runs_dir(
            "build_output_lines_prefers_stage_output_over_final_output",
            |_d| {
                let run_id = "test-content-final-output-tail-wins";
                let mut agent =
                    setup_run_state_agent_with_final_output(run_id, "present", "the answer");
                agent.stages = vec![make_stage_record("present")];
                // The stage also wrote real output, which always wins over the
                // final-answer fallback.
                runstate::append_stage_output(run_id, 0, "streamed stage output");

                let dash = make_test_dashboard();
                let (lines, showing_final) = dash.build_output_lines(&agent, true, 100);
                assert!(!showing_final);
                let text: String = lines
                    .iter()
                    .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("streamed stage output"));
                assert!(!text.contains("the answer"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
            },
        );
    }

    #[test]
    fn render_content_pane_final_output_fallback_names_the_final_output_file() {
        crate::runstate::with_isolated_runs_dir(
            "render_content_pane_final_output_fallback_names_the_final_output_file",
            |_d| {
                let run_id = "test-content-final-output-hint";
                let mut agent =
                    setup_run_state_agent_with_final_output(run_id, "present", "the answer");
                agent.stages = vec![make_stage_record("present")];

                let backend = TestBackend::new(120, 40);
                let mut terminal = Terminal::new(backend).unwrap();
                let mut dash = make_test_dashboard();
                dash.stage_content_mode = StageContentMode::Output;
                terminal
                    .draw(|f| {
                        let area = Rect::new(0, 0, 100, 20);
                        dash.render_content_pane(f, area, &agent, 100);
                    })
                    .unwrap();
                let buf = rendered_buffer(&terminal);
                assert!(buf.contains("the answer"), "{buf}");
                // The hint names the file actually shown, not the stage's
                // (empty) output.log.
                assert!(buf.contains(leviath_core::FINAL_OUTPUT_FILE), "{buf}");
                assert!(!buf.contains("output.log"), "{buf}");

                let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
            },
        );
    }

    #[test]
    fn build_context_lines_with_snapshot() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-bcl", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(make_context_snapshot(4000, 8000)));
        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        assert!(!lines.is_empty());
    }

    #[test]
    fn build_context_lines_without_snapshot() {
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-bcl2", AgentDisplayStatus::Active);
        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        assert!(!lines.is_empty()); // should show "no context snapshot" message
    }

    /// A parsed blueprint's stage graph, the way `sync_from_run_state` loads it.
    fn graph_from(toml: &str) -> Option<std::sync::Arc<crate::tui::flowgraph::StageGraph>> {
        Some(std::sync::Arc::new(
            crate::tui::flowgraph::StageGraph::from_blueprint(
                &leviath_core::manifest::parse_manifest(toml).expect("fixture parses"),
            ),
        ))
    }

    #[test]
    fn build_context_lines_with_graph_info() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-graph", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(make_context_snapshot(4000, 8000)));
        agent.graph = graph_from(
            r#"
[agent]
name = "g"
[stages.main]
[stages.main.transitions.implement]
hint = "after plan"
[stages.implement]
"#,
        );
        agent.stages = vec![crate::runstate::StageRecord {
            status: crate::runstate::StageRunStatus::Active,
            entered: true,
            prompt_tokens: 100,
            completion_tokens: 50,
            started_at: Some(chrono::Utc::now().timestamp() - 30),
            ..crate::runstate::StageRecord::new("main".to_string(), 0)
        }];
        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("Stage:"));
    }

    #[test]
    fn build_context_lines_graph_info_falls_back_to_stage_names_when_no_stage_record() {
        // agent.stages doesn't have an entry for the selected index, so
        // sel_name must fall back to graph.stage_names.get(selected_stage).
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-graph-fallback", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(make_context_snapshot(4000, 8000)));
        agent.graph = graph_from(
            r#"
[agent]
name = "g"
[stages.main]
[stages.main.transitions.implement]
[stages.implement]
"#,
        );
        agent.stages = vec![]; // no stage records at all -> .get(0) is None

        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("Stage: main"));
    }

    #[test]
    fn build_context_lines_graph_info_shows_visited_count() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-graph-visited", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(make_context_snapshot(4000, 8000)));
        agent.graph =
            graph_from("[agent]\nname = \"g\"\n[stages.main]\n[stages.main.transitions]\n");
        // Two records named "main" -> visited count 2, exercising the plural "s".
        let rec = crate::runstate::StageRecord {
            status: crate::runstate::StageRunStatus::Active,
            entered: true,
            started_at: Some(chrono::Utc::now().timestamp() - 30),
            ..crate::runstate::StageRecord::new("main".to_string(), 0)
        };
        agent.stages = vec![rec.clone(), rec];

        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("Visited 2 times"));
    }

    #[test]
    fn build_context_lines_graph_info_edge_with_non_always_condition() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-graph-cond", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(make_context_snapshot(4000, 8000)));
        agent.graph = graph_from(
            r#"
[agent]
name = "g"
[stages.main]
[stages.main.transitions.error_recovery]
condition = "error"
[stages.error_recovery]
"#,
        );
        agent.stages = vec![crate::runstate::StageRecord {
            status: crate::runstate::StageRunStatus::Active,
            entered: true,
            started_at: Some(chrono::Utc::now().timestamp() - 30),
            ..crate::runstate::StageRecord::new("main".to_string(), 0)
        }];

        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("[error]"));
    }

    #[test]
    fn build_output_lines_non_run_state() {
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-nrs", AgentDisplayStatus::Active);
        // is_run_state is false, so content will be empty; there is no run on
        // disk either, so the final-answer fallback finds nothing.
        let (lines, showing_final) = dash.build_output_lines(&agent, true, 80);
        assert!(lines.is_empty());
        assert!(!showing_final);
    }

    #[test]
    fn build_context_lines_with_entries() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-entries", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 4000,
            max_tokens: 8000,
            regions: vec![runstate::RegionSnapshot {
                name: "system".to_string(),
                kind: "pinned".to_string(),
                current_tokens: 2000,
                max_tokens: 4000,
                entries: vec![runstate::RegionEntrySnapshot {
                    content: "Hello world".to_string(),
                    tokens: 5,
                    kind: Default::default(),
                    metadata: None,
                    key: None,
                    taint: Default::default(),
                }],
                description: None,
            }],
        }));
        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("entry 1"));
    }

    #[test]
    fn build_context_lines_old_run_without_entries() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-old", AgentDisplayStatus::Active);
        // Has tokens but no entries = old run
        agent.context_snapshot = Some(std::sync::Arc::new(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 4000,
            max_tokens: 8000,
            regions: vec![runstate::RegionSnapshot {
                name: "system".to_string(),
                kind: "pinned".to_string(),
                current_tokens: 2000,
                max_tokens: 4000,
                entries: vec![],
                description: None,
            }],
        }));
        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("predates"));
    }

    // ─── build_context_lines: incoming edges in graph ─────────────────────

    #[test]
    fn build_context_lines_with_graph_info_and_incoming_edges() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-incoming", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(make_context_snapshot(4000, 8000)));
        // "plan" has an edge to "implement", which is the selected stage.
        agent.graph = graph_from(
            r#"
[agent]
name = "g"
[stages.plan]
[stages.plan.transitions.implement]
transform = "clear"
[stages.implement]
"#,
        );
        agent.stages = vec![crate::runstate::StageRecord {
            status: crate::runstate::StageRunStatus::Active,
            entered: true,
            prompt_tokens: 100,
            completion_tokens: 50,
            started_at: Some(chrono::Utc::now().timestamp() - 30),
            ..crate::runstate::StageRecord::new("implement".to_string(), 1)
        }];
        // selected_stage = 0, so we look up index 0 in stages which is "implement"
        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        // Should show stage info
        assert!(!lines.is_empty());
        // The incoming edge from "plan" should be listed, with its transform.
        assert!(text.contains("← plan"));
        assert!(text.contains("[transform: clear]"));
    }

    #[test]
    fn build_context_lines_with_terminal_stage_no_edges() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-terminal", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(make_context_snapshot(4000, 8000)));
        // "plan" has no outgoing edges (terminal)
        agent.graph =
            graph_from("[agent]\nname = \"g\"\n[stages.plan]\n[stages.plan.transitions]\n");
        agent.stages = vec![crate::runstate::StageRecord {
            status: crate::runstate::StageRunStatus::Complete,
            entered: true,
            prompt_tokens: 100,
            completion_tokens: 50,
            started_at: Some(chrono::Utc::now().timestamp() - 60),
            ended_at: Some(chrono::Utc::now().timestamp() - 10),
            ..crate::runstate::StageRecord::new("plan".to_string(), 0)
        }];
        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("terminal"));
    }

    #[test]
    fn build_context_lines_skips_the_transition_block_for_a_linear_blueprint() {
        // A linear blueprint's chain is a graph too, but the block is for
        // ones that branch: no "Stage:" header here.
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-noedge", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(make_context_snapshot(4000, 8000)));
        agent.graph = graph_from("[agent]\nname = \"g\"\n[stages.main]\n[stages.next]\n");
        agent.stages = vec![crate::runstate::StageRecord {
            status: crate::runstate::StageRunStatus::Active,
            entered: true,
            prompt_tokens: 100,
            completion_tokens: 50,
            started_at: Some(chrono::Utc::now().timestamp() - 30),
            ..crate::runstate::StageRecord::new("main".to_string(), 0)
        }];
        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(!text.contains("Stage:"), "{text}");
        assert!(!text.contains("Transitions"), "{text}");
    }

    // ─── build_context_lines: multiple region kinds ───────────────────────

    #[test]
    fn build_context_lines_all_region_kinds() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-kinds", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 6000,
            max_tokens: 8000,
            regions: vec![
                runstate::RegionSnapshot {
                    name: "system".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                    description: None,
                },
                runstate::RegionSnapshot {
                    name: "conv".to_string(),
                    kind: "sliding".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                    description: None,
                },
                runstate::RegionSnapshot {
                    name: "hist".to_string(),
                    kind: "history".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                    description: None,
                },
                runstate::RegionSnapshot {
                    name: "temp".to_string(),
                    kind: "temporary".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                    description: None,
                },
                runstate::RegionSnapshot {
                    name: "cls".to_string(),
                    kind: "clearable".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                    description: None,
                },
                runstate::RegionSnapshot {
                    name: "brain".to_string(),
                    kind: "custom".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                    description: None,
                },
                runstate::RegionSnapshot {
                    name: "other".to_string(),
                    kind: "unknown_kind".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                    description: None,
                },
            ],
        }));
        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        assert!(!lines.is_empty());
    }

    // ─── build_context_lines: 90%+ bar color (error red) ─────────────────

    #[test]
    fn build_context_lines_high_usage_region() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-hiuse", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 7500,
            max_tokens: 8000,
            regions: vec![runstate::RegionSnapshot {
                name: "system".to_string(),
                kind: "pinned".to_string(),
                current_tokens: 7500,
                max_tokens: 8000,
                entries: vec![],
                description: None,
            }],
        }));
        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        assert!(!lines.is_empty());
    }

    // ─── build_context_lines: 70-90% bar (warning yellow) ────────────────

    #[test]
    fn build_context_lines_medium_usage_region() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-meduse", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 5800,
            max_tokens: 8000,
            regions: vec![runstate::RegionSnapshot {
                name: "system".to_string(),
                kind: "compacting".to_string(),
                current_tokens: 5800,
                max_tokens: 8000,
                entries: vec![],
                description: None,
            }],
        }));
        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        assert!(!lines.is_empty());
    }

    // ─── build_context_lines: zero usage region ───────────────────────────

    #[test]
    fn build_context_lines_zero_usage_region() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-zerouse", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 0,
            max_tokens: 8000,
            regions: vec![runstate::RegionSnapshot {
                name: "empty".to_string(),
                kind: "sliding".to_string(),
                current_tokens: 0,
                max_tokens: 8000,
                entries: vec![],
                description: None,
            }],
        }));
        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        assert!(!lines.is_empty());
    }

    // ─── render_content_pane: with active search producing matches ─────────

    #[test]
    fn render_content_pane_search_with_matches() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        // We need actual content lines to match against - build_output_lines
        // will return empty for non-run-state, so use context mode with a snapshot
        // that has entries containing "hello"
        dash.stage_content_mode = StageContentMode::Context;
        dash.search_query = "token".to_string();
        let mut agent = make_test_agent("run-sm2", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 4000,
            max_tokens: 8000,
            regions: vec![runstate::RegionSnapshot {
                name: "system".to_string(),
                kind: "pinned".to_string(),
                current_tokens: 2000,
                max_tokens: 4000,
                entries: vec![runstate::RegionEntrySnapshot {
                    content: "hello token world".to_string(),
                    tokens: 5,
                    kind: Default::default(),
                    metadata: None,
                    key: None,
                    taint: Default::default(),
                }],
                description: None,
            }],
        }));
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        // Matches present: the title carries the position within them.
        assert!(buf.contains("/token/"), "{buf}");
        assert!(buf.contains("1/3"), "{buf}");
    }

    // ─── render_content_pane: scroll at bottom (detail_scroll = 0) ────────

    /// The reported bug: with lines longer than the pane, `Paragraph` wraps
    /// them into more display rows than the logical-line scroll math counted,
    /// and the document's tail was clipped past the pane bottom - at
    /// detail_scroll 0 (bottom / auto-follow) the last line was simply not on
    /// screen. Scrolling now counts display rows, so the bottom is the
    /// bottom.
    #[test]
    fn wrapped_content_shows_its_last_line_at_the_bottom() {
        crate::runstate::with_isolated_runs_dir(
            "wrapped_content_shows_its_last_line_at_the_bottom",
            |_d| {
                let backend = TestBackend::new(50, 14);
                let mut terminal = Terminal::new(backend).unwrap();
                let mut dash = make_test_dashboard();
                dash.stage_content_mode = StageContentMode::Output;
                dash.detail_scroll = 0;
                let agent = setup_run_state_agent_with_logs(
                    "run-wrap-bottom",
                    &[],
                    Some(&format!(
                        "{}\n\n{}\n\nTHE-FINAL-LINE",
                        "wrapping ".repeat(40),
                        "more wrapping text here ".repeat(30),
                    )),
                );
                terminal
                    .draw(|f| dash.render_content_pane(f, Rect::new(0, 0, 48, 14), &agent, 48))
                    .unwrap();
                let screen: String = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                assert!(
                    screen.contains("THE-FINAL-LINE"),
                    "the document tail must be visible at detail_scroll 0:\n{screen}"
                );
            },
        );
    }

    #[test]
    fn wrapped_rows_counts_display_rows_not_logical_lines() {
        let lines = vec![
            Line::from("a".repeat(100)),
            Line::from("short"),
            Line::from(""),
        ];
        // At width 40 the 100-char line wraps to 3 rows: 3 + 1 + 1 = 5.
        assert_eq!(wrapped_rows(&lines, 40), 5);
        // Wide enough for no wrapping: one row per logical line.
        assert_eq!(wrapped_rows(&lines, 120), 3);
        // Degenerate width renders nothing.
        assert_eq!(wrapped_rows(&lines, 0), 0);
    }

    #[test]
    fn render_content_pane_scrollbar_visible_when_overflow() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Context;
        dash.detail_scroll = 5;
        let mut agent = make_test_agent("run-scroll", AgentDisplayStatus::Active);
        // Make snapshot with many entries to exceed screen height
        let entries: Vec<runstate::RegionEntrySnapshot> = (0..50)
            .map(|i| runstate::RegionEntrySnapshot {
                content: format!("content line {}", i),
                tokens: 10,
                kind: Default::default(),
                metadata: None,
                key: None,
                taint: Default::default(),
            })
            .collect();
        agent.context_snapshot = Some(std::sync::Arc::new(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 4000,
            max_tokens: 8000,
            regions: vec![runstate::RegionSnapshot {
                name: "big".to_string(),
                kind: "sliding".to_string(),
                current_tokens: 4000,
                max_tokens: 8000,
                entries,
                description: None,
            }],
        }));
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("\u{2191}"), "{buf}");
    }

    // ─── build_output_lines: logs mode with prefixed lines ────────────────

    #[test]
    fn build_output_lines_logs_mode_non_run_state() {
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-logs-nrs", AgentDisplayStatus::Active);
        // non-run-state, is_output = false
        let (lines, _showing_final) = dash.build_output_lines(&agent, false, 80);
        // Should be empty because no disk content
        assert!(lines.is_empty());
    }

    // ─── render_content_pane: stage_name from stages list ─────────────────

    #[test]
    fn render_content_pane_with_stage_name_in_title() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        let mut agent = make_test_agent("run-sn", AgentDisplayStatus::Active);
        agent.stages = vec![crate::runstate::StageRecord {
            status: crate::runstate::StageRunStatus::Active,
            entered: true,
            ..crate::runstate::StageRecord::new("analyze".to_string(), 0)
        }];
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 10);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Output"), "{buf}");
    }

    // ─── render_content_pane: Logs mode with tool count (run_state) ──────

    #[test]
    fn render_content_pane_logs_mode_run_state() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Logs;
        // is_run_state = true means it tries to read disk, but dir won't exist
        // so it returns empty content gracefully
        let agent = make_test_agent("run-logs-rs", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Logs"), "{buf}");
    }

    // ─── render_context_bar: bar color variants ───────────────────────────

    #[test]
    fn render_context_bar_regions_string_with_many_region_types() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-regions", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 4000,
            max_tokens: 8000,
            regions: vec![
                runstate::RegionSnapshot {
                    name: "s1".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 500,
                    max_tokens: 1000,
                    entries: vec![],
                    description: None,
                },
                runstate::RegionSnapshot {
                    name: "s2".to_string(),
                    kind: "sliding".to_string(),
                    current_tokens: 500,
                    max_tokens: 1000,
                    entries: vec![],
                    description: None,
                },
                runstate::RegionSnapshot {
                    name: "s3".to_string(),
                    kind: "compacting".to_string(),
                    current_tokens: 500,
                    max_tokens: 1000,
                    entries: vec![],
                    description: None,
                },
                runstate::RegionSnapshot {
                    name: "s4".to_string(),
                    kind: "history".to_string(),
                    current_tokens: 500,
                    max_tokens: 1000,
                    entries: vec![],
                    description: None,
                },
                runstate::RegionSnapshot {
                    name: "s5".to_string(),
                    kind: "other".to_string(),
                    current_tokens: 500,
                    max_tokens: 1000,
                    entries: vec![],
                    description: None,
                },
                runstate::RegionSnapshot {
                    name: "s6".to_string(),
                    kind: "more".to_string(),
                    current_tokens: 500,
                    max_tokens: 1000,
                    entries: vec![],
                    description: None,
                },
                runstate::RegionSnapshot {
                    name: "s7".to_string(),
                    kind: "extra".to_string(),
                    current_tokens: 500,
                    max_tokens: 1000,
                    entries: vec![],
                    description: None,
                },
            ],
        }));
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("ctx"), "{buf}");
    }

    // ─── render_context_bar: run_state agent uses stage context ──────────

    #[test]
    fn render_context_bar_run_state_uses_stage_context() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-rs-ctx", AgentDisplayStatus::Active);
        // is_run_state = true, context_snapshot as fallback
        agent.context_snapshot = Some(std::sync::Arc::new(make_context_snapshot(3000, 8000)));
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("ctx"), "{buf}");
    }

    /// Seed the cached history for `run_id` so browsing tests can render an
    /// archived point (`browsed_context_point` reads the cache, keyed by the
    /// selected run).
    fn seed_history(
        dash: &mut crate::commands::dashboard::state::Dashboard,
        run_id: &str,
        points: Vec<leviath_core::run_archive::RunPoint>,
    ) {
        dash.history = Some(crate::commands::dashboard::history::RunHistoryCache {
            run_id: run_id.to_string(),
            visits: crate::commands::dashboard::history::derive_visits(&points),
            points,
            loaded_at_tick: u64::MAX, // never considered stale by the TTL
        });
    }

    /// A one-point context history for the browsing render tests.
    fn one_point_history(
        context: runstate::ContextSnapshot,
    ) -> Vec<leviath_core::run_archive::RunPoint> {
        vec![leviath_core::run_archive::RunPoint {
            meta: leviath_core::run_meta::RunMeta::new(
                "r".to_string(),
                "a".to_string(),
                "/p".to_string(),
                "t".to_string(),
                None,
                "/w".to_string(),
                1,
            ),
            context,
            at: 1,
        }]
    }

    #[test]
    fn render_context_bar_titles_cover_the_fallback_arms() {
        // An index past the cached points still titles with the position;
        // an unrepresentable timestamp renders a blank clock.
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let agent = make_test_agent("run-hist-fallback", AgentDisplayStatus::Active);
        dash.agents.push(agent.clone());
        dash.update_display_indices();
        let mut points = one_point_history(make_context_snapshot(1000, 8000));
        points[0].at = i64::MIN;
        seed_history(&mut dash, "run-hist-fallback", points);

        dash.context_history_idx = Some(0);
        terminal
            .draw(|f| dash.render_context_bar(f, Rect::new(0, 0, 80, 5), &agent))
            .unwrap();

        dash.context_history_idx = Some(9);
        terminal
            .draw(|f| dash.render_context_bar(f, Rect::new(0, 0, 80, 5), &agent))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("point 10/1"), "{text}");
    }

    #[test]
    fn a_tree_cursor_move_scrolls_the_context_view_once() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-follow", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(make_context_snapshot(1000, 8000)));
        dash.agents.push(agent.clone());
        dash.update_display_indices();
        dash.stage_content_mode = StageContentMode::Context;
        dash.context_tree.follow_cursor = true;

        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        assert!(
            !dash.context_tree.follow_cursor,
            "the one-shot follow flag is consumed by the draw"
        );
    }

    #[test]
    fn render_context_bar_shows_history_position_when_browsing() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let agent = make_test_agent("run-hist-bar", AgentDisplayStatus::Active);
        dash.agents.push(agent.clone());
        dash.update_display_indices();
        seed_history(
            &mut dash,
            "run-hist-bar",
            one_point_history(make_context_snapshot(1000, 8000)),
        );
        dash.context_history_idx = Some(0);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("point 1/1"),
            "history position in title: {text}"
        );
    }

    #[test]
    fn build_context_lines_uses_browsed_history_point() {
        let mut dash = make_test_dashboard();
        // No live snapshot on the agent → the browsed point is the only source.
        let agent = make_test_agent("run-hist-lines-xyzzy", AgentDisplayStatus::Active);
        dash.agents.push(agent.clone());
        dash.update_display_indices();
        seed_history(
            &mut dash,
            "run-hist-lines-xyzzy",
            one_point_history(runstate::ContextSnapshot {
                stage_name: "browsed-stage".to_string(),
                total_tokens: 42,
                max_tokens: 100,
                regions: vec![runstate::RegionSnapshot {
                    name: "hist-region".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 42,
                    max_tokens: 100,
                    entries: vec![],
                    description: None,
                }],
            }),
        );
        dash.context_history_idx = Some(0);
        let (lines, _rows) = dash.build_context_lines(&agent, 80);
        let text: String = lines.iter().map(|l| format!("{l:?}")).collect::<String>();
        assert!(text.contains("hist-region"), "browsed region rendered");
    }

    // ─── Clickable chips and tree rows ────────────────────────────────────

    /// A context window with more rows than a short pane can show.
    fn tall_context_agent(id: &str) -> DashboardAgent {
        let entry = |text: &str| runstate::RegionEntrySnapshot {
            content: text.to_string(),
            tokens: 5,
            kind: Default::default(),
            metadata: None,
            key: None,
            taint: Default::default(),
        };
        let mut agent = make_test_agent(id, AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 40,
            max_tokens: 100,
            regions: (0..4)
                .map(|i| runstate::RegionSnapshot {
                    name: format!("region{i}"),
                    kind: "pinned".to_string(),
                    current_tokens: 10,
                    max_tokens: 25,
                    entries: vec![entry("alpha"), entry("beta")],
                    description: None,
                })
                .collect(),
        }));
        agent
    }

    /// Only the rows the viewport is showing become clickable; the ones
    /// scrolled past the bottom register nothing, so a click cannot land on a
    /// row that is not there.
    #[test]
    fn only_the_visible_context_rows_are_clickable() {
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Context;
        let agent = tall_context_agent("run-ctx-click");
        terminal
            .draw(|f| dash.render_content_pane(f, Rect::new(0, 0, 100, 10), &agent, 100))
            .unwrap();
        let rows: Vec<usize> = dash
            .click_targets
            .iter()
            .filter_map(|(_, t)| match t {
                ClickTarget::ContextRow(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert!(!rows.is_empty(), "some rows are on screen");
        assert!(
            rows.len() < 12,
            "four regions with two entries each do not fit in eight rows: {rows:?}"
        );
        // Every registered row sits inside the pane's inner area.
        for (rect, target) in &dash.click_targets {
            if matches!(target, ClickTarget::ContextRow(_)) {
                assert!((1..9).contains(&rect.y), "{rect:?}");
            }
        }
    }

    /// The chips are registered only for the modes the label actually offers,
    /// and only when there is room to draw them.
    #[test]
    fn the_mode_chips_registered_match_the_label() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let agent = make_test_agent("run-chips", AgentDisplayStatus::Active);
        dash.stage_content_mode = StageContentMode::Output;
        terminal
            .draw(|f| dash.render_content_pane(f, Rect::new(0, 0, 100, 20), &agent, 100))
            .unwrap();
        // The pane registers nothing else here (no snapshot, so no tree rows),
        // so the whole registry is the chips.
        let chips: Vec<ClickTarget> = dash.click_targets.iter().map(|(_, t)| *t).collect();
        assert!(chips.contains(&ClickTarget::ContentMode(StageContentMode::Logs)));
        assert!(chips.contains(&ClickTarget::ContentMode(StageContentMode::Context)));
        assert!(
            !chips.contains(&ClickTarget::ContentMode(StageContentMode::Output)),
            "the mode already showing has no chip of its own"
        );

        // A pane too narrow for the title registers nothing rather than
        // buttons over columns that were never drawn. (`draw` clears the
        // registry each frame; this test drives one pane at a time.)
        dash.click_targets.clear();
        let backend = TestBackend::new(6, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| dash.render_content_pane(f, Rect::new(0, 0, 6, 6), &agent, 6))
            .unwrap();
        assert!(dash.click_targets.is_empty(), "no room for a chip");
        // …and neither does a pane with no width at all.
        dash.click_targets.clear();
        terminal
            .draw(|f| dash.render_content_pane(f, Rect::new(0, 0, 1, 6), &agent, 1))
            .unwrap();
        assert!(dash.click_targets.is_empty());
    }

    // ─── render_content_pane: Context mode with is_run_state (disk fallback)

    #[test]
    fn render_content_pane_context_mode_run_state() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Context;
        let agent = make_test_agent("run-ctx-rs", AgentDisplayStatus::Active);
        // No context_snapshot so it shows "no context snapshot available"
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Context"), "{buf}");
    }

    // ─── render_content_pane: file path hint for context mode ─────────────

    #[test]
    fn render_content_pane_file_path_hint_context_mode() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Context;
        let agent = make_test_agent("run-ctx-fph", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Context"), "{buf}");
    }

    // ─── render_content_pane: file path hint for Logs mode ───────────────

    #[test]
    fn render_content_pane_file_path_hint_logs_mode() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Logs;
        let agent = make_test_agent("run-logs-fph", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Logs"), "{buf}");
    }

    // ─── render_content_pane: search with no matches shows 0 matches ──────

    #[test]
    fn render_content_pane_search_no_matches() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Context;
        dash.search_query = "xyznotfound".to_string();
        let mut agent = make_test_agent("run-nm", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(std::sync::Arc::new(make_context_snapshot(4000, 8000)));
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        // The counterpart to the case above: no matches says so, rather than
        // showing a position in an empty set.
        assert!(buf.contains("0 matches"), "{buf}");
    }
}
