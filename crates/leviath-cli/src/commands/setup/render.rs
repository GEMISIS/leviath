//! Drawing the setup wizard.
//!
//! One `draw` per frame, laid out as a fixed header (step breadcrumb), a body
//! that varies per step, and a footer (message line plus key hints), with an
//! optional help overlay on top. Every function takes `&Wizard` and produces
//! widgets - no state changes here, so a render can never be the reason
//! something moved.
//!
//! Every step builds a `Screen`: flat lines, plus the line each selectable
//! row starts on. That shape is what makes the wizard survive a small window.
//! The screens used to be `List`s of pre-sized items and assumed the terminal
//! was tall enough, so the tuning screen's thirteen fields simply stopped at
//! whatever row ran out of pane, with nothing on screen to say more existed.
//! Wrapping happens here too, in `wrap_line`, for the same reason: the
//! number of rows a screen occupies is only knowable once its text is wrapped,
//! and without that number there is nothing to scroll against.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};

mod endpoints;

use super::catalog::{self, Credential};
use super::state::{FieldValue, Step, Wizard};
use crate::tui::theme::*;
use crate::tui::widgets::footer::{Hint, draw_hint_bar, hint};
use crate::tui::widgets::help::{HelpSection, draw_help};

/// The smallest window the wizard will try to draw in. Below this there is no
/// honest layout left, and half a bordered pane reads as a broken program
/// rather than a small one.
const MIN_WIDTH: u16 = 24;
const MIN_HEIGHT: u16 = 6;

/// Draw one frame.
pub(crate) fn draw(frame: &mut Frame, wizard: &Wizard) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "Window too small",
                    Style::default().fg(C_WARN).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!("Need {MIN_WIDTH}x{MIN_HEIGHT}"),
                    Style::default().fg(C_MUTED),
                )),
            ]),
            area,
        );
        return;
    }

    let chunks = body_layout(area);
    if chunks[0].height > 0 {
        draw_header(frame, chunks[0], wizard);
    }
    draw_body(frame, chunks[1], wizard);
    draw_footer(frame, chunks[2], wizard);

    if let Some(pending) = &wizard.confirm {
        pending.dialog.draw(frame, frame.area());
    } else if let Some(picker) = &wizard.picker {
        picker.draw(frame, frame.area());
    } else if wizard.show_help {
        draw_help(frame, frame.area(), &help_sections(), &wizard.help_scroll);
    }
}

/// The help overlay's content, matching the bindings in `input.rs`.
fn help_sections() -> [HelpSection; 5] {
    [
        HelpSection {
            title: "Navigate",
            entries: vec![
                ("↑ ↓ / k j", "move"),
                ("pgup / pgdn", "scroll a page"),
                ("home / end", "first row / the button"),
                ("← → / h l", "change a choice"),
                ("space", "select / toggle"),
                ("enter", "act on the focused row; Continue moves on"),
                ("enter", "on a default, opens a searchable list"),
                ("tab", "next screen"),
                ("shift-tab / esc", "previous screen"),
                ("? / F1", "this help, on every screen"),
            ],
        },
        HelpSection {
            title: "Choosing from a list",
            entries: vec![
                ("type", "search the list"),
                ("↑ ↓ / pgup / pgdn / home / end", "move"),
                ("enter", "choose the highlighted entry"),
                ("esc", "keep what it was"),
            ],
        },
        HelpSection {
            title: "Editing a value",
            entries: vec![
                ("← →", "move the cursor"),
                ("enter", "save"),
                ("esc", "cancel"),
            ],
        },
        HelpSection {
            title: "Anywhere",
            entries: vec![
                ("v", "re-check a credential against the provider"),
                ("o", "open the provider's signup page"),
                ("ctrl-r", "show or hide credentials"),
            ],
        },
        HelpSection {
            title: "Finish",
            entries: vec![
                ("ctrl-s", "write and finish, from anywhere"),
                (
                    "q / ctrl-c",
                    "quit without writing (asks if you changed things)",
                ),
            ],
        },
    ]
}

/// The step's Continue/action button, rendered as the last cursor row.
fn continue_line(wizard: &Wizard) -> Line<'static> {
    button_line(&wizard.continue_label(), wizard.on_continue())
}

/// The step breadcrumb.
fn draw_header(frame: &mut Frame, area: Rect, wizard: &Wizard) {
    let current = wizard.step.index();
    let mut spans = vec![Span::styled(
        "Leviath setup  ",
        Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
    )];
    for (index, step) in Step::ALL.iter().enumerate() {
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
        spans.push(Span::styled(step.title(), style));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(C_BORDER)),
        ),
        area,
    );
}

/// One step's content: flat lines, plus where each selectable row begins.
///
/// The row index is what lets scrolling follow the selection. A `List` tracks
/// that itself, but its items cannot wrap, and a wizard row is a label and a
/// help line that both have to fold on a narrow window.
#[derive(Default)]
struct Screen {
    lines: Vec<Line<'static>>,
    /// First line of each selectable row, in cursor order. The last entry is
    /// always the Continue button, matching [`Wizard::nav_rows`].
    rows: Vec<usize>,
}

impl Screen {
    /// Mark the next line as the start of the next selectable row.
    fn row(&mut self) {
        self.rows.push(self.lines.len());
    }

    fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }

    fn blank(&mut self) {
        self.lines.push(Line::from(""));
    }

    /// Close the screen with its Continue button, which every step has.
    fn finish(mut self, wizard: &Wizard) -> Self {
        self.blank();
        self.row();
        self.push(continue_line(wizard));
        self
    }

    /// Re-flow to `width`, keeping the row markers pointing at the same rows.
    fn wrapped(self, width: usize) -> Self {
        let mut out = Screen::default();
        let mut rows = self.rows.iter().peekable();
        for (index, line) in self.lines.iter().enumerate() {
            while rows.peek().is_some_and(|start| **start == index) {
                rows.next();
                out.row();
            }
            out.lines.extend(wrap_line(line, width));
        }
        out
    }
}

/// The step's own content.
fn draw_body(frame: &mut Frame, area: Rect, wizard: &Wizard) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_BORDER_FOCUS))
        .title(format!(" {} ", wizard.step.title()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    draw_screen(
        frame,
        inner,
        area,
        &build_screen(wizard).wrapped(inner.width as usize),
        wizard,
    );
}

/// Which selectable row, if any, sits under a point in the window.
///
/// This rebuilds the layout the last frame used rather than remembering it.
/// A stored layout would make drawing a state change, and the wizard's one
/// rule is that a render never moves anything; rebuilding costs a screenful of
/// lines on a click, which is not a cost worth trading that rule for.
pub(crate) fn row_at(area: Rect, wizard: &Wizard, column: u16, row: u16) -> Option<usize> {
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return None;
    }
    let chunks = body_layout(area);
    let block = Block::default().borders(Borders::ALL);
    let inner = block.inner(chunks[1]);
    if column < inner.x
        || column >= inner.x + inner.width
        || row < inner.y
        || row >= inner.y + inner.height
    {
        return None;
    }

    let screen = build_screen(wizard).wrapped(inner.width as usize);
    let offset = first_visible(&screen, wizard, inner.height as usize);
    let line = offset + (row - inner.y) as usize;
    // The row that owns this line is the last one starting at or before it,
    // and only if the line is still inside the screen's content.
    if line >= screen.lines.len() {
        return None;
    }
    screen
        .rows
        .iter()
        .rposition(|start| *start <= line)
        .filter(|_| screen.rows.first().is_some_and(|first| *first <= line))
}

