use super::*;
use crate::runfile::reader_tests::{initial, scripted_run, spec};

fn parse(text: &str) -> toml::Table {
    text.parse::<toml::Table>().unwrap()
}

#[test]
fn a_spec_reads_back_as_toml_under_its_own_table() {
    let text = spec_toml(&spec());
    let table = parse(&text);
    let spec = table["spec"].as_table().unwrap();
    assert_eq!(spec["run_id"].as_str(), Some("t-1"));
    // An absent optional field is left out, not written as a placeholder.
    assert!(!spec.contains_key("requested_output"));
}

#[test]
fn a_state_holds_what_toml_cannot_by_writing_it_as_text() {
    let mut s = initial();
    s.progress
        .entry_region_digests
        .insert("notes".into(), u64::MAX);
    s.progress.entry_region_digests.insert("small".into(), 7);
    let table = parse(&state_toml(&s));
    let digests = table["state"]["progress"]["entry_region_digests"]
        .as_table()
        .unwrap();
    assert_eq!(digests["notes"].as_str(), Some("18446744073709551615"));
    assert_eq!(digests["small"].as_integer(), Some(7));
}

#[test]
fn deltas_read_as_an_array_of_tables_with_nulls_named() {
    let states = scripted_run(2);
    let first = crate::state::StateDelta::between(&states[0], &states[1], 1, vec![]);
    // A region whose head changed and whose entries did not carries a `None`
    // inside a list, which TOML has no word for.
    let mut widened = states[1].clone();
    widened.context.regions[0].max_tokens += 1;
    let second = crate::state::StateDelta::between(&states[1], &widened, 2, vec![]);
    let text = deltas_toml(&[first, second]);
    let table = parse(&text);
    let deltas = table["delta"].as_array().unwrap();
    assert_eq!(deltas.len(), 2);
    assert!(text.contains("\"none\""));
}

/// A change that clears a field (`pending` back to nothing) says so: it is
/// not an empty table that names no field at all.
#[test]
fn a_cleared_field_is_named_as_none() {
    let delta = crate::state::StateDelta {
        seq: 3,
        at: 0,
        changes: vec![crate::state::Change::Pending(None)],
        events: vec![],
    };
    let table = parse(&deltas_toml(&[delta]));
    let change = &table["delta"][0]["changes"][0];
    assert_eq!(change["Pending"].as_str(), Some("none"), "{table}");
}

/// A table of settings that are all unset is an empty table: its fields are
/// absent, not each the word "none".
#[test]
fn settings_left_unset_are_absent_not_none() {
    let value = serde_json::json!({
        "nudge": { "enabled": null, "max": null, "text": null },
        "hooks": { "after_inference": null, "on_error": null },
        "kept": { "max": null, "text": "go" },
        "compact": { "prompt": null },
    });
    let text = render("spec", &value);
    assert!(!text.contains("none"), "{text}");
    let table = parse(&text);
    assert!(table["spec"]["nudge"].as_table().unwrap().is_empty());
    assert_eq!(table["spec"]["kept"]["text"].as_str(), Some("go"));
}
