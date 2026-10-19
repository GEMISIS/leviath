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

/// The lines in `commands` judged inside, so a failure names all of them.
fn judged_inside<'a>(commands: &[&'a str], wd: &Path) -> Vec<&'a str> {
    commands
        .iter()
        .copied()
        .filter(|c| outside(c, wd, None).is_empty())
        .collect()
}

/// The issue's three examples stay pre-approvable, as do the reads of the
/// project an agent writes all day.
#[test]
fn reads_of_the_project_stay_inside() {
    let (_dir, wd) = workdir();
    let lines = [
        "cat src/main.rs",
        "grep -rn foo .",
        "git log -- src",
        "echo /etc/hosts",
        "cd src && cat main.rs",
        "cat -n src/main.rs | head -5",
        "ls",
        "jq '.a' data.json",
        "cut -d. -f1 src/main.rs",
        "git log --format=%h -- src",
        "export FOO=1; cat src/main.rs",
        "cat src/*.rs",
        "grep -rn foo src --include=*.rs",
        "cat -- src/main.rs",
        "for x in a b; do echo hi; done",
    ];
    let outside_ones: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|c| !outside(c, &wd, None).is_empty())
        .collect();
    assert_eq!(outside_ones, Vec::<&str>::new());
}

/// The issue's refusals, each naming what left.
#[test]
fn reads_outside_the_project_are_flagged() {
    let (_dir, wd) = workdir();
    let home = wd.parent().unwrap().join("someone");
    for (command, operand) in [
        ("cat /etc/hosts", "/etc/hosts"),
        ("grep -r key ~", "~"),
        ("head ../other/secret", "../other/secret"),
        ("git -C /elsewhere log", "/elsewhere"),
        ("cat < /etc/hosts", "/etc/hosts"),
        (
            "cat $HOME/.ssh/id_rsa",
            "HOME/.ssh/id_rsa (expanded at run time)",
        ),
        ("cat ~other/x", "~other/x"),
        ("grep -f/etc/list .", "-f/etc/list"),
        ("git --git-dir=/x/.git log", "--git-dir=/x/.git"),
        ("grep -e x /etc/passwd", "/etc/passwd"),
    ] {
        let found = outside(command, &wd, Some(&home));
        assert_eq!(found.operands, [operand], "{command}");
        assert!(found.unplaceable.is_none(), "{command}");
    }
}

/// The review of #970's bypasses: every one of these reads outside the
/// workdir, so none may be pre-approved.
#[test]
fn review_bypasses_are_outside() {
    let (_dir, wd) = workdir();
    let lines = [
        "{ cat; } < /etc/hosts",
        "(cat) < /etc/hosts",
        "for x in a; do cat; done < /etc/hosts",
        "echo \"$(< /etc/hosts)\"",
        "printf %s \"$(< /etc/hosts)\"",
        "grep -ie. /etc/hosts",
        "grep --regex=localhost /etc/hosts",
        "grep -rf /etc/hosts .",
        "jq -n -L /etc '.'",
        "cat {/etc/hosts,src/main.rs}",
        "cat .[.]/.[.]/.[.]/.[.]/.[.]/.[.]/.[.]/.[.]/.[.]/etc/hosts",
        "cat -- --/../../x",
        "echo $HOME",
        "cat src/{a..z}",
        "cat /etc/host*",
        "grep -R key .",
        "grep --dereference-recursive key .",
    ];
    assert_eq!(judged_inside(&lines, &wd), Vec::<&str>::new());
    // A substitution with no text of its own is still named.
    let found = outside("echo \"$(< /etc/hosts)\"", &wd, None);
    assert_eq!(
        found.operands,
        ["/etc/hosts", "a substitution (expanded at run time)"]
    );
    assert!(outside("grep -r key .", &wd, None).is_empty());
}

