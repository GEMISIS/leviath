use super::*;

/// A real workdir with `src/main.rs` in it, canonical so the fence compares
/// like with like on macOS.
fn workdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    (dir, canonical)
}

/// The workdir fence alone: what `may_read` is with no `[read_paths]`.
fn fence(workdir: &Path) -> impl Fn(&Path) -> bool + '_ {
    move |p: &Path| {
        leviath_tools::resolve_within(&p.to_string_lossy(), workdir, leviath_core::resolves_within)
            .is_ok()
    }
}

fn outside(command: &str, workdir: &Path, home: Option<&Path>) -> Outside {
    outside_workdir_for(command, true, workdir, home, &fence(workdir))
}

/// The issue's three examples stay pre-approvable: nothing in them leaves.
#[test]
fn reads_of_the_project_stay_inside() {
    let (_dir, wd) = workdir();
    for command in [
        "cat src/main.rs",
        "grep -rn foo .",
        "git log -- src",
        "grep -rn \"/api/v1\" src",
        "echo /etc/hosts",
        "cd src && cat main.rs",
        "cat -n src/main.rs | head -5",
        "ls",
        "jq '.a/.b' data.json",
        "cut -d. -f1 src/main.rs",
        "git log --format=%h/%s -- src",
        "export FOO=1; cat src/main.rs",
    ] {
        let found = outside(command, &wd, None);
        assert!(found.is_empty(), "{command}: {found:?}");
    }
}

/// The issue's refusals, each naming what left.
#[test]
fn reads_outside_the_project_are_flagged() {
    let (_dir, wd) = workdir();
    let home = wd.parent().unwrap().join("someone");
    for (command, key, operand) in [
        ("cat /etc/hosts", "shell:cat /etc/hosts", "/etc/hosts"),
        ("grep -r key ~", "shell:grep", "~"),
        (
            "head ../other/secret",
            "shell:head ../other/secret",
            "../other/secret",
        ),
        ("git -C /elsewhere log", "shell:git", "/elsewhere"),
        ("cat < /etc/hosts", "shell:cat", "/etc/hosts"),
        (
            "cat $HOME/.ssh/id_rsa",
            "shell:cat",
            "HOME/.ssh/id_rsa (expanded at run time)",
        ),
        ("cat ~other/x", "shell:cat ~other/x", "~other/x"),
        ("grep -f/etc/list .", "shell:grep", "-f/etc/list"),
        (
            "git --git-dir=/x/.git log",
            "shell:git",
            "--git-dir=/x/.git",
        ),
        ("grep -e x /etc/passwd", "shell:grep", "/etc/passwd"),
    ] {
        let found = outside(command, &wd, Some(&home));
        assert_eq!(found.keys, [key], "{command}");
        assert_eq!(found.operands, [operand], "{command}");
    }
}

/// A `cd` out of the workdir uncovers everything after it, and is itself
/// flagged; one with no target goes home, which is outside too.
#[test]
fn a_cd_out_uncovers_the_rest_of_the_line() {
    let (_dir, wd) = workdir();
    let found = outside("cd / && cat etc/passwd", &wd, None);
    assert_eq!(found.keys, ["shell:cat etc/passwd", "shell:cd"]);
    let found = outside("cd; ls", &wd, None);
    assert_eq!(found.keys, ["shell:cd", "shell:ls"]);
    assert_eq!(found.operands[0], "cd");
}

/// After a `cd` inside, a relative path has to stay in from both places the
/// shell might be, so a climb that only works from the deeper one is caught.
#[test]
fn a_cd_inside_is_followed_from_every_base() {
    let (_dir, wd) = workdir();
    assert!(outside("cd src && cat main.rs", &wd, None).is_empty());
    // Inside from `src`, outside from the workdir: which one the shell is in
    // depends on whether the `cd` ran, so it asks.
    assert!(!outside("cd src || cat ../src/main.rs", &wd, None).is_empty());
    let found = outside("cd src && cat ../../x", &wd, None);
    assert_eq!(found.keys, ["shell:cat ../../x"]);
}

/// `~` is the home it names: inside when the workdir is under it and the
/// path lands in the workdir, outside otherwise, and outside with no home.
#[test]
fn home_expands_before_the_check() {
    let (_dir, wd) = workdir();
    let home = wd.parent().unwrap().to_path_buf();
    let name = wd.file_name().unwrap().to_string_lossy().to_string();
    assert!(outside(&format!("cat ~/{name}/src/main.rs"), &wd, Some(&home)).is_empty());
    assert!(!outside("cat ~/x", &wd, None).is_empty());
    assert!(!outside(&format!("cd ~/{name}/src && cat ../../x"), &wd, Some(&home)).is_empty());
}

/// A granted `[read_paths]` root is as good as the workdir: `may_read` is
/// what decides, so whatever it allows is inside.
#[test]
fn a_granted_root_counts_as_inside() {
    let (_dir, wd) = workdir();
    let granted = |p: &Path| p.starts_with("/granted") || fence(&wd)(p);
    let found = outside_workdir_for("cat /granted/doc.md", true, &wd, None, &granted);
    assert!(found.is_empty(), "{found:?}");
}

/// A line that runs nothing, or cannot be read, names nothing.
#[test]
fn nothing_runs_nothing_is_outside() {
    let (_dir, wd) = workdir();
    assert!(outside("for x in /etc/*; do", &wd, None).is_empty());
    assert!(outside("cat `x`", &wd, None).is_empty());
    assert!(outside("eval cat /etc/hosts", &wd, None).is_empty());
}

#[test]
fn the_platform_reading_is_the_default() {
    let (_dir, wd) = workdir();
    let found = outside_workdir("cat /etc/hosts", &wd, None, &fence(&wd));
    assert!(!found.is_empty());
}

/// A symlink in the workdir that points out is outside when named, because
/// the fence checks where it really goes.
#[cfg(unix)]
#[test]
fn a_symlink_out_is_outside() {
    let (_dir, wd) = workdir();
    std::os::unix::fs::symlink("/etc", wd.join("link")).unwrap();
    assert_eq!(outside("cat link", &wd, None).keys, ["shell:cat link"]);
    assert_eq!(
        outside("cat link/hosts", &wd, None).keys,
        ["shell:cat link/hosts"]
    );
}

#[cfg(windows)]
#[test]
fn a_symlink_out_is_outside() {
    // Windows CI cannot create symlinks; the fence's canonical check is the
    // same call either way, so hold the absolute spelling instead.
    let (_dir, wd) = workdir();
    let found = outside_workdir_for("type C:\\Windows\\win.ini", false, &wd, None, &fence(&wd));
    assert!(
        found.is_empty(),
        "type opens no file it is named: {found:?}"
    );
    let found = outside_workdir_for("more C:\\Windows\\win.ini", false, &wd, None, &fence(&wd));
    assert_eq!(found.keys, ["shell:more C:\\Windows\\win.ini"]);
}

#[test]
fn candidates_read_flags_the_way_a_program_might() {
    fn all(t: &str) -> Vec<&str> {
        candidates(t).collect()
    }
    assert_eq!(all("src"), ["src"]);
    assert_eq!(all("-"), ["-"]);
    assert_eq!(all("--file=x"), ["x"]);
    assert!(all("--verbose").is_empty());
    assert_eq!(all("-rn"), ["n"]);
    assert!(all("-r").is_empty());
    assert_eq!(all("-rf/etc/x"), ["f/etc/x", "/etc/x", "/x"]);
    assert_eq!(all("-f../x"), ["../x"]);
    assert_eq!(all("-f./list"), ["./list"]);
}
