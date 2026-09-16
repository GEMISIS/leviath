//! Drawing the `lev rage` screen: a header with the four steps, a body per
//! step, and the shared hint bar. Nothing here changes state.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
};

use super::report::PRIVACY_WARNING;
use super::state::{Rage, Step};
use super::{Outcome, human_bytes};
use crate::tui::theme::*;
use crate::tui::widgets::footer::{Hint, draw_hint_bar, hint};

/// How many left-out files the summary lists before saying "and N more".
const SKIPPED_SHOWN: usize = 4;

/// Draw one frame.
pub(crate) fn draw(frame: &mut Frame, ui: &Rage) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(area);
    draw_header(frame, rows[0], ui);
    draw_body(frame, rows[1], ui);
    draw_footer(frame, rows[2], ui);
    if let Some(picker) = &ui.picker {
        picker.draw(frame, area);
    }
}

fn draw_header(frame: &mut Frame, area: Rect, ui: &Rage) {
    let current = ui.step.index();
    let mut spans = vec![Span::styled(
        "lev rage  ",
        Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
    )];
    for (index, title) in Step::TITLES.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" › ", Style::default().fg(C_DIM)));
        }
        let style = if index == current {
            Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)
        } else if index < current {
            Style::default().fg(C_SUCCESS)
        } else {
            Style::default().fg(C_DIM)
        };
        spans.push(Span::styled(*title, style));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(bordered("", C_BORDER)),
        area,
    );
}

fn draw_body(frame: &mut Frame, area: Rect, ui: &Rage) {
    match ui.step {
        Step::About | Step::Run | Step::Agent => draw_intro(frame, area),
        Step::Note => draw_note(frame, area, ui),
        Step::Summary => draw_summary(frame, area, ui),
    }
}

/// The text under the pickers: what the command does, and does not do.
fn draw_intro(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(
            "Pack what a bug report needs into one zip.",
            Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(
            "The zip holds lev doctor, the daemon's state and log, your config with every key",
        ),
        Line::from("removed, your blueprints, and the run you pick. Nothing is uploaded anywhere."),
        Line::from(""),
        Line::from(Span::styled(
            "It keeps your task text, the model's replies, tool output and file contents.",
            Style::default().fg(C_WARN),
        )),
        Line::from(Span::styled(
            "Read it before you share it.",
            Style::default().fg(C_WARN),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(bordered(" lev rage ", C_BORDER)),
        area,
    );
}

fn draw_note(frame: &mut Frame, area: Rect, ui: &Rage) {
    let block = bordered(
        " What happened? What did you expect? (Ctrl-S when done, Esc to go back) ",
        C_BORDER_FOCUS,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(&ui.note, inner);
}

/// The summary is drawn only once the bundle was built or the build failed;
/// the loop hands the terminal back before the build runs.
fn draw_summary(frame: &mut Frame, area: Rect, ui: &Rage) {
    if let Some(outcome) = &ui.outcome {
        draw_outcome(frame, area, outcome);
        return;
    }
    let error = ui.error.clone().unwrap_or_default();
    let lines = vec![
        Line::from(Span::styled(
            "The bundle could not be written.",
            Style::default().fg(C_ERROR).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(error),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(bordered(" Failed ", C_ERROR)),
        area,
    );
}

/// The written bundle: sections, what was left out, and the warning in red.
fn draw_outcome(frame: &mut Frame, area: Rect, outcome: &Outcome) {
    let halves = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(9)])
        .split(area);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Wrote ", Style::default().fg(C_MUTED)),
            Span::styled(
                outcome.zip_path.display().to_string(),
                Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  {}  {} secrets removed",
                    human_bytes(outcome.zip_bytes),
                    outcome.redactions
                ),
                Style::default().fg(C_MUTED),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "{:<22}{:>6}{:>12}{:>12}",
                "Section", "files", "bytes", "redacted"
            ),
            Style::default().fg(C_DIM),
        )),
    ];
    for section in &outcome.sections {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<22}", section.name),
                Style::default().fg(C_ACCENT),
            ),
            Span::raw(format!(
                "{:>6}{:>12}{:>12}",
                section.files,
                human_bytes(section.bytes),
                section.redactions
            )),
        ]));
    }
    // Notes first: rare, and worth more than one more "not present".
    for note in &outcome.notes {
        lines.push(Line::from(Span::styled(
            format!("  note: {note}"),
            Style::default().fg(C_WARN),
        )));
    }
    if !outcome.skipped.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Left out:",
            Style::default().fg(C_DIM),
        )));
        // The pane does not scroll, and a long list of files that were not
        // there would push the warning below it off the screen. The printed
        // summary and `manifest.json` carry the whole list.
        for skipped in outcome.skipped.iter().take(SKIPPED_SHOWN) {
            lines.push(Line::from(vec![
                Span::styled(format!("  {}", skipped.path), Style::default().fg(C_MUTED)),
                Span::styled(format!("  {}", skipped.reason), Style::default().fg(C_DIM)),
            ]));
        }
        if outcome.skipped.len() > SKIPPED_SHOWN {
            lines.push(Line::from(Span::styled(
                format!(
                    "  and {} more, listed in manifest.json",
                    outcome.skipped.len() - SKIPPED_SHOWN
                ),
                Style::default().fg(C_DIM),
            )));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(bordered(" Your bundle ", C_SUCCESS)),
        halves[0],
    );

    let warning = vec![
        Line::from(Span::styled(
            "BEFORE YOU SHARE THIS FILE",
            Style::default().fg(C_ERROR).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(PRIVACY_WARNING),
        Line::from(""),
        Line::from(Span::styled(
            "How to attach it to an issue: https://leviath.dev/docs/reporting-issues",
            Style::default().fg(C_MUTED),
        )),
    ];
    frame.render_widget(
        Paragraph::new(warning)
            .wrap(Wrap { trim: false })
            .block(bordered(" Read this ", C_ERROR)),
        halves[1],
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, ui: &Rage) {
    let hints: Vec<Hint> = match ui.step {
        Step::About | Step::Run | Step::Agent => vec![
            hint("↑↓", "choose"),
            hint("Enter", "select"),
            hint("type", "filter"),
            hint("Esc", "back"),
            hint("Ctrl-C", "quit"),
        ],
        Step::Note => vec![
            hint("Ctrl-S", "build the bundle"),
            hint("Esc", "back"),
            hint("Ctrl-C", "quit"),
        ],
        Step::Summary => vec![hint("Enter", "done"), hint("q", "done")],
    };
    let message = ui.message.as_deref().map(|text| (text, C_WARN));
    draw_hint_bar(frame, area, message, &hints, true);
}

fn bordered(title: &str, color: ratatui::style::Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .title(title.to_string())
}
