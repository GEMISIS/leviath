//! New-run screen rendering: the agent picker, the task editor, and the `@`
//! file-reference popup drawn over it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState,
};

use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;
use crate::commands::dashboard::types::{ClickTarget, NewRunPane};
use crate::tui::widgets::markdown_edit::{MODE_CHORD, MdAction, MdEditView, chord_label};

impl Dashboard {
    pub(in crate::commands::dashboard) fn draw_new_run_screen(
        &mut self,
        frame: &mut Frame,
        area: Rect,
    ) {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(rows[0]);

        self.draw_new_run_agents(frame, panes[0]);
        // The selected blueprint's stage graph sits above the task editor,
        // when the pane has the rows for both; the editor keeps its minimum.
        let preview_h = super::super::new_run_preview::preview_height(panes[1].height);
        let task_area = if preview_h > 0 {
            let right = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(preview_h), Constraint::Min(1)])
                .split(panes[1]);
            self.draw_new_run_preview(frame, right[0]);
            right[1]
        } else {
            panes[1]
        };
        self.draw_new_run_task(frame, task_area);
        // The completion floats over the task pane, so it is drawn after it.
        self.draw_file_ref_popup(frame, task_area);
        self.draw_new_run_help_bar(frame, rows[1]);
    }

    fn draw_new_run_agents(&self, frame: &mut Frame, area: Rect) {
        let visible = self.filtered_new_run_agents();
        let header = Row::new(["Blueprint", "Source", "Description"].into_iter().map(|h| {
            Cell::from(Span::styled(
                h,
                Style::default().add_modifier(Modifier::BOLD),
            ))
        }));
        let rows: Vec<Row> = visible
            .iter()
            .filter_map(|i| self.new_run_agents.get(*i))
            .map(|a| {
                Row::new(vec![
                    Cell::from(a.name.clone()),
                    Cell::from(Span::styled(
                        a.source.clone(),
                        Style::default().fg(source_colour(&a.source)),
                    )),
                    Cell::from(Span::styled(
                        a.description.clone(),
                        Style::default().fg(C_DIM),
                    )),
                ])
            })
            .collect();

        // The filter reads as part of the title, the way the main list's does,
        // so there is no separate line to notice.
        let title = match self.new_run_filter.is_empty() {
            true => format!(" Agent Blueprints ({}) ", visible.len()),
            false => format!(
                " Agent Blueprints  /{}▌  {}/{} ",
                self.new_run_filter,
                visible.len(),
                self.new_run_agents.len()
            ),
        };
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(34),
                Constraint::Percentage(20),
                Constraint::Percentage(46),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(focus_style(self.new_run_focus == NewRunPane::Agents))
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
            state.select(Some(self.new_run_selected.min(visible.len() - 1)));
        }
        frame.render_stateful_widget(table, area, &mut state);

        if visible.is_empty() {
            let hint = match self.new_run_filter.is_empty() {
                true => {
                    "No agent blueprints found. `lev setup` installs the bundled ones.".to_string()
                }
                false => format!("No agent blueprints match \"{}\".", self.new_run_filter),
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(C_DIM)))),
                Rect {
                    x: area.x + 2,
                    y: area.y + 2,
                    width: area.width.saturating_sub(4),
                    height: 1,
                },
            );
        }
    }

    fn draw_new_run_task(&mut self, frame: &mut Frame, area: Rect) {
        // The editor keeps every row but the last, which is the Start button's.
        let (area, button_row) = super::button::editor_and_button_rows(area);
        let focused = self.new_run_focus == NewRunPane::Task;
        let agent = self
            .new_run_selected_agent()
            .map(|a| a.name.clone())
            .unwrap_or_else(|| "no agent blueprint selected".to_string());
        let view = MdEditView::titled(
            Line::from(vec![
                Span::styled(
                    format!(" Task for {agent} "),
                    Style::default()
                        .fg(focus_colour(focused))
                        .add_modifier(Modifier::BOLD),
                ),
                // On the title rather than the help bar: this is the pane
                // you are looking at when you press Enter, and a setting
                // this consequential should not be one line further away
                // than the thing it changes.
                match self.new_run_yolo {
                    true => Span::styled(
                        "[ unattended ] ",
                        Style::default().fg(C_WARN).add_modifier(Modifier::BOLD),
                    ),
                    false => Span::styled("", Style::default()),
                },
            ]),
            focus_colour(focused),
            focused,
        );
        self.new_run_task.render(frame, area, &view);
        // The Start button, right-aligned under the editor. Lit like a focused
        // pane's border when Tab has reached it; a click on it starts the run
        // whether or not it has focus.
        self.draw_action_button(
            frame,
            button_row,
            START_BUTTON,
            self.new_run_focus == NewRunPane::Start,
            ClickTarget::NewRunStart,
        );
    }

    /// The `@` completion, drawn as a floating menu inside the task pane.
    fn draw_file_ref_popup(&self, frame: &mut Frame, area: Rect) {
        if !self.new_run_file_ref {
            return;
        }
        let matches = self.file_ref_matches();
        let selected = self.new_run_file_selected;
        let lines: Vec<Line> = match matches.is_empty() {
            true => vec![Line::from(Span::styled(
                " no matching file ",
                Style::default().fg(C_DIM),
            ))],
            false => matches
                .iter()
                .enumerate()
                .map(|(i, path)| {
                    let style = match i == selected {
                        true => Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                        false => Style::default().fg(C_MUTED),
                    };
                    Line::from(Span::styled(format!(" {path} "), style))
                })
                .collect(),
        };

        // Anchored to the bottom of the pane so it never covers the title, and
        // clamped to the pane so a long list cannot overflow the frame.
        let height = (lines.len() as u16 + 2).min(area.height);
        let popup = Rect {
            x: area.x + 1,
            y: area.y + area.height.saturating_sub(height),
            width: area.width.saturating_sub(2),
            height,
        };
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(C_ACCENT))
                    .title(" files "),
            ),
            popup,
        );
    }

    /// The help bar's text for the current focus. Names the unattended
    /// setting's state, not only its key: the title chip appears only when
    /// it is on, so with it off nothing on the screen said so, and a warning
    /// dialog declined by an Enter meant as "yes" left a person believing
    /// the opposite of what the next run would do.
    pub(in crate::commands::dashboard) fn new_run_help_bar_text(&self) -> String {
        let unattended = match self.new_run_yolo {
            true => "unattended: on",
            false => "unattended: off",
        };
        match (self.new_run_file_ref, self.new_run_focus) {
            (true, _) => " ↑↓ choose · Enter/Tab insert · Esc dismiss ".to_string(),
            (false, NewRunPane::Agents) => format!(
                " ↑↓ select · type to filter · Tab write task · ^Y {unattended} · F1 help · Esc back "
            ),
            // The formatting chord is named on the pane that has the toolbar,
            // and only there: on the picker it would be a key that does
            // nothing.
            (false, NewRunPane::Task) => format!(
                " ^Enter start · Enter newline · Tab Start button · @ file · {} bold · {MODE_CHORD} preview · ^Y {unattended} · F1 help · Esc back ",
                chord_label(MdAction::Bold)
            ),
            (false, NewRunPane::Start) => format!(
                " Enter/Space start the run · Shift+Tab task · Tab agents · ^Y {unattended} · F1 help · Esc back "
            ),
        }
    }

    fn draw_new_run_help_bar(&self, frame: &mut Frame, area: Rect) {
        let hint = self.new_run_help_bar_text();
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(C_DIM)))),
            area,
        );
    }
}

