//! The crate's one confirmation dialog: two explicit buttons, focus on the
//! safe answer by default.
//!
//! This replaces the "`y` accepts, anything else dismisses" pattern, which
//! made a stray keypress a silent cancel - or, focus-inverted, made a stray
//! `y` destructive. Here the user always sees which button is focused,
//! Enter activates only the focused button, and `y`/`n` remain as explicit
//! accelerators.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::popup::{centered, popup_frame};
use crate::tui::theme::{C_DIM, C_ERROR, C_WARN, C_WHITE};

/// What a keypress did to the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmOutcome {
    /// The dialog is still open (focus may have moved).
    Pending,
    /// The user confirmed the action.
    Yes,
    /// The user declined / dismissed.
    No,
}

/// A modal Yes/No question. Construct with [`Confirm::new`]; focus starts on
/// the No button (the safe answer) and [`Confirm::danger`] switches the
/// styling from warning to destructive.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Confirm {
    pub(crate) title: String,
    pub(crate) body: Vec<Line<'static>>,
    pub(crate) yes_label: &'static str,
    pub(crate) no_label: &'static str,
    pub(crate) focus_yes: bool,
    pub(crate) danger: bool,
    /// An optional "and stop asking" box, with its label and whether it is
    /// ticked. `None` means the dialog does not offer one.
    ///
    /// A third button was the other option and is worse: the choice being made
    /// here is not a third answer to the question, it is a note about future
    /// questions, and a row of three buttons hides that difference.
    pub(crate) remember: Option<(&'static str, bool)>,
}

impl Confirm {
    pub(crate) fn new(
        title: impl Into<String>,
        body: Vec<Line<'static>>,
        yes_label: &'static str,
        no_label: &'static str,
    ) -> Self {
        Self {
            title: title.into(),
            body,
            yes_label,
            no_label,
            focus_yes: false,
            danger: false,
            remember: None,
        }
    }

    /// Offer a "stop asking" box, unticked, toggled with Space.
    ///
    /// The caller reads [`Self::remembered`] after a `Yes` to find out whether
    /// it was ticked. Deliberately not part of the outcome: whether to ask
    /// again is a separate decision from the answer, and folding it into the
    /// answer would make every other dialog carry a field it has no use for.
    pub(crate) fn with_remember(mut self, label: &'static str) -> Self {
        self.remember = Some((label, false));
        self
    }

    /// Whether the "stop asking" box is ticked.
    pub(crate) fn remembered(&self) -> bool {
        matches!(self.remember, Some((_, true)))
    }