/// A glob is expanded and every match checked; one too wide to check is
/// outside.
#[test]
fn a_glob_is_checked_match_by_match() {
    let (_dir, wd) = workdir();
    // No match is passed on as written, which is inside.
    assert!(outside("cat src/*.py", &wd, None).is_empty());
    // A match the fence refuses is outside, whatever else matched.
    let refuses_main = |p: &Path| !p.ends_with("src/main.rs") && fence(&wd)(p);
    let found = outside_workdir_for("cat src/*", true, &wd, None, &refuses_main);
    assert_eq!(found.operands, ["src/*"]);
    // Past the cap, nothing is checked and the word is outside.
    let many = wd.join("many");
    std::fs::create_dir_all(&many).unwrap();
    for n in 0..=MAX_GLOB_MATCHES {
        std::fs::write(many.join(n.to_string()), "").unwrap();
    }
    assert_eq!(outside("cat many/*", &wd, None).operands, ["many/*"]);
}

/// A `cd` out of the workdir uncovers everything after it, and is itself
/// flagged; one with no target goes home, which is outside too.
#[test]
fn a_cd_out_uncovers_the_rest_of_the_line() {
    let (_dir, wd) = workdir();
    let found = outside("cd / && cat etc/passwd", &wd, None);
    assert_eq!(
        found.operands,
        ["/", "(after a cd out of the working directory)"]
    );
    let found = outside("cd; ls", &wd, None);
    assert_eq!(found.operands[0], "cd");
}

/// After a `cd` inside, a relative path has to stay in from both places the
/// shell might be, so a climb that only works from the deeper one is caught.
#[test]
fn a_cd_inside_is_followed_from_every_base() {
    let (_dir, wd) = workdir();
    assert!(outside("cd src && cat main.rs", &wd, None).is_empty());
    assert!(!outside("cd src || cat ../src/main.rs", &wd, None).is_empty());
    assert!(!outside("cd src && cat ../../x", &wd, None).is_empty());
}

/// `~` is the home it names: inside when the workdir is under it and the
/// path lands in the workdir, outside otherwise, and outside with no home.
#[test]
fn home_expands_before_the_check() {
    let (_dir, wd) = workdir();
    let home = wd.parent().unwrap().to_path_buf();
    let name = wd.file_name().unwrap().to_string_lossy().to_string();
    assert!(outside(&format!("cat ~/{name}/src/main.rs"), &wd, Some(&home)).is_empty());
    assert!(outside(&format!("cd ~/{name}/src && cat main.rs"), &wd, Some(&home)).is_empty());
    assert!(!outside("cat ~/x", &wd, None).is_empty());
}

/// A granted `[read_paths]` root is as good as the workdir: `may_read` is
/// what decides, so whatever it allows is inside.
#[test]
fn a_granted_root_counts_as_inside() {
    let (_dir, wd) = workdir();
    let granted = |p: &Path| p.ends_with("granted/doc.md") || fence(&wd)(p);
    let found = outside_workdir_for("cat /granted/doc.md", true, &wd, None, &granted);
    assert!(found.is_empty(), "{found:?}");
}

/// A line whose reads its words cannot show is unplaceable: what a `deny`
/// refuses. A line that does not parse, `eval`, a shell or an interpreter.
#[test]
fn a_line_that_runs_something_unseen_is_unplaceable() {
    let (_dir, wd) = workdir();
    for (command, why) in [
        ("cat `x`", "it cannot be read as commands"),
        (
            "eval cat /etc/hosts",
            "it runs a command assembled at run time",
        ),
        (
            "sh -c \"cat /etc/hosts\"",
            "`sh` runs a command this cannot read",
        ),
        ("/bin/bash -c ls", "`bash` runs a command this cannot read"),
        (
            "python3 -c 'print(1)'",
            "`python3` runs a command this cannot read",
        ),
        (
            "git -c alias.x=!ls x",
            "`git` runs a command this cannot read",
        ),
    ] {
        let found = outside(command, &wd, None);
        assert_eq!(found.unplaceable.as_deref(), Some(why), "{command}");
        assert_eq!(found.refusal().as_deref(), Some(why), "{command}");
    }
    // Inside and readable: nothing to refuse.
    assert_eq!(outside("cat src/main.rs", &wd, None).refusal(), None);
    let named = outside("cat /etc/hosts", &wd, None).refusal();
    assert_eq!(named.as_deref(), Some("it names /etc/hosts"));
    // A line that runs nothing names nothing.
    assert!(
        outside("for x in /etc/*; do", &wd, None)
            .refusal()
            .is_none()
    );
}

