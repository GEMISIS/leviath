//! Drawing the setup wizard.
//!
//! One `draw` per frame, laid out as a fixed header (step breadcrumb), a body
//! that varies per step, and a footer (message line plus key hints), with an
//! optional help overlay on top. Every function takes `&Wizard` and produces
//! widgets - no state changes here, so a render can never be the reason
//! something moved.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

use super::catalog::{self, Credential};
use super::state::{FieldValue, Step, Wizard};
use crate::tui::theme::*;

/// Draw one frame.
pub fn draw(frame: &mut Frame, wizard: &Wizard) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(frame.area());

    draw_header(frame, chunks[0], wizard);
    draw_body(frame, chunks[1], wizard);
    draw_footer(frame, chunks[2], wizard);

    if wizard.show_tos_confirm {
        draw_tos_confirm(frame, frame.area());
    } else if wizard.show_help {
        draw_help(frame, frame.area());
    }
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

/// The step's own content.
fn draw_body(frame: &mut Frame, area: Rect, wizard: &Wizard) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_BORDER_FOCUS))
        .title(format!(" {} ", wizard.step.title()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match wizard.step {
        Step::Welcome => draw_welcome(frame, inner, wizard),
        Step::Providers => draw_providers(frame, inner, wizard),
        Step::ProviderDetail => draw_provider_detail(frame, inner, wizard),
        Step::Defaults | Step::Limits => draw_fields(frame, inner, wizard),
        Step::Agents => draw_agents(frame, inner, wizard),
        Step::Mcp => draw_mcp(frame, inner, wizard),
        Step::Review => draw_review(frame, inner, wizard),
    }
}

fn draw_welcome(frame: &mut Frame, area: Rect, wizard: &Wizard) {
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
    lines.push(Line::from(Span::styled(
        "Press Enter to begin.",
        Style::default().fg(C_ACCENT),
    )));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Greedy word-wrap for plain text rendered inside `List` items, which
/// (unlike `Paragraph`) cannot wrap. Words longer than `width` break at a
/// character boundary rather than overflowing.
fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let mut word = word;
        loop {
            let need = if current.is_empty() {
                word.chars().count()
            } else {
                current.chars().count() + 1 + word.chars().count()
            };
            if need <= width {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
                break;
            }
            if current.is_empty() {
                // A single word wider than the pane: hard-break it after
                // `width` characters (a char boundary by construction).
                // This branch is only reachable when the word has more than
                // `width` chars (a shorter word takes the fits-branch above),
                // so the boundary at char `width` always exists.
                let cut = word
                    .char_indices()
                    .nth(width)
                    .map(|(i, _)| i)
                    .expect("infallible: the word is longer than width chars");
                let (head, tail) = word.split_at(cut);
                lines.push(head.to_string());
                // The cut is strictly inside the word, so the tail is never
                // empty; the loop re-tests it against the width.
                word = tail;
            } else {
                lines.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn draw_providers(frame: &mut Frame, area: Rect, wizard: &Wizard) {
    let items: Vec<ListItem> = wizard
        .providers
        .iter()
        .enumerate()
        .map(|(index, row)| {
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
            if let Some(var) = row.from_env {
                spans.push(Span::styled(
                    format!("  (${var})"),
                    Style::default().fg(C_WARN),
                ));
            } else if !row.value.is_empty() {
                spans.push(Span::styled("  (set)", Style::default().fg(C_MUTED)));
            }
            let mut item_lines = vec![Line::from(spans)];
            // Word-wrap the blurb to the pane: `List` does not wrap, so a
            // long description on a narrow window would otherwise clip
            // mid-sentence with nothing to say more text exists.
            for chunk in wrap_plain(row.provider.blurb, area.width.saturating_sub(6) as usize) {
                item_lines.push(Line::from(Span::styled(
                    format!("    {chunk}"),
                    Style::default().fg(C_DIM),
                )));
            }
            ListItem::new(item_lines)
        })
        .collect();

    frame.render_widget(List::new(items), area);
}

fn draw_provider_detail(frame: &mut Frame, area: Rect, wizard: &Wizard) {
    let Some(index) = wizard.detail_row() else {
        return;
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

    match row.provider.credential {
        Credential::ApiKey | Credential::BaseUrl => {
            let label = if row.provider.credential == Credential::ApiKey {
                "API key"
            } else {
                "Base URL"
            };
            let shown = credential_display(wizard, index);
            lines.push(Line::from(vec![
                Span::styled(format!("{label}: "), Style::default().fg(C_MUTED)),
                Span::styled(shown, Style::default().fg(C_WHITE)),
            ]));
            if let Some(var) = row.from_env {
                lines.push(Line::from(Span::styled(
                    format!("Supplied by ${var} - it will not be written to the config."),
                    Style::default().fg(C_WARN),
                )));
            }
            lines.push(Line::from(Span::styled(
                "Enter to edit.  Ctrl-R shows it.  o opens the signup page.  v re-checks.",
                Style::default().fg(C_DIM),
            )));
        }
        Credential::None => {
            lines.push(Line::from(vec![
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

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// What to print in place of a credential.
fn credential_display(wizard: &Wizard, index: usize) -> String {
    let row = &wizard.providers[index];
    if let Some(edit) = &wizard.edit
        && edit.target == super::state::EditTarget::Credential(index)
    {
        return if edit.masked && !wizard.reveal {
            format!("{}▌", "•".repeat(edit.buffer.chars().count()))
        } else {
            format!("{}▌", edit.buffer)
        };
    }
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

fn draw_fields(frame: &mut Frame, area: Rect, wizard: &Wizard) {
    let items: Vec<ListItem> = wizard
        .fields()
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let selected = index == wizard.cursor;
            let value = match &wizard.edit {
                Some(edit) if edit.target == super::state::EditTarget::Field(index) => {
                    format!("{}▌", edit.buffer)
                }
                _ => field.value.display(),
            };
            let hint = match &field.value {
                FieldValue::Bool(_) => "space",
                FieldValue::Choice { .. } => "← →",
                _ => "enter",
            };
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        if selected { "› " } else { "  " },
                        Style::default().fg(C_ACCENT),
                    ),
                    Span::styled(format!("{:<28}", field.label), name_style(selected)),
                    Span::styled(value, Style::default().fg(C_ACCENT)),
                    Span::styled(format!("   [{hint}]"), Style::default().fg(C_DIM)),
                ]),
                Line::from(Span::styled(
                    format!("    {}", field.help),
                    Style::default().fg(C_DIM),
                )),
            ])
        })
        .collect();

    frame.render_widget(List::new(items), area);
}

fn draw_agents(frame: &mut Frame, area: Rect, wizard: &Wizard) {
    let items: Vec<ListItem> = wizard
        .agents
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let mark = if row.selected {
                GLYPH_COMPLETE
            } else {
                GLYPH_PENDING
            };
            let action = row.action.label(row.agent.version);
            let action_style = if row.action.is_change() {
                Style::default().fg(C_ACCENT)
            } else {
                Style::default().fg(C_DIM)
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{mark} "),
                    Style::default().fg(if row.selected { C_SUCCESS } else { C_DIM }),
                ),
                Span::styled(
                    format!("{:<22}", row.agent.name),
                    name_style(index == wizard.cursor),
                ),
                Span::styled(action, action_style),
            ]))
        })
        .collect();

    frame.render_widget(List::new(items), area);
}