    /// Style the dialog as destructive (red border, red Yes button).
    pub(crate) fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    /// `←`/`→`/`h`/`l`/Tab move focus between the two buttons; Enter activates
    /// the focused one; `y`/`n` answer directly; Esc declines. Everything else
    /// is ignored - a stray key never answers a confirmation.
    pub(crate) fn handle(&mut self, key: &KeyEvent) -> ConfirmOutcome {
        match key.code {
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Char('h')
            | KeyCode::Char('l') => {
                self.focus_yes = !self.focus_yes;
                ConfirmOutcome::Pending
            }
            KeyCode::Enter => {
                if self.focus_yes {
                    ConfirmOutcome::Yes
                } else {
                    ConfirmOutcome::No
                }
            }
            // Space ticks the box rather than answering, and does nothing
            // at all on a dialog that offers none.
            KeyCode::Char(' ') => {
                if let Some((_, checked)) = &mut self.remember {
                    *checked = !*checked;
                }
                ConfirmOutcome::Pending
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => ConfirmOutcome::Yes,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => ConfirmOutcome::No,
            _ => ConfirmOutcome::Pending,
        }
    }

    /// Draw the dialog centered in `area` (pass the whole frame area).
    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect) {
        let accent = if self.danger { C_ERROR } else { C_WARN };
        // Size the popup to the body: title border + body + blank + buttons.
        let popup = centered(60, 50, area);
        let inner = popup_frame(frame, popup, &self.title, accent);

        let mut lines = self.body.clone();
        if let Some((label, checked)) = self.remember {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(
                    if checked { "[x] " } else { "[ ] " },
                    Style::default().fg(accent),
                ),
                Span::styled(label, Style::default().fg(C_WHITE)),
                Span::styled("  (space)", Style::default().fg(C_DIM)),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(self.button_row(accent));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn button_row(&self, accent: ratatui::style::Color) -> Line<'static> {
        let focused = Style::default()
            .fg(accent)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED);
        let blurred = Style::default().fg(C_WHITE);
        let (no_style, yes_style) = if self.focus_yes {
            (blurred, focused)
        } else {
            (focused, blurred)
        };
        Line::from(vec![
            Span::styled(format!("[ {} ]", self.no_label), no_style),
            Span::styled("   ", Style::default()),
            Span::styled(format!("[ {} ]", self.yes_label), yes_style),
            Span::styled("   ", Style::default()),
            Span::styled(
                "←→ choose · enter confirm · esc cancel",
                Style::default().fg(C_DIM),
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_terminal;
    use crossterm::event::KeyModifiers;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn dialog() -> Confirm {
        Confirm::new(
            "Kill agent?",
            vec![Line::from("run-1 is still active.")],
            "Kill",
            "Cancel",
        )
    }

    #[test]
    fn focus_starts_on_no_and_enter_declines() {
        let mut confirm = dialog();
        assert!(!confirm.focus_yes);
        assert_eq!(confirm.handle(&press(KeyCode::Enter)), ConfirmOutcome::No);
    }

    #[test]
    fn every_focus_movement_key_flips_the_focused_button() {
        for code in [
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Char('h'),
            KeyCode::Char('l'),
        ] {
            let mut confirm = dialog();
            assert_eq!(confirm.handle(&press(code)), ConfirmOutcome::Pending);
            assert!(confirm.focus_yes, "{code:?} should move focus to Yes");
        }
    }

    #[test]
    fn enter_activates_the_focused_button() {
        let mut confirm = dialog();
        confirm.handle(&press(KeyCode::Right));
        assert_eq!(confirm.handle(&press(KeyCode::Enter)), ConfirmOutcome::Yes);
    }

    #[test]
    fn y_and_n_answer_directly_and_esc_declines() {
        assert_eq!(
            dialog().handle(&press(KeyCode::Char('y'))),
            ConfirmOutcome::Yes
        );
        assert_eq!(
            dialog().handle(&press(KeyCode::Char('Y'))),
            ConfirmOutcome::Yes
        );
        assert_eq!(
            dialog().handle(&press(KeyCode::Char('n'))),
            ConfirmOutcome::No
        );
        assert_eq!(
            dialog().handle(&press(KeyCode::Char('N'))),
            ConfirmOutcome::No
        );
        assert_eq!(dialog().handle(&press(KeyCode::Esc)), ConfirmOutcome::No);
    }

    #[test]
    fn stray_keys_never_answer() {
        let mut confirm = dialog();
        for code in [
            KeyCode::Char('x'),
            KeyCode::Char(' '),
            KeyCode::Up,
            KeyCode::F(1),
        ] {
            assert_eq!(confirm.handle(&press(code)), ConfirmOutcome::Pending);
        }
        assert!(!confirm.focus_yes);
    }

    #[test]
    fn draw_shows_title_body_and_both_buttons() {
        let mut terminal = test_terminal();
        let confirm = dialog();
        terminal
            .draw(|frame| confirm.draw(frame, frame.area()))
            .unwrap();

        let text = terminal.backend().text();
        assert!(text.contains(" Kill agent? "));
        assert!(text.contains("run-1 is still active."));
        assert!(text.contains("[ Cancel ]"));
        assert!(text.contains("[ Kill ]"));
    }

    #[test]
    fn draw_renders_the_danger_variant_and_the_yes_focused_state() {
        let mut terminal = test_terminal();
        let mut confirm = dialog().danger();
        confirm.handle(&press(KeyCode::Tab));
        assert!(confirm.focus_yes);
        terminal
            .draw(|frame| confirm.draw(frame, frame.area()))
            .unwrap();
        assert!(terminal.backend().text().contains("[ Kill ]"));
    }

    /// The box is a note about future questions, not an answer to this one:
    /// Space ticks it and leaves the dialog open, and the answer still has to
    /// be given.
    #[test]
    fn the_remember_box_ticks_without_answering() {
        let mut confirm = dialog().with_remember("Don't ask again");
        assert!(!confirm.remembered());

        assert_eq!(
            confirm.handle(&press(KeyCode::Char(' '))),
            ConfirmOutcome::Pending
        );
        assert!(confirm.remembered());
        // And it unticks.
        confirm.handle(&press(KeyCode::Char(' ')));
        assert!(!confirm.remembered());

        confirm.handle(&press(KeyCode::Char(' ')));
        assert_eq!(
            confirm.handle(&press(KeyCode::Char('y'))),
            ConfirmOutcome::Yes
        );
        assert!(confirm.remembered(), "the tick survives the answer");
    }

    /// Space on a dialog that offers no box does nothing at all - it must not
    /// become a second way to answer.
    #[test]
    fn space_is_inert_without_a_box() {
        let mut confirm = dialog();
        assert_eq!(
            confirm.handle(&press(KeyCode::Char(' '))),
            ConfirmOutcome::Pending
        );
        assert!(!confirm.remembered());
    }

    /// The box is on screen, with its state and the key that changes it.
    #[test]
    fn the_remember_box_is_drawn_with_its_state() {
        let mut confirm = dialog().with_remember("Don't ask again this session");
        let mut terminal =
            ratatui::Terminal::new(crate::tui::TestBackendHarness::new(70, 16)).unwrap();
        terminal
            .draw(|frame| confirm.draw(frame, frame.area()))
            .unwrap();
        let text = terminal.backend().text();
        assert!(text.contains("[ ]"), "unticked to start: {text}");
        assert!(text.contains("space"));

        confirm.handle(&press(KeyCode::Char(' ')));
        terminal
            .draw(|frame| confirm.draw(frame, frame.area()))
            .unwrap();
        let text = terminal.backend().text();
        assert!(text.contains("[x]"), "ticked: {text}");
    }
}