/// The Start button's face. Fixed text, so the click rect is the drawn text.
const START_BUTTON: &str = "[ Start run ]";

/// Border colour for a pane, by whether it holds the keys.
fn focus_style(focused: bool) -> Style {
    Style::default().fg(focus_colour(focused))
}

fn focus_colour(focused: bool) -> Color {
    match focused {
        true => C_BORDER_FOCUS,
        false => C_BORDER,
    }
}

/// Colour an agent's source, so a bundled-but-not-installed row reads as the
/// one that needs a step before it will run.
fn source_colour(source: &str) -> Color {
    match source {
        "bundled" => C_WARN,
        _ => C_ACCENT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::make_test_dashboard;
    use crate::commands::dashboard::types::NewRunAgent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rendered(dash: &mut Dashboard) -> String {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn agent(name: &str, source: &str) -> NewRunAgent {
        NewRunAgent {
            name: name.to_string(),
            source: source.to_string(),
            description: format!("does {name} things"),
            path: format!("/agents/{name}"),
        }
    }

    /// A dashboard on the new-run screen with two agents and three files, and
    /// no toasts to overlay the assertions.
    fn screen() -> Dashboard {
        let mut dash = make_test_dashboard();
        dash.new_run_screen = true;
        dash.new_run_agents = vec![agent("alpha", "installed"), agent("beta", "bundled")];
        dash.new_run_files = vec![
            "README.md".to_string(),
            "src/lib.rs".to_string(),
            "src/main.rs".to_string(),
        ];
        dash
    }

    #[test]
    fn the_screen_lists_agents_with_their_source() {
        let mut dash = screen();
        let out = rendered(&mut dash);
        assert!(out.contains("Agent Blueprints (2)"), "{out}");
        assert!(out.contains("alpha"), "{out}");
        assert!(out.contains("installed"), "{out}");
        assert!(out.contains("bundled"), "{out}");
        assert!(out.contains("Task for alpha"), "{out}");
        assert!(out.contains("type to filter"), "help bar: {out}");
    }

    #[test]
    fn the_filter_shows_in_the_title_with_its_counts() {
        let mut dash = screen();
        dash.new_run_filter = "alph".to_string();
        let out = rendered(&mut dash);
        assert!(out.contains("/alph"), "{out}");
        assert!(out.contains("1/2"), "{out}");
    }

    #[test]
    fn an_empty_catalog_says_how_to_get_agents() {
        let mut dash = make_test_dashboard();
        dash.new_run_screen = true;
        let out = rendered(&mut dash);
        assert!(out.contains("No agent blueprints found"), "{out}");
        assert!(out.contains("lev setup"), "{out}");
        assert!(
            out.contains("no agent blueprint selected"),
            "task title: {out}"
        );
    }

    #[test]
    fn a_filter_matching_nothing_says_so() {
        let mut dash = screen();
        dash.new_run_filter = "zzz".to_string();
        let out = rendered(&mut dash);
        assert!(out.contains("No agent blueprints match"), "{out}");
    }

    #[test]
    fn the_focused_pane_is_the_one_with_the_lit_border() {
        assert_eq!(focus_style(true).fg, Some(C_BORDER_FOCUS));
        assert_eq!(focus_style(false).fg, Some(C_BORDER));
        assert_eq!(source_colour("bundled"), C_WARN);
        assert_eq!(source_colour("installed"), C_ACCENT);
    }

    #[test]
    fn focusing_the_task_pane_changes_the_help_bar() {
        let mut dash = screen();
        dash.new_run_focus = NewRunPane::Task;
        let out = rendered(&mut dash);
        assert!(out.contains("^Enter start"), "{out}");
        assert!(out.contains("Enter newline"), "{out}");
        assert!(out.contains("@ file"), "{out}");
    }

    /// The button is on screen whichever part of the form has the keys, and
    /// lights up only when Tab has reached it; the help bar then names the
    /// two keys that press it.
    #[test]
    fn the_start_button_sits_under_the_task_and_lights_when_focused() {
        use crate::commands::dashboard::test_support::style_at_text;
        let mut dash = screen();
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        let quiet = style_at_text(&terminal, START_BUTTON);
        assert_eq!(quiet.fg, Some(C_BORDER), "unfocused: {quiet:?}");
        let out = rendered(&mut dash);
        assert!(out.contains(START_BUTTON), "{out}");
        assert!(!out.contains("Enter/Space start"), "{out}");

        dash.new_run_focus = NewRunPane::Start;
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        let lit = style_at_text(&terminal, START_BUTTON);
        assert_eq!(lit.bg, Some(C_BORDER_FOCUS), "focused: {lit:?}");
        let out = rendered(&mut dash);
        assert!(out.contains("Enter/Space start"), "help bar: {out}");
        let button = dash
            .click_targets
            .iter()
            .find(|(_, t)| *t == ClickTarget::NewRunStart)
            .map(|(r, _)| *r)
            .expect("the button registered its rect");
        assert_eq!(button.width as usize, START_BUTTON.chars().count());
        assert_eq!(button.x + button.width, 120, "right-aligned to the pane");
    }

    #[test]
    fn the_completion_popup_lists_matching_files() {
        let mut dash = screen();
        dash.new_run_focus = NewRunPane::Task;
        dash.new_run_file_ref = true;
        dash.new_run_file_query = "src".to_string();
        dash.new_run_file_selected = 1;
        let out = rendered(&mut dash);
        assert!(out.contains("src/lib.rs"), "{out}");
        assert!(out.contains("src/main.rs"), "{out}");
        assert!(!out.contains("README.md"), "filtered out: {out}");
        assert!(out.contains("Enter/Tab insert"), "help bar: {out}");
    }

    #[test]
    fn the_completion_popup_says_when_nothing_matches() {
        let mut dash = screen();
        dash.new_run_focus = NewRunPane::Task;
        dash.new_run_file_ref = true;
        dash.new_run_file_query = "nothing-like-this".to_string();
        let out = rendered(&mut dash);
        assert!(out.contains("no matching file"), "{out}");
    }

    #[test]
    fn the_popup_is_clamped_to_a_short_pane() {
        let mut dash = screen();
        dash.new_run_focus = NewRunPane::Task;
        dash.new_run_file_ref = true;
        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        // Drawing inside the frame is the assertion: an unclamped popup panics
        // ratatui's buffer bounds check.
        let out: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(out.contains("files"), "{out}");
    }

    /// The setting is on the pane you are looking at when you press Enter,
    /// and only when it is on.
    #[test]
    fn the_task_pane_says_when_a_run_will_be_unattended() {
        let mut dash = screen();

        // The help bar names the chord either way, so the assertion is on the
        // marker the title carries only while it is armed.
        let quiet = rendered(&mut dash);
        assert!(!quiet.contains("[ unattended ]"), "{quiet}");

        dash.new_run_yolo = true;
        let loud = rendered(&mut dash);
        assert!(loud.contains("[ unattended ]"), "{loud}");
    }
}