/// The header/body/footer split, shared by drawing and hit-testing so a click
/// can never land on a layout the frame did not use.
fn body_layout(area: Rect) -> std::rc::Rc<[Rect]> {
    // The breadcrumb is the first thing to go on a short window. It says where
    // you are, which the body's own title also says, so spending three of
    // twelve rows on it costs more than it tells you.
    let header = if area.height >= 14 { 3 } else { 0 };
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(area)
}

/// The current step's content, before wrapping.
fn build_screen(wizard: &Wizard) -> Screen {
    match wizard.step {
        Step::Welcome => build_welcome(wizard),
        Step::Providers => build_providers(wizard),
        Step::ProviderDetail => build_provider_detail(wizard),
        Step::Defaults | Step::Limits => build_fields(wizard),
        Step::Agents => build_agents(wizard),
        Step::Mcp => build_mcp(wizard),
        Step::Review => build_review(wizard),
    }
}

/// Total width of a styled line, counted the way [`wrap_line`] counts.
fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Split into alternating runs of whitespace and non-whitespace, so wrapping
/// can keep column padding that fits and drop it at a break.
fn runs(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let blank = rest.starts_with(char::is_whitespace);
        let end = rest
            .char_indices()
            .find(|(_, c)| c.is_whitespace() != blank)
            .map_or(rest.len(), |(i, _)| i);
        let (head, tail) = rest.split_at(end);
        out.push(head);
        rest = tail;
    }
    out
}

/// Word-wrap a styled line to `width`, keeping every span's style and
/// indenting continuations to the line's own leading spaces.
///
/// `Paragraph`'s `Wrap` would fold the text, but only inside the widget: the
/// caller never learns how many rows came out, and the wizard needs that
/// number to scroll. Wrapping up front means the row count and the scroll
/// offset are the same units.
fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    if line_width(line) <= width {
        return vec![line.clone()];
    }
    // A hanging indent keeps a wrapped help line reading as one item, but only
    // when it leaves most of the width for text.
    let indent_len = line
        .spans
        .first()
        .map_or(0, |s| s.content.chars().take_while(|c| *c == ' ').count());
    let indent_len = if indent_len * 2 >= width {
        0
    } else {
        indent_len
    };

    /// Close a row, dropping the whitespace it would otherwise end in: the
    /// break stands in for the space it happened at.
    ///
    /// Indexing rather than `last_mut`, because a row is only ever closed with
    /// something on it - `has_word` is what decides to close one.
    fn close_row(spans: &mut Vec<Span<'static>>) -> Line<'static> {
        while spans.len() > 1 && spans[spans.len() - 1].content.trim().is_empty() {
            spans.pop();
        }
        let last = spans.len() - 1;
        spans[last].content = spans[last].content.trim_end().to_string().into();
        Line::from(std::mem::take(spans))
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let mut has_word = false;

    for span in &line.spans {
        for run in runs(&span.content) {
            let blank = run.starts_with(char::is_whitespace);
            // Whitespace that a break already stood in for.
            if blank && !has_word && !out.is_empty() {
                continue;
            }
            let mut piece = run;
            loop {
                let len = piece.chars().count();
                if used + len <= width {
                    current.push(Span::styled(piece.to_string(), span.style));
                    used += len;
                    has_word |= !blank;
                    break;
                }
                if has_word {
                    out.push(close_row(&mut current));
                    used = indent_len;
                    has_word = false;
                    if indent_len > 0 {
                        current.push(Span::raw(" ".repeat(indent_len)));
                    }
                    // The break stands in for the space that did not fit.
                    if blank {
                        break;
                    }
                    continue;
                }
                // A single word wider than the pane, on a line with nothing
                // else on it: hard-break it at a char boundary. `used` is the
                // indent here, which is strictly under `width`, so there is
                // always at least one character of room.
                let cut = piece
                    .char_indices()
                    .nth(width - used)
                    .map(|(i, _)| i)
                    .expect("infallible: the run is longer than the room left");
                let (head, tail) = piece.split_at(cut);
                current.push(Span::styled(head.to_string(), span.style));
                out.push(close_row(&mut current));
                used = indent_len;
                if indent_len > 0 {
                    current.push(Span::raw(" ".repeat(indent_len)));
                }
                piece = tail;
            }
        }
    }
    // Empty when the text ended exactly at a break.
    if !current.is_empty() {
        out.push(close_row(&mut current));
    }
    out
}

/// The first line to show, given where the user last scrolled and where the
/// cursor is.
///
/// The cursor wins. `wizard.scroll` is what the wheel and the page keys move,
/// but a selection the user cannot see is worse than a lost scroll position,
/// so an off-screen cursor pulls the viewport back to it.
fn first_visible(screen: &Screen, wizard: &Wizard, height: usize) -> usize {
    let total = screen.lines.len();
    let max = total.saturating_sub(height);
    let mut offset = wizard.scroll.min(max);
    let Some(&start) = screen.rows.get(wizard.cursor) else {
        return offset;
    };
    // The row runs to the start of the next one, so a two-line field scrolls
    // into view whole rather than showing its label with the help cut off.
    let end = screen
        .rows
        .get(wizard.cursor + 1)
        .copied()
        .unwrap_or(total)
        .max(start + 1);
    if start < offset {
        offset = start;
    } else if end > offset + height {
        offset = end.saturating_sub(height).min(start);
    }
    offset
}

/// Render a built screen into `inner`, with a scrollbar on `outer`'s border
/// when there is more than fits.
fn draw_screen(frame: &mut Frame, inner: Rect, outer: Rect, screen: &Screen, wizard: &Wizard) {
    // At least one row: the floor in `draw` leaves the body three rows and its
    // border takes two.
    let height = inner.height as usize;
    let offset = first_visible(screen, wizard, height);
    frame.render_widget(
        Paragraph::new(screen.lines.clone()).scroll((offset.min(u16::MAX as usize) as u16, 0)),
        inner,
    );

    let total = screen.lines.len();
    if total > height {
        let mut state = ScrollbarState::new(total - height).position(offset);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓")),
            outer.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }
}

fn build_welcome(wizard: &Wizard) -> Screen {
    let configured: Vec<&str> = wizard
        .providers
        .iter()
        .filter(|r| r.selected)
        .map(|r| r.provider.display)
        .collect();
    let pending = wizard.agents.iter().filter(|r| r.selected).count();

    let mut lines = vec![
        Line::from(Span::styled(
            "This sets up providers, defaults, the bundled agents, and any MCP",
            Style::default().fg(C_WHITE),
        )),
        Line::from(Span::styled(
            "servers you already have configured in other tools.",
            Style::default().fg(C_WHITE),
        )),
        Line::from(""),
    ];

    if configured.is_empty() {
        lines.push(Line::from(Span::styled(
            "Nothing is configured yet.",
            Style::default().fg(C_MUTED),
        )));
    } else {
        lines.push(Line::from(vec![
            Span::styled("Already configured: ", Style::default().fg(C_MUTED)),
            Span::styled(configured.join(", "), Style::default().fg(C_SUCCESS)),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled("Blueprints to install: ", Style::default().fg(C_MUTED)),
        Span::styled(pending.to_string(), Style::default().fg(C_WHITE)),
    ]));
    if !wizard.mcp.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                "MCP servers found elsewhere: ",
                Style::default().fg(C_MUTED),
            ),
            Span::styled(wizard.mcp.len().to_string(), Style::default().fg(C_WHITE)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Nothing is written until the last screen.",
        Style::default().fg(C_DIM),
    )));

    Screen {
        lines,
        rows: Vec::new(),
    }
    .finish(wizard)
}

