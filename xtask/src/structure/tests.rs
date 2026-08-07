//! Tests for the structure rule.
//!
//! A sibling file so the scaffolding below stays out of the coverage report,
//! matching the layout the workspace uses everywhere else.

use super::*;

fn reader(files: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Result<String> {
    move |p: &str| {
        files
            .iter()
            .find(|(name, _)| *name == p)
            .map(|(_, body)| (*body).to_string())
            .ok_or_else(|| anyhow::anyhow!("no such fixture: {p}"))
    }
}

fn measured(path: &str, lines: usize) -> Measured {
    Measured {
        path: path.to_string(),
        lines,
    }
}

#[test]
fn production_lines_stops_at_the_test_module() {
    let src = "one\ntwo\n#[cfg(test)]\nmod tests {\n    fn a() {}\n}\n";
    assert_eq!(production_lines(src), 2);
}

/// An indented `#[cfg(test)]` sits on a field or a method, not at the top of the
/// file's test section, so it must not end the count.
#[test]
fn an_indented_cfg_test_is_not_the_test_section() {
    let src = "one\n    #[cfg(test)]\n    field: u8,\ntwo\n";
    assert_eq!(production_lines(src), 4);
}

#[test]
fn a_file_with_no_tests_counts_every_line() {
    assert_eq!(production_lines("a\nb\nc\n"), 3);
}

#[test]
fn test_paths_are_recognised_by_every_convention_the_repo_uses() {
    for p in [
        "crates/x/src/tests.rs",
        "crates/x/src/host_tests.rs",
        "crates/x/tests/integration.rs",
    ] {
        assert!(is_test_path(p), "{p}");
    }
    for p in ["crates/x/src/lib.rs", "crates/x/src/testing.rs"] {
        assert!(!is_test_path(p), "{p}");
    }
}

#[test]
fn measure_skips_tests_and_sorts_longest_first() {
    const FILES: &[(&str, &str)] = &[
        ("a.rs", "x\nx\nx\n"),
        ("b.rs", "x\n"),
        ("tests.rs", "x\nx\nx\nx\nx\n"),
    ];
    let paths = vec![
        "a.rs".to_string(),
        "b.rs".to_string(),
        "tests.rs".to_string(),
    ];
    let got = measure(&paths, &reader(FILES)).unwrap();
    assert_eq!(got.len(), 2, "the test file should be skipped: {got:?}");
    assert_eq!(got[0].path, "a.rs");
    assert_eq!(got[0].lines, 3);
    assert_eq!(got[1].path, "b.rs");
}

#[test]
fn a_read_failure_is_reported_rather_than_swallowed() {
    const FILES: &[(&str, &str)] = &[];
    let paths = vec!["missing.rs".to_string()];
    assert!(measure(&paths, &reader(FILES)).is_err());
}

/// The rule, stated once: a file over the cap fails, and the message names it.
#[test]
fn a_file_over_the_cap_fails_the_check() {
    let over = vec![measured("crates/x/src/long.rs", MAX_PRODUCTION_LINES + 1)];
    let err = report(StructureMode::Check, &over).unwrap_err().to_string();
    assert!(err.contains("over the production-line limit"), "{err}");
}

/// Exactly at the cap is allowed; the limit is "at most", not "fewer than".
#[test]
fn a_file_exactly_at_the_cap_passes() {
    let at = vec![measured("crates/x/src/exact.rs", MAX_PRODUCTION_LINES)];
    assert!(!at[0].is_over());
    assert!(report(StructureMode::Check, &at).is_ok());
}

#[test]
fn list_mode_prints_and_never_fails() {
    let huge = vec![measured("anything.rs", 99_999)];
    assert!(report(StructureMode::List, &huge).is_ok());
}

#[test]
fn mode_parses_its_three_forms() {
    assert_eq!(StructureMode::parse(&[]).unwrap(), StructureMode::Check);
    assert_eq!(
        StructureMode::parse(&["--check".to_string()]).unwrap(),
        StructureMode::Check
    );
    assert_eq!(
        StructureMode::parse(&["--list".to_string()]).unwrap(),
        StructureMode::List
    );
    assert!(StructureMode::parse(&["--nope".to_string()]).is_err());
}

/// The check has to hold against the real tree, or it is decorative.
#[test]
fn the_workspace_itself_passes_the_rule() {
    // `cargo test -p xtask` runs from the package directory, but the rule walks
    // workspace-relative paths - as `cargo xtask structure` does from the root.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits under the workspace root");
    std::env::set_current_dir(root).expect("chdir to the workspace root");

    let paths = walk_workspace().expect("the workspace should be walkable");
    assert!(paths.len() > 100, "only found {} files", paths.len());
    let measured = measure(&paths, &|p| Ok(std::fs::read_to_string(p)?))
        .expect("every walked file should be readable");
    let over: Vec<&Measured> = measured.iter().filter(|m| m.is_over()).collect();
    assert!(over.is_empty(), "over the limit: {over:?}");
}
