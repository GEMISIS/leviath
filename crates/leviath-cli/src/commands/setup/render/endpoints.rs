//! The credential screen for an endpoint preset: one small form per entry,
//! then a row to add another.
//!
//! Split from the parent because a preset with two entries is sixteen cursor
//! rows where every other provider has one, and the layout that draws them
//! is its own thing rather than a longer arm of the provider card.

use super::*;
use crate::commands::setup::state::{EditTarget, EndpointCursor, EndpointField, EndpointRow};

/// The credential screen for the endpoint preset at provider row `index`.
pub(super) fn build_endpoint_detail(wizard: &Wizard, index: usize) -> Screen {
    let row = &wizard.providers[index];
    let position = wizard.detail + 1;
    let total = wizard.selected_providers().len();
    let entries = wizard.endpoints_under(row.provider.id);

    let mut screen = Screen::default();
    screen.push(Line::from(vec![
        Span::styled(
            row.provider.display,
            Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("   {position} of {total}"),
            Style::default().fg(C_DIM),
        ),
    ]));
    screen.push(Line::from(Span::styled(
        row.provider.blurb,
        Style::default().fg(C_MUTED),
    )));
    screen.push(Line::from(""));

    for (nth, &entry) in entries.iter().enumerate() {
        push_entry(&mut screen, wizard, index, entry, nth);
    }

    let on_add = wizard.endpoint_cursor(index) == Some(EndpointCursor::Add);
    screen.row();
    screen.push(button_line(
        &format!("Add another {}", row.provider.display),
        on_add,
    ));
    screen.finish(wizard)
}

/// One entry's form: a heading, its fields, its two buttons.
fn push_entry(screen: &mut Screen, wizard: &Wizard, index: usize, entry: usize, nth: usize) {
    let row: &EndpointRow = &wizard.endpoints[entry];
    screen.push(Line::from(vec![
        Span::styled(
            format!("Endpoint {}: ", nth + 1),
            Style::default().fg(C_MUTED),
        ),
        Span::styled(
            row.name.clone(),
            Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
        ),
    ]));
    for field in EndpointField::ALL {
        let focused = wizard.endpoint_cursor(index) == Some(EndpointCursor::Field(entry, field));
        screen.row();
        match field {
            EndpointField::Verify | EndpointField::Remove => {
                screen.push(button_line(&field_display(wizard, row, field), focused));
            }
            _ => {
                screen.push(field_line(wizard, entry, field, focused));
                if focused && !field.help().is_empty() {
                    screen.push(Line::from(Span::styled(
                        format!("    {}", field.help()),
                        Style::default().fg(C_DIM),
                    )));
                }
            }
        }
        if field == EndpointField::DefaultModel {
            screen.push(entry_status_line(wizard, entry));
        }
    }
    screen.push(Line::from(""));
}

/// `› Label: value`, with the editor's buffer in place of the value while
/// this field is being typed into.
fn field_line(wizard: &Wizard, entry: usize, field: EndpointField, focused: bool) -> Line<'static> {
    let row = &wizard.endpoints[entry];
    let marker = if focused { "› " } else { "  " };
    let mut spans = vec![
        Span::styled(marker, Style::default().fg(C_ACCENT)),
        Span::styled(format!("{}: ", field.label()), Style::default().fg(C_MUTED)),
    ];
    let editing = wizard
        .edit
        .as_ref()
        .filter(|edit| edit.target == EditTarget::Endpoint { entry, field });
    match editing {
        Some(edit) => spans.extend(edit.line.display_spans(wizard.reveal).spans),
        None => spans.push(Span::styled(
            field_display(wizard, row, field),
            Style::default().fg(if focused { C_WHITE } else { C_MUTED }),
        )),
    }
    Line::from(spans)
}

/// What a field shows when it is not being edited.
fn field_display(wizard: &Wizard, row: &EndpointRow, field: EndpointField) -> String {
    let or_hint = |value: &str, hint: &str| {
        if value.is_empty() {
            format!("({hint})")
        } else {
            value.to_string()
        }
    };
    match field {
        EndpointField::Name => row.name.clone(),
        EndpointField::BaseUrl => or_hint(&row.base_url, "http://localhost:8080/v1"),
        EndpointField::ApiKey if row.api_key.is_empty() => "(none)".to_string(),
        EndpointField::ApiKey if !wizard.reveal => catalog::redact(&row.api_key),
        EndpointField::ApiKey => row.api_key.clone(),
        EndpointField::Headers => or_hint(&row.headers, "none"),
        EndpointField::Models => {
            let detected = row.outcome.models().len();
            match (row.models.is_empty(), detected) {
                (true, 0) => "(none typed; the check fills this in)".to_string(),
                (true, n) => format!("({n} detected)"),
                (false, _) => row.models.clone(),
            }
        }
        EndpointField::DefaultModel => row
            .default_model
            .clone()
            .unwrap_or_else(|| "(none)".to_string()),
        EndpointField::Verify | EndpointField::Remove => field.label().to_string(),
    }
}