fn build_providers(wizard: &Wizard) -> Screen {
    let mut screen = Screen::default();
    for (index, row) in wizard.providers.iter().enumerate() {
        let mark = if row.selected {
            GLYPH_COMPLETE
        } else {
            GLYPH_PENDING
        };
        let mut spans = vec![
            Span::styled(
                format!("{mark} "),
                Style::default().fg(if row.selected { C_SUCCESS } else { C_DIM }),
            ),
            Span::styled(row.provider.display, name_style(index == wizard.cursor)),
        ];
        let entries = wizard.endpoints_under(row.provider.id).len();
        if let Some(var) = row.from_env {
            spans.push(Span::styled(
                format!("  (${var})"),
                Style::default().fg(C_WARN),
            ));
        } else if !row.value.is_empty() {
            spans.push(Span::styled("  (set)", Style::default().fg(C_MUTED)));
        } else if entries == 1 {
            spans.push(Span::styled("  (1 endpoint)", Style::default().fg(C_MUTED)));
        } else if entries > 1 {
            spans.push(Span::styled(
                format!("  ({entries} endpoints)"),
                Style::default().fg(C_MUTED),
            ));
        }
        screen.row();
        screen.push(Line::from(spans));
        screen.push(Line::from(Span::styled(
            format!("    {}", row.provider.blurb),
            Style::default().fg(C_DIM),
        )));
    }
    screen.finish(wizard)
}

fn build_provider_detail(wizard: &Wizard) -> Screen {
    let Some(index) = wizard.detail_row() else {
        // Forced onto an empty credential screen (tests do): only the button.
        return Screen::default().finish(wizard);
    };
    // `detail_row` yields an index into `providers`, so this is a read rather
    // than a lookup that could miss.
    let row = &wizard.providers[index];
    let position = wizard.detail + 1;
    let total = wizard.selected_providers().len();

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                row.provider.display,
                Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("   {position} of {total}"),
                Style::default().fg(C_DIM),
            ),
        ]),
        Line::from(Span::styled(
            row.provider.blurb,
            Style::default().fg(C_MUTED),
        )),
        Line::from(""),
    ];

    // The credential (or effort) row is the screen's one cursor row; the
    // marker shows whether it or the Continue button holds focus.
    let row_marker = if wizard.on_continue() { "  " } else { "› " };
    let credential_row = lines.len();
    match row.provider.credential {
        Credential::ApiKey | Credential::BaseUrl => {
            let label = if row.provider.credential == Credential::ApiKey {
                "API key"
            } else {
                "Base URL"
            };
            let mut spans = vec![
                Span::styled(row_marker, Style::default().fg(C_ACCENT)),
                Span::styled(format!("{label}: "), Style::default().fg(C_MUTED)),
            ];
            match &wizard.edit {
                Some(edit) if edit.target == super::state::EditTarget::Credential(index) => {
                    spans.extend(edit.line.display_spans(wizard.reveal).spans);
                }
                _ => spans.push(Span::styled(
                    credential_display(wizard, index),
                    Style::default().fg(C_WHITE),
                )),
            }
            lines.push(Line::from(spans));
            if let Some(var) = row.from_env {
                lines.push(Line::from(Span::styled(
                    format!("Supplied by ${var} - it will not be written to the config."),
                    Style::default().fg(C_WARN),
                )));
            }
            lines.push(Line::from(Span::styled(
                "Enter or click to edit.  Ctrl-R shows what you typed.",
                Style::default().fg(C_DIM),
            )));
        }
        // A preset's screen is a form per entry, not this card.
        Credential::Endpoint => return endpoints::build_endpoint_detail(wizard, index),
        Credential::None => {
            lines.push(Line::from(vec![
                Span::styled(row_marker, Style::default().fg(C_ACCENT)),
                Span::styled("Reasoning effort: ", Style::default().fg(C_MUTED)),
                Span::styled(
                    super::state::effort_options()[row.effort],
                    Style::default().fg(C_ACCENT),
                ),
            ]));
            lines.push(Line::from(Span::styled(
                "← / → to change.  Sign in with `claude` if you have not already.",
                Style::default().fg(C_DIM),
            )));
            // The transport is opt-in, so the terms risk has to be on the
            // screen where it is opted into - not only in the README.
            for warning in [
                "⚠️  Anthropic's terms prohibit third-party use of subscription auth",
                "    without prior approval. By enabling this transport you accept",
                "    responsibility for compliance with their terms.",
                "    For unambiguous compliance, use a direct Anthropic API key.",
            ] {
                lines.push(Line::from(Span::styled(
                    warning,
                    Style::default().fg(C_WARN),
                )));
            }
        }
    }

    lines.push(Line::from(""));
    lines.push(status_line(wizard, index));
    lines.push(Line::from(""));

    let mut screen = Screen {
        lines,
        rows: vec![credential_row],
    };
    for (offset, action) in wizard.detail_actions().iter().enumerate() {
        // Row 0 is the credential itself, so the actions start after it.
        let focused = wizard.cursor == offset + 1;
        screen.row();
        screen.push(button_line(&action.label(row.provider.display), focused));
    }
    screen.finish(wizard)
}

/// A clickable action, drawn the same way the Continue button is so that what
/// can be pressed looks like one thing.
fn button_line(label: &str, focused: bool) -> Line<'static> {
    let style = if focused {
        Style::default()
            .fg(C_ACCENT)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(C_MUTED)
    };
    Line::from(vec![
        Span::styled(
            if focused { "› " } else { "  " },
            Style::default().fg(C_ACCENT),
        ),
        Span::styled(format!("[ {label} ]"), style),
    ])
}

/// What to print in place of a credential when it is not being edited.
fn credential_display(wizard: &Wizard, index: usize) -> String {
    let row = &wizard.providers[index];
    if row.value.is_empty() {
        return match row.from_env {
            Some(_) => "(from the environment)".to_string(),
            None => format!("({})", row.provider.hint),
        };
    }
    if row.provider.credential == Credential::ApiKey && !wizard.reveal {
        catalog::redact(&row.value)
    } else {
        row.value.clone()
    }
}

/// The verification result line for one provider.
fn status_line(wizard: &Wizard, index: usize) -> Line<'static> {
    let row = &wizard.providers[index];
    if row.checking {
        let frame = SPINNER[(wizard.ticks as usize) % SPINNER.len()];
        return Line::from(vec![
            Span::styled(format!("{frame} "), Style::default().fg(C_ACCENT)),
            Span::styled("checking…", Style::default().fg(C_MUTED)),
        ]);
    }
    match &row.outcome {
        super::verify::Outcome::Skipped => {
            Line::from(Span::styled("not checked yet", Style::default().fg(C_DIM)))
        }
        super::verify::Outcome::Reachable { .. } => Line::from(vec![
            Span::styled(format!("{GLYPH_COMPLETE} "), Style::default().fg(C_SUCCESS)),
            Span::styled(row.outcome.summary(), Style::default().fg(C_SUCCESS)),
        ]),
        super::verify::Outcome::Failed { .. } => Line::from(vec![
            Span::styled(format!("{GLYPH_ERROR} "), Style::default().fg(C_ERROR)),
            Span::styled(row.outcome.summary(), Style::default().fg(C_ERROR)),
        ]),
    }
}

