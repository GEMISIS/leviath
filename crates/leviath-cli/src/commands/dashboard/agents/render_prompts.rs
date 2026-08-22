//! Drawing the prompts overlay: the two prompt boxes stacked full screen,
//! the focused one framed in the focus colour, plus the small name popups
//! (new stage, new region) the editor asks with.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::super::state::Dashboard;
use super::super::theme::*;
use super::editor::Overlay;
use super::prompts::PromptFocus;
use crate::tui::widgets::line_edit::LineEdit;
use crate::tui::widgets::markdown_edit::{MarkdownEdit, MdEditView};
use crate::tui::widgets::popup::{centered, popup_frame};

impl Dashboard {
    /// The prompts overlay, if it is open. Returns whether it drew.
    pub(super) fn draw_editor_prompts(&mut self, frame: &mut Frame, area: Rect) -> bool {
        let Some(Overlay::Prompts(prompts)) = self.editor().overlay.as_mut() else {
            return false;
        };
        // The hint bar on the last row stays: it says what the keys do here.
        let area = Rect {
            height: area.height.saturating_sub(1),
            ..area
        };
        frame.render_widget(Clear, area);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Percentage(60),
                Constraint::Min(3),
            ])
            .split(area);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" Prompts · {}", prompts.stage),
                    Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " · what the stage is told, and how it decides where to go next",
                    Style::default().fg(C_DIM),
                ),
            ])),
            rows[0],
        );
        let focus = prompts.focus;
        prompt_box(
            frame,
            rows[1],
            &mut prompts.system,
            " System prompt · what this stage does ",
            focus == PromptFocus::System,
        );
        prompt_box(
            frame,
            rows[2],
            &mut prompts.transition,
            " Transition prompt · how it picks the next path (only read with more than one) ",
            focus == PromptFocus::Transition,
        );
        true
    }

    /// The add-stage and add-region name prompts, if one is open. Returns
    /// whether it drew.
    pub(super) fn draw_editor_name_popups(&mut self, frame: &mut Frame, area: Rect) -> bool {
        let editor = self.editor();
        if let Some(line) = &editor.add_stage {
            name_popup(
                frame,
                area,
                line,
                "New stage",
                "Letters, digits, . _ - · Enter adds it after the selected stage",
            );
            return true;
        }
        if let Some(line) = &editor.add_region {
            name_popup(
                frame,
                area,
                line,
                "New region",
                "Letters, digits, . _ - · a pinned region with a 5% budget to start",
            );
            return true;
        }
        false
    }
}

/// One prompt box: framed, the focused one in the focus colour with a
/// visible cursor and a lit toolbar.
fn prompt_box(
    frame: &mut Frame,
    area: Rect,
    text: &mut MarkdownEdit,
    title: &'static str,
    focused: bool,
) {
    let colour = if focused { C_BORDER_FOCUS } else { C_BORDER };
    text.render(frame, area, &MdEditView::new(title, colour, focused));
}

/// A one-line name prompt with a help line under it.
fn name_popup(frame: &mut Frame, area: Rect, line: &LineEdit, title: &str, help: &str) {
    let popup = centered(50, 20, area);
    let popup = Rect {
        height: popup.height.clamp(3, 5),
        ..popup
    };
    let inner = popup_frame(frame, popup, title, C_BORDER_FOCUS);
    let mut spans = vec![Span::styled("Name  ", Style::default().fg(C_DIM))];
    spans.extend(line.display_spans(true).spans);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(spans),
            Line::from(Span::styled(help, Style::default().fg(C_MUTED))),
        ]),
        inner,
    );
}