/// The check's result for one entry, drawn like the provider card's.
fn entry_status_line(wizard: &Wizard, entry: usize) -> Line<'static> {
    let row = &wizard.endpoints[entry];
    if row.checking {
        let frame = SPINNER[(wizard.ticks as usize) % SPINNER.len()];
        return Line::from(vec![
            Span::styled(format!("    {frame} "), Style::default().fg(C_ACCENT)),
            Span::styled("checking…", Style::default().fg(C_MUTED)),
        ]);
    }
    match &row.outcome {
        super::super::verify::Outcome::Skipped => Line::from(Span::styled(
            "    not checked yet",
            Style::default().fg(C_DIM),
        )),
        super::super::verify::Outcome::Reachable { models } => {
            let shown: Vec<&str> = models.iter().take(6).map(String::as_str).collect();
            let more = models.len().saturating_sub(shown.len());
            let mut text = format!("{}: {}", row.outcome.summary(), shown.join(", "));
            if more > 0 {
                text.push_str(&format!(" and {more} more"));
            }
            Line::from(vec![
                Span::styled(
                    format!("    {GLYPH_COMPLETE} "),
                    Style::default().fg(C_SUCCESS),
                ),
                Span::styled(text, Style::default().fg(C_SUCCESS)),
            ])
        }
        super::super::verify::Outcome::Failed { .. } => Line::from(vec![
            Span::styled(format!("    {GLYPH_ERROR} "), Style::default().fg(C_ERROR)),
            Span::styled(
                format!(
                    "{}; type the model ids by hand above",
                    row.outcome.summary()
                ),
                Style::default().fg(C_ERROR),
            ),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{rendered, wizard};
    use super::*;
    use crate::commands::setup::verify::Outcome;

    fn open_llama(w: &mut Wizard) -> usize {
        let llama = w
            .providers
            .iter()
            .position(|r| r.provider.id == "llama-cpp")
            .expect("in the table");
        w.add_endpoint(llama);
        w.enter(Step::ProviderDetail);
        llama
    }

    #[test]
    fn the_endpoint_screen_draws_every_field_and_the_add_row() {
        let (_dir, mut w) = wizard();
        open_llama(&mut w);
        let screen = rendered(&w);
        assert!(screen.contains("llama.cpp"), "{screen}");
        assert!(screen.contains("Endpoint 1: llama-cpp"), "{screen}");
        for field in EndpointField::ALL {
            assert!(
                screen.contains(field.label()),
                "{field:?} missing:\n{screen}"
            );
        }
        assert!(screen.contains("http://localhost:8080/v1"), "{screen}");
        assert!(
            screen.contains("(none typed; the check fills this in)"),
            "{screen}"
        );
        assert!(screen.contains("not checked yet"), "{screen}");
        assert!(screen.contains("Add another llama.cpp"), "{screen}");
        // The focused field explains itself; the first row is the name.
        assert!(screen.contains("› Name"), "{screen}");
        assert!(screen.contains(EndpointField::Name.help()), "{screen}");
    }

    #[test]
    fn the_key_is_redacted_until_revealed_and_the_buffer_shows_while_editing() {
        let (_dir, mut w) = wizard();
        open_llama(&mut w);
        w.endpoints[0].api_key = "sk-endpoint-secret".to_string();
        let screen = rendered(&w);
        assert!(!screen.contains("sk-endpoint-secret"), "leaked:\n{screen}");
        assert!(screen.contains("****cret"), "{screen}");
        w.reveal = true;
        assert!(rendered(&w).contains("sk-endpoint-secret"));

        w.cursor = 1;
        w.open_endpoint_editor(0, EndpointField::BaseUrl);
        w.edit.as_mut().unwrap().line =
            crate::tui::widgets::line_edit::LineEdit::new("typing", false);
        assert!(rendered(&w).contains("typing"));
    }

    #[test]
    fn a_check_result_lists_the_models_and_a_failure_points_at_the_models_row() {
        let (_dir, mut w) = wizard();
        open_llama(&mut w);
        w.endpoints[0].outcome = Outcome::Reachable {
            models: (1..=8).map(|n| format!("m{n}")).collect(),
        };
        w.endpoints[0].default_model = Some("m3".to_string());
        let screen = rendered(&w);
        assert!(
            screen.contains("8 models: m1, m2, m3, m4, m5, m6 and 2 more"),
            "{screen}"
        );
        assert!(screen.contains("(8 detected)"), "{screen}");
        assert!(screen.contains("Default model: m3"), "{screen}");

        w.endpoints[0].outcome = Outcome::Failed {
            message: "unreachable - check your network".to_string(),
        };
        w.endpoints[0].models = "typed-one".to_string();
        let screen = rendered(&w);
        assert!(screen.contains("unreachable"), "{screen}");
        assert!(screen.contains("type the model ids by hand"), "{screen}");
        assert!(screen.contains("Models: typed-one"), "{screen}");

        w.endpoints[0].checking = true;
        assert!(rendered(&w).contains("checking"));
    }

    /// The pick-list says how many entries a preset holds, and a short
    /// listing is shown whole.
    #[test]
    fn the_pick_list_counts_entries_and_a_short_listing_is_shown_whole() {
        let (_dir, mut w) = wizard();
        let llama = open_llama(&mut w);
        w.enter(Step::Providers);
        assert!(rendered(&w).contains("llama.cpp  (1 endpoint)"));
        w.add_endpoint(llama);
        assert!(rendered(&w).contains("llama.cpp  (2 endpoints)"));

        w.enter(Step::ProviderDetail);
        w.endpoints[0].outcome = Outcome::Reachable {
            models: vec!["a".to_string(), "b".to_string()],
        };
        let screen = rendered(&w);
        assert!(screen.contains("2 models: a, b"), "{screen}");
        assert!(!screen.contains("more"), "{screen}");
    }

    #[test]
    fn the_focus_marker_reaches_the_buttons_and_the_add_row() {
        let (_dir, mut w) = wizard();
        open_llama(&mut w);
        w.endpoints[0].headers = "X-Org: r".to_string();
        w.cursor = 6;
        let screen = rendered(&w);
        assert!(screen.contains("› [ Check this endpoint ]"), "{screen}");
        assert!(screen.contains("Headers: X-Org: r"), "{screen}");
        w.cursor = 8;
        assert!(rendered(&w).contains("› [ Add another llama.cpp ]"));
    }
}