fn build_fields(wizard: &Wizard) -> Screen {
    let mut screen = Screen::default();
    for (index, field) in wizard.fields().iter().enumerate() {
        let selected = index == wizard.cursor;
        let hint = match &field.value {
            FieldValue::Bool(_) => "enter/space",
            FieldValue::Choice { .. } => "enter/← →",
            _ => "enter",
        };
        let mut spans = vec![
            Span::styled(
                if selected { "› " } else { "  " },
                Style::default().fg(C_ACCENT),
            ),
            Span::styled(format!("{:<28}", field.label), name_style(selected)),
        ];
        match &wizard.edit {
            Some(edit) if edit.target == super::state::EditTarget::Field(index) => {
                spans.extend(edit.line.display_spans(wizard.reveal).spans);
            }
            _ => spans.push(Span::styled(
                field.value.display(),
                Style::default().fg(C_ACCENT),
            )),
        }
        spans.push(Span::styled(
            format!("   [{hint}]"),
            Style::default().fg(C_DIM),
        ));
        screen.row();
        screen.push(Line::from(spans));
        screen.push(Line::from(Span::styled(
            format!("    {}", field.help),
            Style::default().fg(C_DIM),
        )));
    }
    screen.finish(wizard)
}

fn build_agents(wizard: &Wizard) -> Screen {
    let mut screen = Screen::default();
    for (index, row) in wizard.agents.iter().enumerate() {
        let mark = if row.selected {
            GLYPH_COMPLETE
        } else {
            GLYPH_PENDING
        };
        let action = row.action.label(row.agent.version);
        // Dim for "nothing to do", and for a locally edited install too:
        // it is offered, not urged, because reinstalling destroys the edit.
        let action_style = if row.action.preselect() {
            Style::default().fg(C_ACCENT)
        } else {
            Style::default().fg(C_DIM)
        };
        screen.row();
        screen.push(Line::from(vec![
            Span::styled(
                format!("{mark} "),
                Style::default().fg(if row.selected { C_SUCCESS } else { C_DIM }),
            ),
            Span::styled(
                format!("{:<22}", row.agent.name),
                name_style(index == wizard.cursor),
            ),
            Span::styled(action, action_style),
        ]));
    }
    screen.finish(wizard)
}

fn build_mcp(wizard: &Wizard) -> Screen {
    let mut screen = Screen::default();
    for (index, row) in wizard.mcp.iter().enumerate() {
        let mark = if row.selected {
            GLYPH_COMPLETE
        } else {
            GLYPH_PENDING
        };
        let mut detail = vec![Span::styled(
            format!("    from {}", row.source),
            Style::default().fg(C_DIM),
        )];
        if !row.candidate.scope.is_empty() {
            detail.push(Span::styled(
                format!(" · {}", row.candidate.scope),
                Style::default().fg(C_DIM),
            ));
        }
        if row.collides {
            detail.push(Span::styled(
                format!(" · already configured; would be added as {}", row.name),
                Style::default().fg(C_WARN),
            ));
        }
        if !row.candidate.inline_secrets.is_empty() {
            detail.push(Span::styled(
                format!(
                    " · carries a literal secret in {}",
                    row.candidate.inline_secrets.join(", ")
                ),
                Style::default().fg(C_WARN),
            ));
        }
        let endpoint = row
            .candidate
            .config
            .url
            .clone()
            .or_else(|| row.candidate.config.command.clone())
            .unwrap_or_default();
        screen.row();
        screen.push(Line::from(vec![
            Span::styled(
                format!("{mark} "),
                Style::default().fg(if row.selected { C_SUCCESS } else { C_DIM }),
            ),
            Span::styled(
                format!("{:<22}", row.candidate.config.name),
                name_style(index == wizard.cursor),
            ),
            Span::styled(endpoint, Style::default().fg(C_MUTED)),
        ]));
        screen.push(Line::from(detail));
    }

    for error in &wizard.mcp_scan_errors {
        screen.push(Line::from(Span::styled(
            format!("{GLYPH_ERROR} {error}"),
            Style::default().fg(C_WARN),
        )));
    }
    screen.finish(wizard)
}

fn build_review(wizard: &Wizard) -> Screen {
    let mut lines = vec![Line::from(Span::styled(
        "About to write:",
        Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
    ))];
    for change in wizard.review_lines() {
        lines.push(Line::from(vec![
            Span::styled("  • ", Style::default().fg(C_ACCENT)),
            Span::styled(change, Style::default().fg(C_WHITE)),
        ]));
    }

    let secrets = wizard.selected_inline_secrets();
    if !secrets.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "These imported servers carry a credential written out in full, which",
            Style::default().fg(C_WARN),
        )));
        lines.push(Line::from(Span::styled(
            "would be copied into your Leviath config:",
            Style::default().fg(C_WARN),
        )));
        for entry in secrets {
            lines.push(Line::from(Span::styled(
                format!("  • {entry}"),
                Style::default().fg(C_WARN),
            )));
        }
    }

    let failures: Vec<String> = wizard
        .providers
        .iter()
        .filter(|r| r.selected && r.outcome.is_failure())
        .map(|r| format!("{}: {}", r.provider.display, r.outcome.summary()))
        .collect();
    if !failures.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Did not verify (saving anyway is fine):",
            Style::default().fg(C_ERROR),
        )));
        for failure in failures {
            lines.push(Line::from(Span::styled(
                format!("  • {failure}"),
                Style::default().fg(C_ERROR),
            )));
        }
    }

    if wizard
        .providers
        .iter()
        .any(|r| r.selected && r.provider.id == "claude-code")
    {
        lines.push(Line::from(""));
        for warning in [
            "Claude Code transport: Anthropic's terms may prohibit third-party use of",
            "subscription auth without prior approval. By enabling it you accept",
            "responsibility for compliance with their terms.",
        ] {
            lines.push(Line::from(Span::styled(
                warning,
                Style::default().fg(C_WARN),
            )));
        }
    }

    Screen {
        lines,
        rows: Vec::new(),
    }
    .finish(wizard)
}

/// The footer's key hints for the wizard's current mode.
fn footer_hints(wizard: &Wizard) -> Vec<Hint> {
    if wizard.confirm.is_some() {
        return vec![
            hint("←→", "choose"),
            hint("enter", "confirm"),
            hint("esc", "cancel"),
        ];
    }
    if wizard.picker.is_some() {
        return vec![
            hint("type", "search"),
            hint("↑↓", "move"),
            hint("enter/click", "choose"),
            hint("esc", "keep what it was"),
        ];
    }
    if wizard.edit.is_some() {
        return vec![
            hint("enter", "save"),
            hint("esc", "cancel"),
            hint("←→", "move cursor"),
        ];
    }
    // Every step ends the same way: help, save-from-anywhere, quit. Ctrl-R
    // joins where credentials are on screen.
    let tail = |mut hints: Vec<Hint>, credentials: bool| {
        if credentials {
            hints.push(hint("^R", "reveal"));
        }
        hints.push(hint("?", "help"));
        hints.push(hint("^S", "save"));
        hints.push(hint("q", "quit"));
        hints
    };
    match wizard.step {
        Step::Welcome => tail(vec![hint("enter", "begin")], false),
        Step::Providers => tail(
            vec![
                hint("↑↓", "move"),
                hint("space/enter", "select"),
                hint("o", "signup"),
                hint("v", "check"),
                hint("tab", "next"),
            ],
            true,
        ),
        Step::ProviderDetail => tail(
            vec![
                hint("enter", "edit"),
                hint("v", "check"),
                hint("o", "signup"),
                hint("tab", "next"),
                hint("esc", "back"),
            ],
            true,
        ),
        Step::Defaults | Step::Limits => tail(
            vec![
                hint("↑↓", "move"),
                hint("enter", "change"),
                hint("←→", "cycle"),
                hint("tab", "next"),
                hint("esc", "back"),
            ],
            false,
        ),
        Step::Agents | Step::Mcp => tail(
            vec![
                hint("↑↓", "move"),
                hint("space/enter", "select"),
                hint("tab", "next"),
                hint("esc", "back"),
            ],
            false,
        ),
        Step::Review => tail(
            vec![
                hint("enter", "apply"),
                hint("v", "re-check"),
                hint("esc", "back"),
            ],
            true,
        ),
    }
}