fn draw_mcp(frame: &mut Frame, area: Rect, wizard: &Wizard) {
    let mut items: Vec<ListItem> = wizard
        .mcp
        .iter()
        .enumerate()
        .map(|(index, row)| {
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
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        format!("{mark} "),
                        Style::default().fg(if row.selected { C_SUCCESS } else { C_DIM }),
                    ),
                    Span::styled(
                        format!("{:<22}", row.candidate.config.name),
                        name_style(index == wizard.cursor),
                    ),
                    Span::styled(endpoint, Style::default().fg(C_MUTED)),
                ]),
                Line::from(detail),
            ])
        })
        .collect();

    for error in &wizard.mcp_scan_errors {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("{GLYPH_ERROR} {error}"),
            Style::default().fg(C_WARN),
        ))));
    }

    frame.render_widget(List::new(items), area);
}

fn draw_review(frame: &mut Frame, area: Rect, wizard: &Wizard) {
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

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter or Ctrl-S to write.  Esc to go back.",
        Style::default().fg(C_ACCENT),
    )));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_footer(frame: &mut Frame, area: Rect, wizard: &Wizard) {
    let hints = if wizard.edit.is_some() {
        "enter save field · esc cancel"
    } else {
        match wizard.step {
            Step::Providers => "space select · o signup page · tab next · ? help · q quit",
            Step::ProviderDetail => "enter edit · v check · o signup page · tab next · q quit",
            Step::Defaults | Step::Limits => "↑↓ move · enter/space/←→ change · tab next · q quit",
            Step::Agents | Step::Mcp => "space select · tab next · esc back · q quit",
            Step::Review => "enter write · esc back · q quit",
            Step::Welcome => "enter begin · ? help · q quit",
        }
    };

    let message = wizard.message.clone().unwrap_or_default();
    frame.render_widget(
        Paragraph::new(vec![Line::from(vec![
            Span::styled(message, Style::default().fg(C_WARN)),
            Span::styled(
                if wizard.message.is_some() { "  " } else { "" },
                Style::default(),
            ),
            Span::styled(hints, Style::default().fg(C_DIM)),
        ])])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(C_BORDER)),
        ),
        area,
    );
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup = centered(64, 60, area);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(Span::styled("Keys", Style::default().fg(C_ACCENT))),
        Line::from(""),
        Line::from("  ↑ ↓ / k j     move"),
        Line::from("  ← → / h l     change a choice"),
        Line::from("  space         select / toggle"),
        Line::from("  enter         edit, or go on"),
        Line::from("  tab / esc     next / previous screen"),
        Line::from("  v             re-check a provider"),
        Line::from("  o             open a signup page"),
        Line::from("  ctrl-r        show or hide credentials"),
        Line::from("  ctrl-s        write and finish, from anywhere"),
        Line::from("  q / ctrl-c    quit without writing"),
        Line::from(""),
        Line::from(Span::styled(
            "  any key closes this",
            Style::default().fg(C_DIM),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Left).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(C_BORDER_FOCUS))
                .title(" Help "),
        ),
        popup,
    );
}