#[test]
fn the_platform_reading_is_the_default() {
    let (_dir, wd) = workdir();
    let found = outside_workdir("cat /etc/hosts", &wd, None, &fence(&wd));
    assert!(!found.is_empty());
}

/// On `cmd.exe` a `%VAR%` expands, and `type` prints the file it is named.
#[test]
fn cmd_expansions_and_type_are_outside() {
    let (_dir, wd) = workdir();
    let cmd = |c: &str| outside_workdir_for(c, false, &wd, None, &fence(&wd));
    assert_eq!(
        cmd("more %USERPROFILE%\\x").operands,
        ["%USERPROFILE%\\x (expanded at run time)"]
    );
    assert_eq!(
        cmd("echo %USERPROFILE%").operands,
        ["%USERPROFILE% (expanded at run time)"]
    );
    assert!(!cmd("type /etc/hosts").is_empty());
}

/// A symlink in the workdir that points out is outside when named, a glob
/// that reaches it is too, and so is a `..` after it, which the shell
/// resolves beside the link's target rather than beside the link.
#[cfg(unix)]
#[test]
fn a_symlink_out_is_outside() {
    let (_dir, wd) = workdir();
    let out = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(out.path().join("dir")).unwrap();
    std::fs::write(out.path().join("secret.txt"), "TOP").unwrap();
    std::os::unix::fs::symlink(out.path().join("dir"), wd.join("link")).unwrap();
    std::os::unix::fs::symlink(out.path().join("secret.txt"), wd.join("leak.txt")).unwrap();
    let lines = [
        "cat link",
        "cat link/x",
        "cat leak.txt",
        "cat lea*",
        "cat link/../secret.txt",
    ];
    assert_eq!(judged_inside(&lines, &wd), Vec::<&str>::new());
}

#[cfg(windows)]
#[test]
fn a_symlink_out_is_outside() {
    // Windows CI cannot create symlinks. The `..` check is the same call
    // either way, so hold it through a directory that is really there.
    let (_dir, wd) = workdir();
    let lines = ["type src\\..\\..\\x", "more ..\\x"];
    let inside: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|c| outside_workdir_for(c, false, &wd, None, &fence(&wd)).is_empty())
        .collect();
    assert_eq!(inside, Vec::<&str>::new());
    assert!(
        outside_workdir_for("more src\\..\\src\\main.rs", false, &wd, None, &fence(&wd)).is_empty()
    );
}

/// `glob` reads a Windows verbatim path without its prefix, and one that is
/// not absolute without it cannot be matched, so the word is outside.
#[test]
fn a_verbatim_base_is_matched_without_its_prefix() {
    assert_eq!(split_verbatim(r"\\?\C:\w"), (r"\\?\", r"C:\w"));
    assert_eq!(split_verbatim("/w"), ("", "/w"));
    let unc = Path::new(r"\\?\UNC\server\share");
    assert_eq!(expansions(unc, Path::new("*.rs")), None);
}

#[test]
fn candidates_read_flags_the_way_a_program_might() {
    fn all(t: &str) -> Vec<&str> {
        candidates(t).collect()
    }
    assert_eq!(all("src"), ["src"]);
    assert_eq!(all("a=b"), ["a=b"]);
    assert_eq!(all("-"), ["-"]);
    assert_eq!(all("--file=x"), ["x"]);
    assert_eq!(all("--verbose"), ["--verbose"]);
    assert_eq!(all("-rn"), ["n"]);
    assert!(all("-r").is_empty());
    assert_eq!(all("-f/x"), ["/x", "x"]);
}
