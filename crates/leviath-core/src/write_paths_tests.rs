use super::*;

fn globs(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

fn workdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = std::fs::canonicalize(dir.path()).expect("canonical temp dir");
    std::fs::create_dir_all(root.join("docs/panels")).expect("docs");
    std::fs::create_dir_all(root.join("src")).expect("src");
    (dir, root)
}

#[test]
fn a_glob_must_stay_inside_the_workdir() {
    for ok in [
        "docs/**",
        "./docs/*.md",
        "docs\\**",
        ".polywright/panels/**",
        "a[bc]/*",
    ] {
        assert_eq!(check_glob(ok), Ok(()), "{ok}");
    }
    for bad in [
        "",
        "  ",
        "/etc/**",
        "\\\\server\\share\\**",
        "C:/x/**",
        "~/notes/**",
        "../sibling/**",
        "docs/../../x",
        "docs/./x",
        "docs/",
        "docs//x",
    ] {
        let err = check_glob(bad).expect_err(bad);
        assert!(err.contains("is refused"), "{bad}: {err}");
    }
    let err = check_glob("docs/[").expect_err("unclosed class");
    assert!(err.contains("not a valid glob"), "{err}");
}

#[test]
fn an_open_fence_allows_everything() {
    let empty: Vec<Vec<String>> = vec![Vec::new()];
    let fence = WriteFence::new(&empty);
    assert!(fence.is_open());
    assert_eq!(fence.check("/anywhere/at/all", Path::new("/w")), Ok(()));
    assert!(WriteFence::default().is_open());
}

#[test]
fn star_stays_in_a_component_and_double_star_crosses_them() {
    let fence = WriteFence::new(&[globs(&["docs/*.md", "src/**"])]);
    assert!(fence.allows("docs/a.md"));
    assert!(!fence.allows("docs/sub/a.md"));
    assert!(fence.allows("src/a/b/c.rs"));
    assert!(!fence.allows("Src/a.rs"), "matching is case-sensitive");
    assert!(!fence.allows("other.md"));
}

#[test]
fn every_layer_must_match() {
    let fence = WriteFence::new(&[globs(&["docs/**", "src/**"]), globs(&["docs/panels/**"])]);
    assert!(fence.allows("docs/panels/a.json"));
    assert!(!fence.allows("docs/readme.md"), "the inner layer narrows");
    assert!(!fence.allows("src/main.rs"));
    assert_eq!(fence.describe(), "(docs/**, src/**) and (docs/panels/**)");
    let one = WriteFence::new(&[globs(&["docs/**", "a/*"])]);
    assert_eq!(one.describe(), "docs/**, a/*");
}

#[test]
fn a_layer_of_bad_globs_refuses_every_write() {
    let fence = WriteFence::new(&[globs(&["/etc/**", "docs/["])]);
    assert!(!fence.is_open());
    assert!(!fence.allows("docs/a.md"));
    assert!(!fence.allows("etc/passwd"));
}

#[test]
fn a_path_is_judged_lexically_and_as_resolved() {
    let (_dir, root) = workdir();
    let fence = WriteFence::new(&[globs(&["docs/**"])]);
    assert_eq!(fence.check("docs/panels/new.json", &root), Ok(()));
    assert_eq!(fence.check("docs/new/deep/file.md", &root), Ok(()));
    assert_eq!(
        fence.check(&root.join("docs/a.md").to_string_lossy(), &root),
        Ok(())
    );
    assert_eq!(fence.check("./docs/x/../a.md", &root), Ok(()));
    assert_eq!(
        fence.check("src/App.tsx", &root),
        Err("src/App.tsx".to_string())
    );
    assert_eq!(
        fence.check("docs/../src/App.tsx", &root),
        Err("src/App.tsx".to_string()),
        "the refusal shows the folded path"
    );
    // Outside the workdir, or climbing past the root: shown as asked.
    assert_eq!(fence.check("/etc/passwd", &root), Err("/etc/passwd".into()));
    let deep = "../".repeat(64) + "x";
    assert_eq!(fence.check(&deep, &root), Err(deep.clone()));
    // The workdir itself is no file to write.
    assert_eq!(fence.check(".", &root), Err(".".into()));
}

/// A symlink inside an allowed directory that points outside it is judged by
/// where it lands, and a link from outside into an allowed directory is
/// judged by how it is written. Both must pass.
#[cfg(unix)]
#[test]
fn a_symlink_is_judged_by_where_it_lands() {
    let (_dir, root) = workdir();
    std::os::unix::fs::symlink(root.join("src"), root.join("docs/to_src")).expect("link");
    std::os::unix::fs::symlink(root.join("docs"), root.join("src/to_docs")).expect("link");
    let fence = WriteFence::new(&[globs(&["docs/**"])]);
    assert_eq!(
        fence.check("docs/to_src/main.rs", &root),
        Err("docs/to_src/main.rs".to_string())
    );
    assert_eq!(
        fence.check("src/to_docs/a.md", &root),
        Err("src/to_docs/a.md".to_string())
    );
}

/// The Windows twin: a directory junction needs no privilege, and lands the
/// same way a symlink does.
#[cfg(windows)]
#[test]
fn a_junction_is_judged_by_where_it_lands() {
    let (_dir, root) = workdir();
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(root.join("docs").join("to_src"))
        .arg(root.join("src"))
        .status()
        .expect("mklink runs");
    assert!(status.success());
    let fence = WriteFence::new(&[globs(&["docs/**"])]);
    assert_eq!(
        fence.check("docs/to_src/main.rs", &root),
        Err("docs/to_src/main.rs".to_string())
    );
}