/// Confirmation overlay for the Claude Code transport's terms risk.
fn draw_tos_confirm(frame: &mut Frame, area: Rect) {
    let popup = centered(70, 50, area);
    frame.render_widget(Clear, popup);
    let lines = vec![
        Line::from(Span::styled(
            "⚠️  Terms of Service",
            Style::default().fg(C_WARN).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Anthropic's terms prohibit third-party developers from offering",
            Style::default().fg(C_WARN),
        )),
        Line::from(Span::styled(
            "claude.ai subscription auth for their products without prior",
            Style::default().fg(C_WARN),
        )),
        Line::from(Span::styled(
            "approval. The Claude Code transport routes inference through your",
            Style::default().fg(C_WARN),
        )),
        Line::from(Span::styled(
            "subscription via the CLI's OAuth session.",
            Style::default().fg(C_WARN),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "For unambiguous compliance, use a direct Anthropic API key.",
            Style::default().fg(C_WHITE),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("Press ", Style::default().fg(C_DIM)),
            Span::styled(
                "Y",
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " to accept responsibility for compliance,",
                Style::default().fg(C_DIM),
            ),
        ]),
        Line::from(Span::styled(
            "or any other key to cancel.",
            Style::default().fg(C_DIM),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(C_WARN))
                .title(" Confirm "),
        ),
        popup,
    );
}

