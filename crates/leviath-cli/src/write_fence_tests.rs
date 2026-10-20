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

/// A `cd` moves where a redirect lands, and a `>&` to a name is a write: both
/// are held to the globs as the shell would run them.
#[test]
fn a_redirect_is_judged_where_the_shell_would_put_it() {
    let (_dir, root) = workdir();
    let fence = docs_only();
    let shell = |command: &str| {
        write_paths_refusal_for("shell", &json!({ "command": command }), &root, &fence, true)
    };
    let moved = shell("cd src && echo x > docs/cd.txt").unwrap();
    assert!(moved.contains("src/docs/cd.txt is outside"), "{moved}");
    let dup = shell("echo x >&src/a.txt").unwrap();
    assert!(dup.contains("src/a.txt is outside"), "{dup}");
    let dup = shell("echo x 2>&1 >& src/b.txt").unwrap();
    assert!(dup.contains("src/b.txt is outside"), "{dup}");
    let lost = shell("cd $D && echo x > docs/a.md").unwrap();
    assert!(lost.contains("cannot be read ahead of time"), "{lost}");
    let posix_nul = shell("echo x > nul").unwrap();
    assert!(posix_nul.contains("nul is outside"), "{posix_nul}");
    assert_eq!(shell("echo x >&2 > docs/ok.md"), None);
}

/// The workdir's own fence follows `cd` and `>&` the same way, whatever the
/// stage's write paths.
#[test]
fn the_workdir_fence_follows_cd_and_a_dup_to_a_name() {
    let (_dir, root) = workdir();
    let refused = |command: &str| {
        crate::tools::escaping_write_refusal_for(
            "shell",
            &json!({ "command": command }),
            &root,
            true,
        )
    };
    let moved = refused("cd /tmp && echo x > esc.txt").unwrap();
    assert!(moved.contains("outside the working directory"), "{moved}");
    let dup = refused("echo x >&/tmp/esc.txt").unwrap();
    assert!(dup.contains("outside the working directory"), "{dup}");
    let lost = refused("cd - && echo x > a.txt").unwrap();
    assert!(lost.contains("moves to a directory"), "{lost}");
    assert_eq!(refused("cd docs && echo x > a.txt"), None);
    assert_eq!(refused("echo x > $OUT"), None, "left to the write policy");
}