fn draw_footer(frame: &mut Frame, area: Rect, wizard: &Wizard) {
    let hints = footer_hints(wizard);
    let message = wizard.message.as_deref().map(|m| (m, C_WARN));
    draw_hint_bar(frame, area, message, &hints, true);
}

/// Highlight style for the row under the cursor.
fn name_style(selected: bool) -> Style {
    if selected {
        Style::default().fg(C_ACTIVE).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(C_WHITE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::setup::state::{Edit, EditTarget, FieldValue, Wizard};
    use crate::config::Config;
    use crate::tui::TestBackendHarness;
    use ratatui::Terminal;

    /// Render one frame and return every non-blank line of the buffer, so a
    /// test can assert on what a user would actually read.
    pub(super) fn rendered(wizard: &Wizard) -> String {
        let mut terminal = Terminal::new(TestBackendHarness::new(140, 44)).unwrap();
        terminal.draw(|frame| draw(frame, wizard)).unwrap();
        terminal.backend().text()
    }

    /// A provider blurb on a narrow window must wrap, not clip: the tail of
    /// the sentence (here the transport's "cannot be disabled" caveat) has to
    /// reach the screen.
    #[test]
    fn narrow_window_wraps_provider_blurbs_instead_of_clipping() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Providers);
        let mut terminal = Terminal::new(TestBackendHarness::new(48, 44)).unwrap();
        terminal.draw(|frame| draw(frame, &w)).unwrap();
        let screen = terminal.backend().text();
        assert!(
            screen.contains("disabled"),
            "the blurb tail must survive a 48-column window:\n{screen}"
        );
    }

    /// The text of one wrapped line, span styles collapsed away.
    fn wrapped_text(line: &Line<'static>, width: usize) -> Vec<String> {
        wrap_line(line, width)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn wrap_line_breaks_at_words_and_hard_breaks_one_that_never_fits() {
        let plain = Line::from("alpha beta gamma");
        assert_eq!(wrapped_text(&plain, 11), ["alpha beta", "gamma"]);
        // Nothing to do when it already fits, and the line comes back whole.
        assert_eq!(wrapped_text(&plain, 40), ["alpha beta gamma"]);
        // One word wider than the pane hard-breaks by characters.
        assert_eq!(
            wrapped_text(&Line::from("abcdefghij"), 4),
            ["abcd", "efgh", "ij"]
        );
        // Multibyte characters break on char boundaries, never mid-character.
        assert_eq!(
            wrapped_text(&Line::from("\u{65e5}\u{672c}\u{8a9e}\u{3067}\u{3059}"), 2),
            ["\u{65e5}\u{672c}", "\u{8a9e}\u{3067}", "\u{3059}"]
        );
        // A degenerate zero width still terminates.
        assert_eq!(wrapped_text(&Line::from("ab"), 0), ["a", "b"]);
        // Padding that fits is kept, and dropped from the end of a row it
        // would otherwise trail off.
        assert_eq!(wrapped_text(&Line::from("ab  cdef"), 4), ["ab", "cdef"]);
        // Whitespace that lands at the start of a continuation is dropped
        // too: the break already stood in for it. Across a span boundary that
        // is a whole run arriving with a row already broken under it.
        assert_eq!(wrapped_text(&Line::from("aaaa  bbbb"), 4), ["aaaa", "bbbb"]);
        assert_eq!(
            wrapped_text(&Line::from(vec![Span::raw("aaaa "), Span::raw(" bbbb")]), 4),
            ["aaaa", "bbbb"]
        );
        // Text ending exactly at a break leaves nothing to close.
        assert_eq!(wrapped_text(&Line::from("aaaa "), 4), ["aaaa"]);
        assert_eq!(wrapped_text(&Line::from("aaaa bb"), 4), ["aaaa", "bb"]);
    }

    /// An indented line whose word cannot fit even after the indent breaks the
    /// word and keeps indenting what follows.
    #[test]
    fn wrap_line_hard_breaks_under_an_indent_it_can_afford() {
        assert_eq!(
            wrapped_text(&Line::from("  abcdefghijklmno"), 10),
            ["  abcdefgh", "  ijklmno"]
        );
    }

    /// An indented help line keeps its indent on every row it folds onto, so a
    /// wrapped help string still reads as belonging to the field above it.
    #[test]
    fn wrap_line_hangs_continuations_under_the_indent() {
        let indented = Line::from("    alpha beta gamma delta");
        assert_eq!(
            wrapped_text(&indented, 14),
            ["    alpha beta", "    gamma", "    delta"]
        );
        // Unless the indent would leave no useful width, in which case the text
        // is worth more than the alignment and continuations start at column 0.
        assert_eq!(
            wrapped_text(&indented, 8),
            ["    alph", "a beta", "gamma", "delta"]
        );
    }

    /// Styles survive the fold, and column padding that no longer fits is
    /// dropped at the break rather than pushed onto the next row.
    #[test]
    fn wrap_line_keeps_styles_and_drops_padding_at_a_break() {
        let line = Line::from(vec![
            Span::styled("name", Style::default().fg(C_WHITE)),
            Span::styled("        ", Style::default()),
            Span::styled("value", Style::default().fg(C_ACCENT)),
        ]);
        let wrapped = wrap_line(&line, 8);
        assert_eq!(
            wrapped
                .iter()
                .map(|l| l
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>())
                .collect::<Vec<_>>(),
            ["name", "value"]
        );
        assert_eq!(wrapped[1].spans[0].style.fg, Some(C_ACCENT));
    }

    /// Draw into a window of a given size and return what it says.
    fn rendered_at(wizard: &Wizard, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackendHarness::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, wizard)).unwrap();
        terminal.backend().text()
    }

    /// The tuning screen has thirteen two-line fields, which is more than most
    /// windows are tall. It used to render as a `List` that simply stopped at
    /// the bottom of the pane, so the last fields and the Continue button were
    /// unreachable with no sign they existed.
    #[test]
    fn a_short_window_can_still_reach_the_last_field_and_the_button() {
        let (_dir, mut w) = wizard();
        w.show_advanced = true;
        w.enter(Step::Limits);

        let top = rendered_at(&w, 90, 20);
        assert!(top.contains("Max concurrent inferences"), "{top}");
        assert!(
            !top.contains("Max bytes one run may write"),
            "the far end of the form is not on the first screenful:\n{top}"
        );
        // The scrollbar is what says there is more, since nothing else can.
        assert!(top.contains('\u{2193}'), "no scrollbar drawn:\n{top}");

        w.scroll_end();
        let bottom = rendered_at(&w, 90, 20);
        assert!(bottom.contains("Max bytes one run may write"), "{bottom}");
        assert!(
            bottom.contains("Continue:"),
            "the button has to be reachable:\n{bottom}"
        );
    }

    /// Paging moves the selection as well as the view, so the two can never
    /// end up looking at different parts of the form.
    #[test]
    fn paging_moves_the_selection_and_the_view_together() {
        let (_dir, mut w) = wizard();
        w.show_advanced = true;
        w.enter(Step::Limits);

        w.scroll_by(Wizard::PAGE);
        assert_eq!(w.cursor, Wizard::PAGE as usize);
        let screen = rendered_at(&w, 90, 20);
        assert!(
            screen.contains("Finished run retention"),
            "the newly selected row has to be on screen:\n{screen}"
        );

        // Moving the selection back up pulls the view with it, even though the
        // scroll offset was left pointing at the far end of the form.
        w.scroll_end();
        w.move_cursor(-100);
        let back = rendered_at(&w, 90, 20);
        assert!(back.contains("Max concurrent inferences"), "{back}");

        w.scroll_home();
        assert_eq!(w.cursor, 0);
        assert!(rendered_at(&w, 90, 20).contains("Max concurrent inferences"));
    }

    /// A cursor past the end of the rows draws the top of the screen rather
    /// than panicking. Nothing in the wizard puts it there, but tests do, and
    /// a render is never the right place to discover an inconsistency.
    #[test]
    fn a_cursor_past_the_last_row_still_draws() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Providers);
        w.cursor = 999;
        let screen = rendered_at(&w, 90, 20);
        assert!(screen.contains("Providers"), "{screen}");
    }

    /// Review has nothing to select, so there the offset moves on its own
    /// rather than being pinned to a cursor that cannot move.
    #[test]
    fn a_screen_with_no_rows_scrolls_by_offset() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Review);
        assert_eq!(w.row_count(), 0);

        w.scroll_by(Wizard::PAGE);
        assert_eq!(w.scroll, Wizard::PAGE as usize);
        w.scroll_by(-Wizard::PAGE * 4);
        assert_eq!(w.scroll, 0, "scrolling up past the top stops at the top");
    }

    /// Below a floor there is no layout left worth drawing, and half a
    /// bordered pane reads as a crash rather than a small window.
    #[test]
    fn a_window_under_the_floor_says_so_instead_of_drawing_wreckage() {
        let (_dir, w) = wizard();
        let tiny = rendered_at(&w, 20, 5);
        assert!(tiny.contains("Window too small"), "{tiny}");

        // Just above the floor it draws, and drops the breadcrumb to spend the
        // rows on content instead.
        let small = rendered_at(&w, 60, 12);
        assert!(small.contains("Get started"), "{small}");
        assert!(
            !small.contains("\u{203a} Providers"),
            "the breadcrumb is what gives way first:\n{small}"
        );
    }

    /// The chooser draws its prose, its search box and its rows, and a filter
    /// that matches nothing says so rather than showing an empty pane.
    #[test]
    fn the_chooser_draws_the_explanation_the_field_had_no_room_for() {
        let (_dir, mut w) = wizard();
        w.providers[0].selected = true;
        w.providers[0].outcome = crate::commands::setup::verify::Outcome::Reachable {
            models: vec!["claude-opus-4".to_string()],
        };
        w.enter(Step::Defaults);
        w.cursor = 1;
        w.open_picker("Default model", w.defaults[1].value.options().to_vec(), 0);

        let screen = rendered(&w);
        assert!(screen.contains("Default model"), "{screen}");
        assert!(
            screen.contains("never sent to a different provider"),
            "the precedence prose is the point of the screen:\n{screen}"
        );
        assert!(screen.contains("claude-opus-4"), "{screen}");
        assert!(screen.contains("Search"), "{screen}");

        // Nothing matching is a state worth naming.
        w.picker.as_mut().expect("open").query =
            crate::tui::widgets::line_edit::LineEdit::new("zzz", false);
        assert!(rendered(&w).contains("Nothing matches that."));
    }

    /// The chooser fills most of the window, so a short one still gets a list
    /// rather than only a heading.
    #[test]
    fn the_chooser_survives_a_short_window() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Defaults);
        w.open_picker(
            "Default provider",
            w.defaults[0].value.options().to_vec(),
            0,
        );

        let screen = rendered_at(&w, 70, 14);
        assert!(screen.contains("anthropic"), "{screen}");
    }

    /// A window too short to hold the chooser's frame has nothing in it to
    /// click, and neither does the space outside its list.
    #[test]
    fn a_window_too_short_for_the_chooser_declines_to_draw_it() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Defaults);
        w.open_picker(
            "Default provider",
            w.defaults[0].value.options().to_vec(),
            0,
        );
        let picker = w.picker.as_ref().expect("open");

        // `draw` refuses to draw anything under its own floor, so this size
        // only reaches the hit test - which a real 5-row terminal can.
        assert_eq!(picker.row_at(Rect::new(0, 0, 60, 5), 3), None);
        // A click outside the list is not a row either.
        assert_eq!(picker.row_at(Rect::new(0, 0, 90, 40), 1), None);
    }

    /// Hit-testing agrees with drawing about what is on screen: nothing below
    /// the last line is a row, and a window under the floor has no rows at all.
    #[test]
    fn a_click_below_the_content_is_not_the_nearest_row() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Welcome);
        let area = Rect::new(0, 0, 90, 40);

        // Welcome is a handful of lines in a forty-row window, so most of the
        // pane is empty space under them.
        assert_eq!(row_at(area, &w, 4, 34), None);
        // And below the floor there is no layout to resolve against.
        assert_eq!(row_at(Rect::new(0, 0, 10, 4), &w, 2, 2), None);
    }

    pub(super) fn wizard() -> (tempfile::TempDir, Wizard) {
        let dir = tempfile::tempdir().unwrap();
        let wizard = crate::commands::setup::state::tests::test_wizard(dir.path());
        (dir, wizard)
    }

    #[test]
    fn every_step_draws_without_panicking_and_names_itself() {
        // The cheapest guard against a layout arm that only blows up on the one
        // screen nobody opened during testing.
        let dir = tempfile::tempdir().unwrap();
        let mut w = Wizard::new(
            Config::default(),
            &|_| None,
            vec![(
                "Claude Code".to_string(),
                crate::commands::setup::import::Candidate {
                    config: leviath_mcp::MCPServerConfig::stdio("fs", "npx", vec![]),
                    scope: "/repo".to_string(),
                    inline_secrets: vec!["API_TOKEN".to_string()],
                },
            )],
            vec!["Zed: unreadable".to_string()],
            dir.path(),
            std::sync::Arc::new(|_| true),
            Default::default(),
        );
        w.providers[0].selected = true;

        for step in Step::ALL {
            w.enter(step);
            let screen = rendered(&w);
            assert!(
                screen.contains(step.title()),
                "{step:?} did not name itself:\n{screen}"
            );
        }
    }

    #[test]
    fn a_tiny_terminal_still_draws() {
        // Layout constraints that assume room can panic on a small window.
        let (_dir, w) = wizard();
        let mut terminal = Terminal::new(TestBackendHarness::new(20, 8)).unwrap();

        assert!(terminal.draw(|frame| draw(frame, &w)).is_ok());
    }

    #[test]
    fn the_welcome_screen_reports_what_is_already_there() {
        let (_dir, mut w) = wizard();
        assert!(rendered(&w).contains("Nothing is configured yet"));

        w.providers[0].selected = true;
        let screen = rendered(&w);
        assert!(screen.contains("Already configured"));
        assert!(screen.contains("Anthropic"));
    }

    #[test]
    fn the_provider_list_marks_where_each_credential_came_from() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Wizard::new(
            Config::default(),
            &|name| (name == "ANTHROPIC_API_KEY").then(|| "sk-ant-env".to_string()),
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
            Default::default(),
        );
        w.providers[1].selected = true;
        w.providers[1].value = "sk-oai".to_string();
        w.enter(Step::Providers);

        let screen = rendered(&w);
        assert!(
            screen.contains("$ANTHROPIC_API_KEY"),
            "the environment source must be visible:\n{screen}"
        );
        assert!(screen.contains("(set)"), "{screen}");
        assert!(!screen.contains("sk-oai"), "a key leaked:\n{screen}");
    }

    #[test]
    fn a_stored_key_is_redacted_until_ctrl_r() {
        let (_dir, mut w) = wizard();
        w.providers[0].selected = true;
        w.providers[0].value = "sk-ant-secret-value-here".to_string();
        w.enter(Step::ProviderDetail);

        let hidden = rendered(&w);
        // Last four characters, not the first eight - see `catalog::redact`.
        assert!(hidden.contains("****here"), "{hidden}");
        assert!(
            !hidden.contains("sk-ant-s"),
            "issuer prefix leaked:\n{hidden}"
        );
        assert!(
            !hidden.contains("secret-value"),
            "the key leaked:\n{hidden}"
        );

        w.reveal = true;
        assert!(rendered(&w).contains("sk-ant-secret-value-here"));
    }

    #[test]
    fn a_key_being_typed_is_masked_until_revealed() {
        let (_dir, mut w) = wizard();
        w.providers[0].selected = true;
        w.enter(Step::ProviderDetail);
        w.edit = Some(Edit {
            target: EditTarget::Credential(0),
            line: crate::tui::widgets::line_edit::LineEdit::new("sk-typing".to_string(), true),
        });

        let hidden = rendered(&w);
        assert!(hidden.contains("•••"), "{hidden}");
        assert!(!hidden.contains("sk-typing"), "the key leaked:\n{hidden}");

        w.reveal = true;
        assert!(rendered(&w).contains("sk-typing"));
    }

    #[test]
    fn an_empty_credential_shows_its_placeholder_or_its_source() {
        let (_dir, mut w) = wizard();
        w.providers[0].selected = true;
        w.enter(Step::ProviderDetail);
        assert!(rendered(&w).contains("sk-ant-..."));

        w.providers[0].from_env = Some("ANTHROPIC_API_KEY");
        let screen = rendered(&w);
        assert!(screen.contains("from the environment"));
        assert!(
            screen.contains("will not be written"),
            "the user must know the key stays where they put it:\n{screen}"
        );
    }

    #[test]
    fn a_base_url_is_never_masked() {
        // It is not a secret, and hiding it would just be annoying.
        let (_dir, mut w) = wizard();
        let ollama = w
            .providers
            .iter()
            .position(|r| r.provider.id == "ollama")
            .expect("ollama is offered");
        w.providers[ollama].selected = true;
        w.providers[ollama].value = "http://box:11434".to_string();
        w.enter(Step::ProviderDetail);

        assert!(rendered(&w).contains("http://box:11434"));
    }

    #[test]
    fn every_verification_state_is_drawn_distinctly() {
        let (_dir, mut w) = wizard();
        w.providers[0].selected = true;
        w.providers[0].value = "sk-ant".to_string();
        w.enter(Step::ProviderDetail);

        assert!(rendered(&w).contains("not checked yet"));

        w.providers[0].checking = true;
        assert!(rendered(&w).contains("checking"));

        w.providers[0].checking = false;
        w.providers[0].outcome = crate::commands::setup::verify::Outcome::Reachable {
            models: vec!["a".into(), "b".into()],
        };
        assert!(rendered(&w).contains("2 models"));

        w.providers[0].outcome = crate::commands::setup::verify::Outcome::Failed {
            message: "rejected - check the key".into(),
        };
        assert!(rendered(&w).contains("rejected"));
    }

    #[test]
    fn the_claude_code_card_shows_its_effort_and_its_caveat() {
        let (_dir, mut w) = wizard();
        let index = w
            .providers
            .iter()
            .position(|r| r.provider.id == "claude-code")
            .expect("the transport is offered");
        w.providers[index].selected = true;
        w.enter(Step::ProviderDetail);

        let screen = rendered(&w);
        assert!(screen.contains("Reasoning effort"));
        assert!(
            screen.contains("email"),
            "the privacy cost must be on screen"
        );
        assert!(
            screen.contains("terms"),
            "the terms-of-service risk must be on screen:\n{screen}"
        );
    }

    #[test]
    fn the_review_screen_warns_about_the_claude_code_terms() {
        let (_dir, mut w) = wizard();
        let index = w
            .providers
            .iter()
            .position(|r| r.provider.id == "claude-code")
            .expect("the transport is offered");
        w.enter(Step::Review);
        assert!(
            !rendered(&w).contains("Claude Code transport: Anthropic"),
            "the warning is only for a setup that enables it"
        );

        w.providers[index].selected = true;
        let screen = rendered(&w);
        assert!(
            screen.contains("Claude Code transport: Anthropic"),
            "{screen}"
        );
        assert!(screen.contains("responsibility for compliance"), "{screen}");
    }

    #[test]
    fn the_tos_confirmation_dialog_draws_over_the_review_screen() {
        let (_dir, mut w) = wizard();
        let index = w
            .providers
            .iter()
            .position(|r| r.provider.id == "claude-code")
            .expect("the transport is offered");
        w.providers[index].selected = true;
        w.enter(Step::Review);
        w.open_tos_confirm();

        let screen = rendered(&w);
        assert!(
            screen.contains("terms of service"),
            "dialog title missing:\n{screen}"
        );
        assert!(
            screen.contains("[ Accept and save ]"),
            "the affirmative button is missing:\n{screen}"
        );
        assert!(
            screen.contains("[ Cancel ]"),
            "the safe button is missing:\n{screen}"
        );
    }

    #[test]
    fn the_quit_and_no_provider_dialogs_draw_their_buttons() {
        let (_dir, mut w) = wizard();
        w.open_quit_confirm();
        let screen = rendered(&w);
        assert!(screen.contains("Quit setup?"), "{screen}");
        assert!(screen.contains("[ Stay ]"), "{screen}");

        w.confirm = None;
        w.open_no_providers_confirm();
        let screen = rendered(&w);
        assert!(screen.contains("No providers selected"), "{screen}");
        assert!(screen.contains("[ Go back ]"), "{screen}");
        assert!(screen.contains("[ Continue anyway ]"), "{screen}");
    }

    #[test]
    fn the_credential_screen_draws_nothing_when_no_provider_is_selected() {
        let (_dir, mut w) = wizard();
        w.enter(Step::ProviderDetail);

        // Just the chrome and the Continue button, no panic.
        assert!(rendered(&w).contains("Credentials"));
    }

    #[test]
    fn the_credential_screen_moves_the_focus_marker_onto_the_continue_button() {
        let (_dir, mut w) = wizard();
        w.providers[0].selected = true;
        w.enter(Step::ProviderDetail);

        // Cursor on the credential row: the row carries the marker.
        assert!(rendered(&w).contains("› API key:"));

        // Cursor on the Continue button: the row loses it, the button gains it.
        w.cursor = w.row_count();
        let screen = rendered(&w);
        assert!(!screen.contains("› API key:"), "{screen}");
        assert!(screen.contains("› [ Continue: Defaults ]"), "{screen}");
    }

    #[test]
    fn a_field_being_edited_shows_the_buffer_not_the_stored_value() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Limits);
        w.edit = Some(Edit {
            target: EditTarget::Field(0),
            line: crate::tui::widgets::line_edit::LineEdit::new("42".to_string(), false),
        });

        assert!(rendered(&w).contains("42"));
    }

    #[test]
    fn each_field_kind_advertises_the_key_that_changes_it() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Limits);
        let screen = rendered(&w);
        assert!(screen.contains("[enter]"), "numbers are typed");
        assert!(screen.contains("[enter/space]"), "booleans are toggled");

        w.providers[0].selected = true;
        w.enter(Step::Defaults);
        assert!(rendered(&w).contains("enter/← →"), "choices are cycled");
    }

    #[test]
    fn the_agent_list_shows_what_each_row_would_do() {
        let dir = tempfile::tempdir().unwrap();
        crate::bundled::install_bundled(&crate::bundled::BUNDLED_AGENTS[0], dir.path()).unwrap();
        let mut w = crate::commands::setup::state::tests::test_wizard(dir.path());
        w.enter(Step::Agents);

        let screen = rendered(&w);
        assert!(screen.contains("up to date"));
        // Read the expected version off the bundled agent rather than spelling
        // it out, so bumping a blueprint does not break this test.
        let not_installed = &crate::bundled::BUNDLED_AGENTS[1];
        assert!(screen.contains(&format!("install {}", not_installed.version)));
    }

    #[test]
    fn the_mcp_list_flags_collisions_scopes_and_inline_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let base = Config {
            mcp_servers: vec![leviath_mcp::MCPServerConfig::stdio("fs", "npx", vec![])],
            ..Config::default()
        };
        let mut candidate = crate::commands::setup::import::Candidate {
            config: leviath_mcp::MCPServerConfig::http("fs", "https://x.test/mcp"),
            scope: "/repo".to_string(),
            inline_secrets: vec!["Authorization".to_string()],
        };
        candidate.config.name = "fs".to_string();
        let mut w = Wizard::new(
            base,
            &|_| None,
            vec![("Cursor".to_string(), candidate)],
            vec!["Zed: couldn't parse this".to_string()],
            dir.path(),
            std::sync::Arc::new(|_| true),
            Default::default(),
        );
        w.enter(Step::Mcp);

        let screen = rendered(&w);
        assert!(screen.contains("from Cursor"));
        assert!(screen.contains("/repo"));
        assert!(screen.contains("already configured"));
        assert!(screen.contains("fs-2"), "the free name is shown");
        assert!(screen.contains("literal secret"));
        assert!(screen.contains("Zed"), "the unreadable source is reported");
        assert!(screen.contains("https://x.test/mcp"));
    }

    #[test]
    fn an_imported_stdio_server_shows_its_command() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Wizard::new(
            Config::default(),
            &|_| None,
            vec![(
                "Codex".to_string(),
                crate::commands::setup::import::Candidate {
                    config: leviath_mcp::MCPServerConfig::stdio("fs", "npx", vec![]),
                    scope: String::new(),
                    inline_secrets: Vec::new(),
                },
            )],
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
            Default::default(),
        );
        w.enter(Step::Mcp);

        assert!(rendered(&w).contains("npx"));
    }

    #[test]
    fn the_review_screen_lists_changes_and_both_kinds_of_warning() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Wizard::new(
            Config::default(),
            &|_| None,
            vec![(
                "Cursor".to_string(),
                crate::commands::setup::import::Candidate {
                    config: leviath_mcp::MCPServerConfig::stdio("fs", "npx", vec![]),
                    scope: String::new(),
                    inline_secrets: vec!["API_TOKEN".to_string()],
                },
            )],
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
            Default::default(),
        );
        w.providers[0].selected = true;
        w.providers[0].value = "sk-ant-x".to_string();
        w.providers[0].outcome = crate::commands::setup::verify::Outcome::Failed {
            message: "rejected - check the key".into(),
        };
        w.enter(Step::Review);

        let screen = rendered(&w);
        assert!(screen.contains("credential set"));
        assert!(screen.contains("written out in full"), "secret warning");
        assert!(screen.contains("API_TOKEN"));
        assert!(screen.contains("Did not verify"));
        assert!(
            screen.contains("saving anyway is fine"),
            "a failed check must not read as a blocker"
        );
    }

    #[test]
    fn the_review_screen_says_when_nothing_would_change() {
        let (_dir, mut w) = wizard();
        for row in w.agents.iter_mut() {
            row.selected = false;
        }
        w.enter(Step::Review);

        assert!(rendered(&w).contains("Nothing would change"));
    }

    #[test]
    fn the_footer_shows_a_message_and_the_right_hints_per_screen() {
        let (_dir, mut w) = wizard();
        w.message = Some("Credentials shown.".to_string());
        assert!(rendered(&w).contains("Credentials shown."));

        w.message = None;
        for (step, expected, credentials) in [
            (Step::Welcome, "enter begin", false),
            (Step::Providers, "space/enter select", true),
            (Step::ProviderDetail, "enter edit", true),
            (Step::Defaults, "enter change", false),
            (Step::Limits, "enter change", false),
            (Step::Agents, "space/enter select", false),
            (Step::Mcp, "space/enter select", false),
            (Step::Review, "enter apply", true),
        ] {
            w.enter(step);
            let screen = rendered(&w);
            assert!(
                screen.contains(expected),
                "{step:?} footer missing {expected:?}:\n{screen}"
            );
            // Help, save-from-anywhere and quit are on every step; reveal
            // where credentials are on screen.
            for global in ["? help", "^S save", "q quit"] {
                assert!(
                    screen.contains(global),
                    "{step:?} footer missing {global:?}:\n{screen}"
                );
            }
            assert_eq!(
                screen.contains("^R reveal"),
                credentials,
                "{step:?}:\n{screen}"
            );
        }

        w.edit = Some(Edit {
            target: EditTarget::Field(0),
            line: crate::tui::widgets::line_edit::LineEdit::new(String::new(), false),
        });
        assert!(rendered(&w).contains("esc cancel"));

        // A dialog swaps the footer for its own answer hints.
        w.edit = None;
        w.open_quit_confirm();
        assert!(rendered(&w).contains("enter confirm"));
    }

    #[test]
    fn the_help_overlay_lists_the_bindings() {
        let (_dir, mut w) = wizard();
        w.show_help = true;

        let screen = rendered(&w);
        assert!(screen.contains("Help"));
        assert!(screen.contains("ctrl-s"));
        assert!(screen.contains("ctrl-r"));
    }

    #[test]
    fn the_breadcrumb_marks_done_current_and_upcoming_steps() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Agents);

        let screen = rendered(&w);
        for step in Step::ALL {
            assert!(
                screen.contains(step.title()),
                "{step:?} missing from header"
            );
        }
    }

    #[test]
    fn a_choice_field_with_an_out_of_range_index_draws_a_placeholder() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Defaults);
        w.defaults[0].value = FieldValue::Choice {
            options: vec!["a".to_string()],
            index: 9,
        };

        assert!(rendered(&w).contains("(none)"));
    }
}
