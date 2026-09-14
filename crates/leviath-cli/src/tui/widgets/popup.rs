//! Centered popup geometry and the standard cleared, bordered popup frame.
//!
//! Every overlay in every surface starts the same way: compute a centered
//! rectangle, `Clear` what's underneath, draw a bordered block, and render
//! content into the block's inner area. This module is that shared start.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Clear};

/// A centred rectangle covering `percent_x`/`percent_y` of `area`.
pub(crate) fn centered(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// [`centered`], but never smaller than `min_width` by `min_height` cells
/// (bounded by `area`), for a popup whose content has a fixed minimum shape:
/// a file list with a two-line header and a footer is unreadable at three
/// rows, whatever percentage of a small terminal that is.
pub(crate) fn centered_at_least(
    percent_x: u16,
    percent_y: u16,
    area: Rect,
    min_width: u16,
    min_height: u16,
) -> Rect {
    let fitted = centered(percent_x, percent_y, area);
    if fitted.width >= min_width && fitted.height >= min_height {
        return fitted;
    }
    let width = fitted.width.max(min_width).min(area.width);
    let height = fitted.height.max(min_height).min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Clear `area`, draw a bordered block titled ` {title} ` in `border_color`,
/// and return the inner rect for the caller's content.
pub(crate) fn popup_frame(frame: &mut Frame, area: Rect, title: &str, border_color: Color) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(format!(" {title} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_floor_holds_a_popup_open_on_a_tiny_terminal() {
        let roomy = Rect::new(0, 0, 100, 40);
        assert_eq!(
            centered_at_least(64, 70, roomy, 20, 6),
            centered(64, 70, roomy)
        );
        let tiny = Rect::new(0, 0, 10, 4);
        assert_eq!(centered_at_least(64, 70, tiny, 20, 6), tiny);
        let narrow = Rect::new(2, 3, 60, 8);
        let popup = centered_at_least(10, 10, narrow, 20, 6);
        assert_eq!((popup.width, popup.height), (20, 6));
        assert_eq!((popup.x, popup.y), (22, 4));
    }
    use crate::tui::test_terminal;
    use crate::tui::theme::C_WARN;

    #[test]
    fn centered_produces_a_rect_inside_the_area() {
        let area = Rect::new(0, 0, 100, 40);
        let popup = centered(60, 50, area);

        assert!(popup.width <= 60);
        assert!(popup.height <= 20);
        assert!(popup.x >= 20);
        assert!(popup.y >= 10);
        assert!(popup.right() <= area.right());
        assert!(popup.bottom() <= area.bottom());
    }

    #[test]
    fn popup_frame_draws_the_title_and_returns_the_inner_rect() {
        let mut terminal = test_terminal();
        let mut inner = Rect::default();
        terminal
            .draw(|frame| {
                let popup = centered(60, 50, frame.area());
                inner = popup_frame(frame, popup, "Confirm", C_WARN);
            })
            .unwrap();

        let text = terminal.backend().text();
        assert!(text.contains(" Confirm "));
        // The inner rect sits strictly inside the bordered popup.
        assert!(inner.width > 0 && inner.height > 0);
    }
}
