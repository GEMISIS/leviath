//! Tests for `cargo xtask modalities`, driven off fixture JSON so no network
//! is touched.

use super::*;

/// A minimal catalogue body with the fields the parser reads.
fn catalogue() -> String {
    serde_json::json!({
        "data": [
            {
                "id": "anthropic/claude-opus-4.8",
                "architecture": { "input_modalities": ["text", "image", "file"], "output_modalities": ["text"] }
            },
            {
                "id": "openai/gpt-9o",
                "architecture": { "input_modalities": ["image", "text"], "output_modalities": ["text"] }
            },
            {
                "id": "google/gemini-x-image",
                "architecture": { "input_modalities": ["text", "image"], "output_modalities": ["image", "text", "smell"] }
            },
            { "id": "deepseek/deepseek-v4", "architecture": { "input_modalities": ["text"], "output_modalities": ["text"] } },
            { "id": "openai/gpt-free:free", "architecture": { "input_modalities": ["text"], "output_modalities": ["text"] } },
            { "id": "noslash", "architecture": { "input_modalities": ["text"], "output_modalities": ["text"] } },
            { "id": "openai/gpt-embed", "architecture": { "input_modalities": ["text"], "output_modalities": [] } }
        ]
    })
    .to_string()
}

fn fixture_fetch(_url: &str) -> Result<String> {
    Ok(catalogue())
}

fn failing_fetch(_url: &str) -> Result<String> {
    Err(NetworkError("openrouter down".into()).into())
}

fn garbage_fetch(_url: &str) -> Result<String> {
    Ok("not json".into())
}

#[test]
fn mode_parses_write_check_and_rejects_junk() {
    assert_eq!(ModalitiesMode::parse(&[]).unwrap(), ModalitiesMode::Write);
    assert_eq!(
        ModalitiesMode::parse(&["--check".into()]).unwrap(),
        ModalitiesMode::Check
    );
    assert!(ModalitiesMode::parse(&["--nope".into()]).is_err());
}

#[test]
fn the_catalogue_maps_words_and_filters_vendors() {
    let rows = parse_catalogue(&catalogue()).unwrap();
    // Only the three vendors, no colon variants, no slashless ids, no rows
    // with an empty output list.
    assert_eq!(rows.len(), 3);
    // Anthropic's dotted version is normalised to a dash.
    let opus = &rows[&("anthropic".into(), "claude-opus-4-8".into())];
    assert_eq!(opus.input, vec!["text/*", "image/*", "application/pdf"]);
    assert_eq!(opus.output, vec!["text/*"]);
    assert_eq!(opus.source, "openrouter");
    // Words map and reorder into the canonical order; an unknown word is
    // dropped, and inputs keep their canonical order whatever the source order.
    let gem = &rows[&("google".into(), "gemini-x-image".into())];
    assert_eq!(gem.input, vec!["text/*", "image/*"]);
    assert_eq!(gem.output, vec!["text/*", "image/*"]);
    assert!(!rows.contains_key(&("openai".into(), "gpt-embed".into())));
}

#[test]
fn a_table_round_trips_through_render_and_parse() {
    let rows = parse_catalogue(&catalogue()).unwrap();
    let table = Table {
        read_on: "2026-09-11".into(),
        rows,
    };
    let text = render_table(&table);
    assert!(text.starts_with("# Published input and output modalities"));
    let back = parse_table(&text).unwrap();
    assert_eq!(back, table);
}