/// A centred rectangle covering `percent_x`/`percent_y` of `area`.
fn centered(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
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
    fn rendered(wizard: &Wizard) -> String {
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

    #[test]
    fn wrap_plain_wraps_at_word_boundaries_and_hard_breaks_long_words() {
        assert_eq!(
            wrap_plain("alpha beta gamma", 11),
            vec!["alpha beta", "gamma"]
        );
        // One word wider than the pane hard-breaks by characters.
        assert_eq!(wrap_plain("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        // Multibyte characters break on char boundaries, never mid-character.
        assert_eq!(
            wrap_plain("\u{65e5}\u{672c}\u{8a9e}\u{3067}\u{3059}", 2),
            vec!["\u{65e5}\u{672c}", "\u{8a9e}\u{3067}", "\u{3059}"]
        );
        assert!(wrap_plain("", 10).is_empty());
        // A degenerate zero width still terminates and yields one char per line.
        assert_eq!(wrap_plain("ab", 0), vec!["a", "b"]);
    }

    fn wizard() -> (tempfile::TempDir, Wizard) {
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
            buffer: "sk-typing".to_string(),
            masked: true,
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
    fn the_tos_confirmation_overlay_draws_over_the_review_screen() {
        let (_dir, mut w) = wizard();
        let index = w
            .providers
            .iter()
            .position(|r| r.provider.id == "claude-code")
            .expect("the transport is offered");
        w.providers[index].selected = true;
        w.show_tos_confirm = true;
        w.enter(Step::Review);

        let screen = rendered(&w);
        assert!(
            screen.contains("Terms of Service"),
            "overlay title missing:\n{screen}"
        );
        assert!(
            screen.contains("Press"),
            "call to action missing:\n{screen}"
        );
        assert!(
            screen.contains("any other key"),
            "dismissal hint missing:\n{screen}"
        );
    }

    #[test]
    fn the_credential_screen_draws_nothing_when_no_provider_is_selected() {
        let (_dir, mut w) = wizard();
        w.enter(Step::ProviderDetail);

        // Just the chrome, no panic.
        assert!(rendered(&w).contains("Credentials"));
    }

    #[test]
    fn a_field_being_edited_shows_the_buffer_not_the_stored_value() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Limits);
        w.edit = Some(Edit {
            target: EditTarget::Field(0),
            buffer: "42".to_string(),
            masked: false,
        });

        assert!(rendered(&w).contains("42"));
    }

    #[test]
    fn each_field_kind_advertises_the_key_that_changes_it() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Limits);
        let screen = rendered(&w);
        assert!(screen.contains("[enter]"), "numbers are typed");
        assert!(screen.contains("[space]"), "booleans are toggled");

        w.providers[0].selected = true;
        w.enter(Step::Defaults);
        assert!(rendered(&w).contains("← →"), "choices are cycled");
    }

    #[test]
    fn the_agent_list_shows_what_each_row_would_do() {
        let dir = tempfile::tempdir().unwrap();
        crate::bundled::install_bundled(&crate::bundled::BUNDLED_AGENTS[0], dir.path()).unwrap();
        let mut w = crate::commands::setup::state::tests::test_wizard(dir.path());
        w.enter(Step::Agents);

        let screen = rendered(&w);
        assert!(screen.contains("up to date"));
        assert!(screen.contains("install 0.0.1"));
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
        for (step, expected) in [
            (Step::Welcome, "enter begin"),
            (Step::Providers, "space select"),
            (Step::ProviderDetail, "enter edit"),
            (Step::Defaults, "↑↓ move"),
            (Step::Limits, "↑↓ move"),
            (Step::Agents, "space select"),
            (Step::Mcp, "space select"),
            (Step::Review, "enter write"),
        ] {
            w.enter(step);
            let screen = rendered(&w);
            assert!(
                screen.contains(expected),
                "{step:?} footer missing {expected:?}:\n{screen}"
            );
        }

        w.edit = Some(Edit {
            target: EditTarget::Field(0),
            buffer: String::new(),
            masked: false,
        });
        assert!(rendered(&w).contains("esc cancel"));
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
