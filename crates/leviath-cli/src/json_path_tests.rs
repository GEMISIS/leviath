use super::*;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct Region {
    kind: String,
}

#[derive(Debug, Deserialize)]
struct Layout {
    regions: Vec<Region>,
}

#[derive(Debug, Deserialize)]
struct Doc {
    layout: Layout,
}

#[test]
fn a_good_value_reads() {
    let doc: Doc = read(json!({"layout": {"regions": [{"kind": "a"}]}})).unwrap();
    assert_eq!(doc.layout.regions.len(), 1);
    assert_eq!(doc.layout.regions[0].kind, "a");
}

#[test]
fn a_missing_field_names_its_index_and_the_objects_name() {
    let mut regions: Vec<_> = (0..12).map(|_| json!({"kind": "ok"})).collect();
    regions.push(json!({"name": "error_report"}));
    let err = read::<Doc>(json!({"layout": {"regions": regions}})).unwrap_err();
    assert_eq!(
        err,
        "layout.regions[12] (error_report): missing field `kind`"
    );
}

#[test]
fn an_unnamed_object_gets_the_path_alone() {
    let err = read::<Doc>(json!({"layout": {"regions": [{}]}})).unwrap_err();
    assert_eq!(err, "layout.regions[0]: missing field `kind`");
}

#[test]
fn a_wrong_type_names_the_enclosing_objects_name() {
    let err = read::<Doc>(json!({"layout": {"regions": [{"name": "x", "kind": 3}]}})).unwrap_err();
    assert!(err.starts_with("layout.regions[0].kind (x): "), "{err}");
}

#[test]
fn a_root_failure_has_no_path() {
    let err = read::<Doc>(json!([])).unwrap_err();
    assert!(!err.contains(": "), "{err}");
}

#[test]
fn a_path_through_a_scalar_finds_no_name() {
    let document = json!({"layout": 5});
    let err = serde_path_to_error::deserialize::<_, Doc>(document.clone()).unwrap_err();
    assert_eq!(nearest_name(&document, err.path()), None);
}