#[test]
fn a_manual_row_is_kept_and_a_missing_model_is_not_forgotten() {
    let existing = parse_table(
        "read_on = \"2026-01-01\"\n\
         [[modality]]\nprovider = \"anthropic\"\nprefix = \"claude-opus-4-8\"\ninput = [\"text/*\"]\noutput = [\"text/*\"]\nsource = \"manual\"\n\
         [[modality]]\nprovider = \"openai\"\nprefix = \"legacy-model\"\ninput = [\"text/*\"]\noutput = [\"text/*\"]\nsource = \"openrouter\"\n",
    )
    .unwrap();
    let fetched = parse_catalogue(&catalogue()).unwrap();
    let merged = merge(&existing, &fetched, "2026-09-11");
    // The manual opus row is untouched despite the catalogue listing more.
    let opus = &merged.table.rows[&("anthropic".into(), "claude-opus-4-8".into())];
    assert_eq!(opus.input, vec!["text/*"]);
    assert_eq!(opus.source, "manual");
    // The legacy row the catalogue no longer lists is still there.
    assert!(
        merged
            .table
            .rows
            .contains_key(&("openai".into(), "legacy-model".into()))
    );
    // The two genuinely new rows are the only changes, both additions.
    assert_eq!(merged.changes.len(), 2);
    assert!(merged.changes.iter().all(|c| matches!(c, Change::Added(_))));
    assert_eq!(merged.table.read_on, "2026-09-11");
}

#[test]
fn a_changed_list_is_reported_and_an_unchanged_one_is_not() {
    let existing = parse_table(
        "read_on = \"2026-01-01\"\n\
         [[modality]]\nprovider = \"anthropic\"\nprefix = \"claude-opus-4-8\"\ninput = [\"text/*\"]\noutput = [\"text/*\"]\nsource = \"openrouter\"\n",
    )
    .unwrap();
    let fetched = parse_catalogue(&catalogue()).unwrap();
    let merged = merge(&existing, &fetched, "2026-09-11");
    let changed = merged
        .changes
        .iter()
        .find(|c| matches!(c, Change::Changed(_, _)))
        .unwrap();
    assert!(format!("{changed}").contains("claude-opus-4-8"));
    // Re-merging the written table against the same catalogue moves nothing.
    let again = merge(&merged.table, &fetched, "2026-12-25");
    assert!(again.changes.is_empty());
    assert_eq!(again.table.read_on, merged.table.read_on);
}

#[test]
fn added_change_prints_a_plus_line() {
    let row = Row {
        provider: "openai".into(),
        prefix: "gpt-9o".into(),
        input: vec!["text/*".into(), "image/*".into()],
        output: vec!["text/*".into()],
        source: "openrouter".into(),
    };
    assert!(format!("{}", Change::Added(row)).starts_with("+ openai/gpt-9o"));
}

#[test]
fn run_with_writes_then_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modalities.toml");
    std::fs::write(&path, "read_on = \"2026-01-01\"\n").unwrap();
    let outcome = run_with(ModalitiesMode::Write, fixture_fetch, &path, "2026-09-11").unwrap();
    assert_eq!(outcome, Outcome::Changed(3));
    let again = run_with(ModalitiesMode::Write, fixture_fetch, &path, "2026-12-25").unwrap();
    assert_eq!(again, Outcome::Unchanged);
}

#[test]
fn run_with_check_fails_when_the_file_would_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modalities.toml");
    std::fs::write(&path, "read_on = \"2026-01-01\"\n").unwrap();
    let err = run_with(ModalitiesMode::Check, fixture_fetch, &path, "2026-09-11").unwrap_err();
    assert!(err.to_string().contains("would change"));
    assert!(!is_network_error(&err));
}

#[test]
fn run_with_surfaces_the_network_and_parse_failures() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modalities.toml");
    std::fs::write(&path, "read_on = \"2026-01-01\"\n").unwrap();
    let err = run_with(ModalitiesMode::Write, failing_fetch, &path, "2026-09-11").unwrap_err();
    assert!(is_network_error(&err));
    let err = run_with(ModalitiesMode::Write, garbage_fetch, &path, "2026-09-11").unwrap_err();
    assert!(!is_network_error(&err));
    // A missing file is a read error, not a network one.
    let err = run_with(
        ModalitiesMode::Write,
        fixture_fetch,
        &dir.path().join("gone.toml"),
        "2026-09-11",
    )
    .unwrap_err();
    assert!(!is_network_error(&err));
}
