//! Tests for the roots a server fences runs to.

use std::path::{Path, PathBuf};

use super::WorkdirRoots;

fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).expect("the directory is there")
}

/// Each root is held as the directory it names when the server starts, in
/// the order given.
#[test]
fn roots_are_resolved_once_in_order() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let roots =
        WorkdirRoots::resolve(&[first.path().join("."), second.path().to_path_buf()]).unwrap();
    assert_eq!(roots.as_slice(), [real(first.path()), real(second.path())]);
    assert!(!roots.is_empty());
    assert!(WorkdirRoots::resolve(&[]).unwrap().is_empty());
}

/// A root that is not there, or is a file, is refused: a server never starts
/// fenced to a path somebody could create later as a link to `/`.
#[test]
fn a_root_that_is_not_an_existing_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("later");
    assert_eq!(
        WorkdirRoots::resolve(&[dir.path().to_path_buf(), missing.clone()]).unwrap_err(),
        format!(
            "--workdir-root '{}' is not a directory that exists; create it first",
            missing.display()
        )
    );
    let file = dir.path().join("file");
    std::fs::write(&file, "x").unwrap();
    assert!(WorkdirRoots::resolve(&[file]).is_err());
}

/// The deepest root holding a path is the one reported, and a path whose
/// `..` climbs out of every root is held by none.
#[test]
fn the_deepest_root_holds_a_path() {
    let outer = tempfile::tempdir().unwrap();
    let inner = outer.path().join("inner");
    std::fs::create_dir_all(inner.join("task")).unwrap();
    let roots = WorkdirRoots::resolve(&[outer.path().to_path_buf(), inner.clone()]).unwrap();
    assert_eq!(
        roots.holding(&inner.join("task")),
        Some(real(&inner).as_path())
    );
    assert_eq!(
        roots.holding(outer.path()),
        Some(real(outer.path()).as_path())
    );
    assert_eq!(roots.holding(&outer.path().join("..")), None);
    // A path with nothing along it to resolve cannot be shown inside.
    assert_eq!(roots.holding(Path::new("no-such-relative-name")), None);
}

/// A root swapped for a symlink after the server started opens nothing: the
/// root is the directory it named at startup, not what its path names now.
#[cfg(unix)]
#[test]
fn a_nested_root_swapped_for_a_symlink_after_startup_opens_nothing() {
    let outer = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::create_dir(elsewhere.path().join("etc")).unwrap();
    let inner = outer.path().join("inner");
    std::fs::create_dir(&inner).unwrap();
    let roots = WorkdirRoots::resolve(&[inner.clone(), outer.path().to_path_buf()]).unwrap();

    // What a run working in the outer root can do to the inner one.
    std::fs::remove_dir(&inner).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), &inner).unwrap();

    assert_eq!(roots.holding(&inner.join("etc")), None);
    assert_eq!(roots.holding(&inner), None);
}
