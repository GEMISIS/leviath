//! The one push button the dashboard draws: a fixed label, right-aligned on
//! its row, lit when Tab has reached it, and clickable whether or not it has
//! the keys.
//!
//! It exists because two screens need the same thing for the same reason: a
//! long-form box where Enter breaks the line needs a way to submit on a
//! terminal that cannot tell Ctrl+Enter from Enter, and a button is that way.
//! One drawing, so the two cannot drift apart in look or in click geometry.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;
use crate::commands::dashboard::types::ClickTarget;

impl Dashboard {
    /// Draw `label` at the right end of `row` and register `target` on the
    /// cells it covers, so a click lands where the text is drawn.
    pub(super) fn draw_action_button(
        &mut self,
        frame: &mut Frame,
        row: Rect,
        label: &str,
        focused: bool,
        target: ClickTarget,
    ) {
        let style = match focused {
            true => Style::default()
                .fg(Color::Black)
                .bg(C_BORDER_FOCUS)
                .add_modifier(Modifier::BOLD),
            false => Style::default().fg(C_BORDER).add_modifier(Modifier::BOLD),
        };
        let width = (label.chars().count() as u16).min(row.width);
        let rect = Rect {
            x: row.x + row.width - width,
            y: row.y,
            width,
            height: row.height.min(1),
        };
        frame.render_widget(Paragraph::new(Span::styled(label.to_string(), style)), rect);
        self.register_click(rect, target);
    }
}

/// Split a pane into the rows an editor keeps and the one row its button
/// takes, so every box-with-a-button lays out the same way.
pub(super) fn editor_and_button_rows(area: Rect) -> (Rect, Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    (rows[0], rows[1])
}
