//! Tests for the blueprint rewrite step.

use super::*;

/// A blueprint using every renamed key, at both levels, written the way a
/// person writes one: comments, blank lines, an inline region table.
const OLD: &str = r#"# my agent
[agent]
name = "mine"
version = "0.1.0"

[sandbox]
kind = "container"
persist = true   # keep it warm between stages

[context.regions]
brain = { kind = "custom", script = "c.rhai", persistent = true }

[stages.plan]
model = "m"

[stages.plan.tool_routing]
persist = false
"#;

#[test]
fn every_old_key_is_respelled_and_nothing_else_moves() {
    let (out, changes) = rewrite(OLD).expect("there is something to change");
    assert_eq!(
        out,
        r#"# my agent
[agent]
name = "mine"
version = "0.1.0"

[sandbox]
kind = "container"
keep_warm = true   # keep it warm between stages

[context.regions]
brain = { kind = "custom", script = "c.rhai", pinned = true }

[stages.plan]
model = "m"

[stages.plan.tool_routing]
keep_results = false
"#
    );
    assert_eq!(changes.len(), 3);
    assert!(changes[0].starts_with("`[sandbox] persist` becomes `keep_warm`."));
    assert!(changes[1].starts_with("`[context.regions.brain] persistent` becomes `pinned`."));
    assert!(
        changes[2].starts_with("`[stages.plan.tool_routing] persist` becomes `keep_results`."),
        "{changes:?}"
    );
    // Each line carries the reason, not only the spelling.
    assert!(
        changes[0].contains("torn down when the run ends"),
        "{changes:?}"
    );
}

/// The rewritten file parses back to the same blueprint the old one did.
#[test]
fn the_rewrite_says_exactly_what_it_said_before() {
    let (out, _) = rewrite(OLD).expect("there is something to change");
    let before = leviath_core::manifest::parse_manifest(OLD).expect("the old spelling parses");
    let after = leviath_core::manifest::parse_manifest(&out).expect("the new spelling parses");
    assert_eq!(format!("{before:#?}"), format!("{after:#?}"));
}

/// A blueprint already on the new names, one that is not TOML, and one whose
/// only old key is already superseded by the new one, all have nothing to do.
#[test]
fn there_is_nothing_to_do_when_there_is_nothing_to_do() {
    assert!(rewrite("[sandbox]\nkeep_warm = true\n").is_none());
    assert!(rewrite("this is not [[[ toml").is_none());
    // Both spellings: respelling would put two `keep_warm` in one table and
    // turn a file that loads into one that does not.
    assert!(rewrite("[sandbox]\npersist = true\nkeep_warm = false\n").is_none());
}

/// A key of the same name in a table nothing renamed is left alone.
#[test]
fn a_key_somebody_else_owns_is_not_touched() {
    assert!(rewrite("[agent]\npersist = true\n").is_none());
}

/// The plan reads the agents directory, skips everything that is not a
/// blueprint needing a rewrite, and comes back in name order.
#[test]
fn the_plan_names_the_blueprints_that_would_change() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| {
        let at = dir.path().join(name);
        std::fs::create_dir_all(&at).unwrap();
        std::fs::write(at.join(MANIFEST), text).unwrap();
    };
    write("zeta", OLD);
    write("alpha", OLD);
    write("current", "[sandbox]\nkeep_warm = true\n");
    write("broken", "this is not [[[ toml");
    // A directory with no manifest at all, and a loose file beside them.
    std::fs::create_dir_all(dir.path().join("notes")).unwrap();
    std::fs::write(dir.path().join("README.md"), "hello").unwrap();

    let plan = plan_blueprints(dir.path());
    let names: Vec<&str> = plan.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "zeta"]);
    assert_eq!(plan[0].path, dir.path().join("alpha").join(MANIFEST));
    assert_eq!(plan[0].changes.len(), 3);
    assert!(plan[0].rewritten.contains("keep_warm = true"));
}

/// An agents directory that is not there is not an error: `lev update` runs on
/// a machine that has never installed a blueprint.
#[test]
fn an_agents_directory_that_is_not_there_has_no_blueprints() {
    let dir = tempfile::tempdir().unwrap();
    assert!(plan_blueprints(&dir.path().join("nothing-here")).is_empty());
}
