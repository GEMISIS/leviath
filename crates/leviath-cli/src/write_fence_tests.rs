use super::*;
use serde_json::json;

fn workdir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    (dir, root)
}

fn docs_only() -> WriteFence {
    WriteFence::new(&[vec![
        "docs/**".to_string(),
        ".polywright/panels/**".to_string(),
    ]])
}

#[test]
fn an_open_fence_refuses_nothing() {
    let (_dir, root) = workdir();
    let open = WriteFence::default();
    let args = json!({ "path": "src/App.tsx", "content": "x" });
    assert_eq!(write_paths_refusal("write_file", &args, &root, &open), None);
}

/// The refusal names the tool, the path, and the globs, so the model can
/// correct course on its next call.
#[test]
fn the_write_tools_are_refused_outside_the_globs() {
    let (_dir, root) = workdir();
    let fence = docs_only();
    let outside = json!({ "path": "src/App.tsx", "content": "x" });
    assert_eq!(
        write_paths_refusal("write_file", &outside, &root, &fence).as_deref(),
        Some(
            "[blocked] write_file refused: src/App.tsx is outside this stage's write paths \
             (docs/**, .polywright/panels/**). Write inside them instead."
        )
    );
    let edit = json!({ "path": "src/App.tsx", "old_str": "a", "new_str": "b" });
    let refused = write_paths_refusal("edit_file", &edit, &root, &fence).unwrap();
    assert!(
        refused.starts_with("[blocked] edit_file refused"),
        "{refused}"
    );
    let inside = json!({ "path": "docs/panel.md", "content": "x" });
    assert_eq!(
        write_paths_refusal("write_file", &inside, &root, &fence),
        None
    );
    assert_eq!(
        write_paths_refusal("edit_file", &inside, &root, &fence),
        None
    );
    // A call with no path writes nothing; the tool says so itself.
    assert_eq!(
        write_paths_refusal("write_file", &json!({}), &root, &fence),
        None
    );
    // A tool that writes nothing a fence covers.
    assert_eq!(
        write_paths_refusal("read_file", &outside, &root, &fence),
        None
    );
}

/// A refusal is reported as a blocked call in the run's events.
#[test]
fn a_refusal_reads_as_blocked() {
    let (_dir, root) = workdir();
    let refused = write_paths_refusal(
        "write_file",
        &json!({ "path": "src/a.rs" }),
        &root,
        &docs_only(),
    )
    .unwrap();
    assert_eq!(
        leviath_runtime::runfile::outcome_of(&refused),
        leviath_core::execution::ToolOutcome::Blocked
    );
}

#[test]
fn a_shell_redirect_is_held_to_the_globs() {
    let (_dir, root) = workdir();
    let fence = docs_only();
    let shell = |command: &str| {
        write_paths_refusal_for("bash", &json!({ "command": command }), &root, &fence, true)
    };
    assert_eq!(shell("echo hi > docs/a.md"), None);
    assert_eq!(shell("ls src 2>/dev/null"), None);
    assert_eq!(shell("cat src/main.rs"), None, "a read is not a write");
    let refused = shell("echo hi > docs/a.md && echo x >> src/main.rs").unwrap();
    assert!(
        refused.starts_with("[blocked] shell refused: src/main.rs is outside"),
        "{refused}"
    );
    // Where a variable points is the shell's to know, not the fence's.
    let refused = shell("echo hi > $OUT").unwrap();
    assert!(
        refused.contains("cannot be read ahead of time"),
        "{refused}"
    );
    let refused = shell("echo 'unterminated > docs/a.md").unwrap();
    assert!(
        refused.contains("cannot be read ahead of time"),
        "{refused}"
    );
    assert_eq!(
        write_paths_refusal("shell", &json!({}), &root, &fence),
        None,
        "a call with no command runs nothing"
    );
}

/// Both shells' readings: `cmd.exe` keeps a backslash as a separator.
#[test]
fn a_windows_redirect_is_read_as_cmd_reads_it() {
    let (_dir, root) = workdir();
    let fence = docs_only();
    let refused = write_paths_refusal_for(
        "shell",
        &json!({ "command": "echo hi > src\\a.txt" }),
        &root,
        &fence,
        false,
    );
    assert!(refused.is_some());
}

#[test]
fn every_write_target_names_them_all_or_says_it_cannot() {
    use crate::shell_keys::every_write_target_for;
    assert_eq!(
        every_write_target_for("a > x; b 2>/dev/null >> y", true),
        WriteTargets::Known(vec!["x".to_string(), "y".to_string()])
    );
    assert_eq!(
        every_write_target_for("a > x; b > $Y", true),
        WriteTargets::Unreadable
    );
    assert_eq!(
        every_write_target_for("echo `x` > y", true),
        WriteTargets::Unreadable
    );
    assert_eq!(
        every_write_target_for("echo 'open", true),
        WriteTargets::Known(Vec::new())
    );
}
