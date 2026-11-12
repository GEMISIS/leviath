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
            "$HOME/.ssh/id_rsa (expanded at run time)",
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
    // A link whose name is a pattern that also matches a plain file: quoted,
    // the shell opens the link.
    std::fs::write(wd.join("m"), "").unwrap();
    std::os::unix::fs::symlink(out.path().join("secret.txt"), wd.join("[m]")).unwrap();
    let lines = [
        "cat link",
        "cat link/x",
        "cat leak.txt",
        "cat lea*",
        "cat link/../secret.txt",
        "cat \"[m]\"",
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

// ─── What a grant can name, and what stops one ───────────────────────────────

use leviath_core::interaction::{GrantBlockerReason, outside_path_of};

/// The lines in `commands` judged outside, so a failure names all of them.
fn judged_outside<'a>(commands: &[&'a str], wd: &Path) -> Vec<&'a str> {
    commands
        .iter()
        .copied()
        .filter(|c| !outside(c, wd, None).is_empty())
        .collect()
}

/// The first word that keeps `command` from being granted, with the reason.
fn blocker(command: &str, wd: &Path) -> (String, GrantBlockerReason) {
    let found = outside(command, wd, None);
    let first = found.blocked.first().expect("the line is blocked");
    assert!(
        first.word.is_empty() || first.detail.starts_with(&format!("`{}` ", first.word)),
        "{first:?}"
    );
    (first.word.clone(), first.reason)
}

/// A plain word that names one path outside is reported as that program with
/// that path, resolved, and a grant records a key for it after the keys that
/// say what runs.
#[test]
fn a_plain_word_outside_is_a_path_a_grant_can_name() {
    let (_dir, wd) = workdir();
    let found = outside("test -x /abs/tool", &wd, None);
    assert_eq!(found.operands, ["/abs/tool"]);
    assert!(found.blocked.is_empty(), "{found:?}");
    assert_eq!(found.paths.len(), 1);
    assert_eq!(found.paths[0].program, "test");
    assert!(found.paths[0].path.ends_with("tool"), "{found:?}");

    let keys = found.grant_keys(vec!["shell:test".to_string()]);
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0], "shell:test");
    assert_eq!(
        outside_path_of(&keys[1]),
        Some(("test".to_string(), found.paths[0].path.clone()))
    );
    // A line with nothing that runs has no keys to add a path to.
    assert!(found.grant_keys(Vec::new()).is_empty());

    // The same path twice is one key; two programs with it are two.
    let twice = outside("test -x /abs/tool -a -r /abs/tool", &wd, None);
    assert_eq!(twice.paths.len(), 2);
    assert_eq!(twice.grant_keys(vec!["shell:test".to_string()]).len(), 2);
    let two = outside("test -x /abs/tool && cat /abs/tool", &wd, None);
    let programs: Vec<&str> = two.paths.iter().map(|p| p.program.as_str()).collect();
    assert_eq!(programs, ["test", "cat"]);
    assert_eq!(two.grant_keys(vec!["shell:test".to_string()]).len(), 3);

    // The program is named as the line names it, and the word after `--` is
    // a plain word whatever it starts with.
    let found = outside("./tools/run -- /abs/tool --path .", &wd, None);
    assert_eq!(found.paths[0].program, "./tools/run");
    assert_eq!(found.operands, ["/abs/tool"]);

    // From two directories an absolute path is still one path, and a
    // relative one that is outside from both is two.
    assert_eq!(outside("cd src && cat /abs/tool", &wd, None).paths.len(), 1);
    assert_eq!(outside("cd src && cat ../../x", &wd, None).paths.len(), 2);
}

/// A line with a word no key can name records nothing, however much of the
/// rest of it could be named.
#[test]
fn a_word_no_key_can_name_stops_the_whole_grant() {
    let (_dir, wd) = workdir();
    let found = outside("test -x /abs/tool && cat \"$HOME/x\"", &wd, None);
    assert_eq!(found.paths.len(), 1);
    assert_eq!(found.blocked.len(), 1);
    assert!(found.grant_keys(vec!["shell:test".to_string()]).is_empty());
}

/// Each way a line is kept from being granted names the word and the reason.
#[test]
fn every_blocker_names_its_word_and_reason() {
    let (_dir, wd) = workdir();
    use GrantBlockerReason::*;
    for (line, word, reason) in [
        // An expansion only the shell can read.
        ("cat \"$HOME/x\"", "$HOME/x", ExpandedAtRunTime),
        ("cat \"$(pwd)/x\"", "$(pwd)/x", ExpandedAtRunTime),
        ("cat < \"$f\"", "$f", ExpandedAtRunTime),
        ("cd \"$dir\" && ls", "$dir", ExpandedAtRunTime),
        ("ls > \"$OUT\"", "$OUT", ExpandedAtRunTime),
        ("export $NAME=1", "$NAME=1", ExpandedAtRunTime),
        // A path outside where no key names it.
        ("cat --file=/etc/hosts", "--file=/etc/hosts", OutsideWorkdir),
        ("grep -f/etc/list x", "-f/etc/list", OutsideWorkdir),
        ("cat -o=/etc/passwd", "-o=/etc/passwd", OutsideWorkdir),
        ("cat < /etc/hosts", "/etc/hosts", OutsideWorkdir),
        ("cd / && ls", "/", OutsideWorkdir),
        ("cat < =ls", "=ls", OutsideWorkdir),
        ("cd =ls && ls", "=ls", OutsideWorkdir),
        // Paths that cannot be listed ahead of time.
        ("cat /etc/host*", "/etc/host*", UnplaceablePath),
        (
            "cat {/etc/hosts,src/main.rs}",
            "{/etc/hosts,src/main.rs}",
            UnplaceablePath,
        ),
        ("cat ~other/x", "~other/x", UnplaceablePath),
        ("cat ~/x", "~/x", UnplaceablePath),
        (
            "cat file://server/share",
            "file://server/share",
            UnplaceablePath,
        ),
        ("cat ab:~other/x", "ab:~other/x", UnplaceablePath),
        ("cat =ls", "=ls", UnplaceablePath),
        ("diff -r a b", "-r", UnplaceablePath),
        ("grep -rS key .", "-rS", UnplaceablePath),
        (
            "wc --files0-from=list",
            "--files0-from=list",
            UnplaceablePath,
        ),
        ("file -bf list", "-bf", UnplaceablePath),
        ("strings @list", "@list", UnplaceablePath),
        (
            "jq -n 'import \"m\" as $d; $d'",
            "import \"m\" as $d; $d",
            UnplaceablePath,
        ),
        ("cd && ls", "cd", UnplaceablePath),
        ("cd src docs && ls", "cd", UnplaceablePath),
        ("cat ***/secret", "***/secret", UnplaceablePath),
        ("grep -R key .", "-R", UnplaceablePath),
        (
            "grep --dereference-recursive key .",
            "--dereference-recursive",
            UnplaceablePath,
        ),
        ("ls > /dev/tcp/host/80", "/dev/tcp/host/80", UnplaceablePath),
        // What the keys themselves cannot name.
        ("$G --version", "$G", ProgramFromVariable),
        ("\"$(which ls)\" -la", "$(which ls)", ProgramFromVariable),
        ("eval ls", "eval", RunsAssembledCommand),
        (". ./env.sh", ".", RunsAssembledCommand),
        ("trap ls EXIT", "trap", InstallsCode),
        ("alias ls=curl", "alias", InstallsCode),
        ("git diff --output=x", "--output=x", EscapeFlag),
        ("file -C -m magic", "-C", EscapeFlag),
        ("setopt extendedglob; cat ^x", "setopt", UnreadableLine),
        ("set -o globdots; cat *", "globdots", UnreadableLine),
        ("cat f<1-5>", "", UnreadableLine),
        ("export 1x=2", "1x=2", UnreadableLine),
        ("ls `pwd`", "", UnreadableLine),
        ("set -e", "", NothingRuns),
        ("", "", NothingRuns),
        ("for x in a b; do", "", NothingRuns),
    ] {
        assert_eq!(blocker(line, &wd), (word.to_string(), reason), "{line}");
    }
    // After a `cd` out, every program on the line is somewhere unknown.
    let found = outside("cd / && ls", &wd, None);
    assert_eq!(found.blocked[1].word, "ls");
    assert_eq!(found.blocked[1].reason, UnplaceablePath);
    // A line that binds or writes something has a key, so it is not one
    // where nothing runs.
    for keyed in [
        "export FOO=1",
        "FOO=1",
        "> out.txt",
        "ls > out.txt",
        "ls > /dev/null 2>&1",
        "ls",
    ] {
        assert!(outside(keyed, &wd, None).blocked.is_empty(), "{keyed}");
    }
}

/// A program that opens no file may be handed a plain expansion; anything
/// that can run, read or assign is still outside, and so is any expansion
/// handed to a program that opens its arguments.
#[test]
fn a_plain_expansion_is_let_through_only_to_a_program_that_opens_no_file() {
    let (_dir, wd) = workdir();
    let printed = [
        "echo \"exit=${pipestatus[1]}\"",
        "echo $HOME \"$1\" \"$@\" ${x:-default} \"${#x}\" ${path##*/} $((1+2))",
        "printf '%s' \"$x\"",
        "printf '%s' -v \"$x\"",
        "sleep \"$delay\"",
        "echo \"$x\" > out.txt",
    ];
    assert_eq!(judged_outside(&printed, &wd), Vec::<&str>::new());
    let refused = [
        "echo \"$(cat src/main.rs)\"",
        "echo \"${x:-$(pwd)}\"",
        "echo $((x))",
        "echo \"${arr[y]}\"",
        "echo \"${!x}\"",
        "echo \"${(e)x}\"",
        "echo $x:A",
        "echo $'a'",
        "echo \"$x\" < \"$f\"",
        "cat \"$x\"",
        "test -e \"$x\"",
        "printf -- \"$fmt\"",
    ];
    assert_eq!(judged_inside(&refused, &wd), Vec::<&str>::new());
    // An expansion that may spell `printf -v`, or the name it assigns, or
    // that assigns by itself, stops the line before its words are walked.
    for line in [
        "printf \"$fmt\" x",
        "printf -v \"$name\" %s x",
        "echo \"${x:=y}\"",
    ] {
        assert_eq!(
            blocker(line, &wd).1,
            GrantBlockerReason::ExpandedAtRunTime,
            "{line}"
        );
    }
    // On `cmd.exe` a `$` is text and a `%VAR%` is what expands: its value is
    // parsed as part of the line, so it is never plain.
    let cmd = |c: &str| outside_workdir_for(c, false, &wd, None, &fence(&wd));
    assert!(cmd("echo $x").is_empty());
    assert!(!cmd("echo $x%PATH%").is_empty());
}

/// A loop variable whose values the line spells out is checked as each of
/// them: as an argument, as a `cd` target, as a redirect.
#[test]
fn a_spelled_out_loop_variable_is_checked_as_its_values() {
    let (_dir, wd) = workdir();
    let inside = [
        "for d in a b; do test -e \"$d\" && echo \"$d\"; done",
        "for f in src/main.rs README.md; do wc -l \"$f\"; done",
        "for d in src docs; do test -d $d && ls \"$d\"; done",
        "for f in src/main.rs; do cat < \"$f\"; done",
        "for f in main; do cat \"src/$f.rs\"; done",
    ];
    assert_eq!(judged_outside(&inside, &wd), Vec::<&str>::new());
    let reaches_out = [
        "for f in src/main.rs /etc/passwd; do cat \"$f\"; done",
        "for f in a ../../etc/passwd; do cat \"$f\"; done",
        "for d in src /; do cd \"$d\" && ls; done",
        // A `cd` in a loop moves the shell for the passes after it.
        "for d in src; do cd \"$d\" && cat main.rs; done",
        "while test -e src; do cat main.rs; cd src; done",
        "until cd src; do cat main.rs; done",
        "for f in a /etc/passwd; do cat < \"$f\"; done",
        "for f in a /etc/passwd; do grep --file=\"$f\" x; done",
        "for f in src; do grep --include=\"$f\" -rn x .; done",
        "for d in R; do grep -$d key .; done",
        "for c in ls; do cat =$c; done",
        "for c in ls; do cat < =$c; done",
        "for d in other; do cat ~$d/x; done",
        "for d in src; do cat \"$d\"/../../x; done",
        "for d in src; do cat {$d,../x}; done",
        "for x in le:../../x; do cat fi$x; done",
        "for x in file; do cat $x:../../x; done",
        "for d in list; do strings @$d; done",
        "for f in a b; do true; done; cat \"$f\"",
        "for f in a b; do f=/etc/passwd; cat \"$f\"; done",
        "for f in *; do cat \"$f\"; done",
    ];
    assert_eq!(judged_inside(&reaches_out, &wd), Vec::<&str>::new());
    // An outside value is a path like any other, named for the program that
    // is handed it; an inside one beside it adds nothing.
    let found = outside(
        "for f in src/main.rs /abs/tool; do cat \"$f\"; done",
        &wd,
        None,
    );
    assert_eq!(found.operands, ["$f"]);
    assert_eq!(found.paths.len(), 1);
    assert_eq!(found.paths[0].program, "cat");
    assert!(found.blocked.is_empty());
    // The platform's other shell has no such loop.
    let cmd = outside_workdir_for(inside[0], false, &wd, None, &fence(&wd));
    assert!(!cmd.is_empty());
}

/// A flag's value and a URL are read the ways a program could open them: see
/// `readings`. What is inside by every reading is inside.
#[test]
fn flag_values_and_urls_are_placed_by_every_reading() {
    let (_dir, wd) = workdir();
    let inside = [
        "godot -s addons/gut/gut_cmdln.gd -gdir=res://tests -gexit",
        "cat -o=src/main.rs",
        "cat res://tests https://example.com/a",
        "cat --url=https://example.com/x",
        "cat file:src/main.rs",
    ];
    assert_eq!(judged_outside(&inside, &wd), Vec::<&str>::new());
    let reaches_out = [
        "cat -o=/etc/passwd",
        "cat -o=../../etc/passwd",
        "cat file:///etc/passwd",
        "cat file:/etc/passwd",
        "cat FILE://localhost/etc/passwd",
        "cat --url=file:///etc/passwd",
        "cat -ufile:///etc/passwd",
        "cat jar:file:///etc/x.jar",
        "cat file://server/share/x",
        "cat file:///%2e%2e/%2e%2e/etc/passwd",
        "cat x:///etc/passwd",
        "cat x://../../etc/passwd",
        "cat --in=x:///etc/passwd",
        "cat -ix:///etc/passwd",
        "cat a://b://../../../etc/passwd",
        "cat -f../list",
        "cat -rf/etc/x",
        "cat -rf/etc/x://y",
        "cat -f/etc/x=y",
    ];
    assert_eq!(judged_inside(&reaches_out, &wd), Vec::<&str>::new());
}

/// A flag that takes its value glued on opens a file called `name=value`,
/// and that reading is kept: a symlink by that name pointing out is seen.
/// A URL glued to a short flag is inside when what follows its scheme is,
/// and a `..` through a name that is not there cannot be resolved at all.
/// (On Windows a one-letter scheme is a drive, and the join folds the `..`.)
#[cfg(unix)]
#[test]
fn a_file_named_like_a_flag_and_its_value_is_checked() {
    let (_dir, wd) = workdir();
    assert!(outside("cat -uhttps://example.com/x/y", &wd, None).is_empty());
    assert_eq!(
        blocker("cat /no/such/../x", &wd),
        (
            "/no/such/../x".to_string(),
            GrantBlockerReason::UnplaceablePath
        )
    );
    let out = tempfile::tempdir().unwrap();
    std::fs::write(out.path().join("secret"), "TOP").unwrap();
    std::os::unix::fs::symlink(out.path().join("secret"), wd.join("f=a")).unwrap();
    assert!(!outside("grep -ff=a src/main.rs", &wd, None).is_empty());
    assert!(outside("grep -ff=b src/main.rs", &wd, None).is_empty());
}

// ─── A word a program or the shell may take apart ────────────────────────────

/// The first name under the root of the file system `wd` is on: something
/// that is there on every platform a test runs on.
fn root_name(wd: &Path) -> String {
    wd.components()
        .find_map(|c| match c {
            std::path::Component::Normal(name) => Some(name.to_string_lossy().to_string()),
            _ => None,
        })
        .expect("a workdir sits under some directory")
}

/// A word is read in its parts as well as whole: what stands between its
/// `:`, `,` and `=` is each checked as a path. `file -m a:b` opens both, and
/// `dd if=x` the one after the `=`.
#[test]
fn every_part_of_a_word_is_checked_as_a_path() {
    let (_dir, wd) = workdir();
    let there = root_name(&wd);
    let reaches_out = [
        format!("file -m ab:/{there}/zz-not-there src/main.rs"),
        format!("file -m=ab:/{there}/zz-not-there src/main.rs"),
        format!("file --magic-file=ab:/{there}/zz-not-there src/main.rs"),
        format!("file -m ab://{there}/zz-not-there src/main.rs"),
        format!("cat if=/{there}/zz-not-there"),
        format!("cat a,/{there}/zz-not-there,b"),
        "file -m x:../../x src/main.rs".to_string(),
        "cat a=../../x".to_string(),
        "cat a:b,../../x".to_string(),
        // `bash` expands a `~` after the `=` or a `:` of a word shaped like
        // an assignment.
        "cat x=~/y".to_string(),
        "cat x=a:~/y".to_string(),
        "cat x:~other/y".to_string(),
    ];
    let lines: Vec<&str> = reaches_out.iter().map(String::as_str).collect();
    assert_eq!(judged_inside(&lines, &wd), Vec::<&str>::new());

    // A part outside at one exact path is something a grant can name, for a
    // plain word; in an option it is not.
    let found = outside(lines[0], &wd, None);
    assert!(found.blocked.is_empty(), "{found:?}");
    assert_eq!(found.paths.len(), 1);
    assert_eq!(found.paths[0].program, "file");
    assert!(found.paths[0].path.ends_with("zz-not-there"), "{found:?}");
    assert_eq!(blocker(lines[1], &wd).1, GrantBlockerReason::OutsideWorkdir);
    // The whole and a part naming the same path are one path.
    let found = outside(&format!("cat ab:///{there}/zz-not-there"), &wd, None);
    assert_eq!(found.paths.len(), 1, "{found:?}");

    // Parts that are harmless relative text, or an absolute path with nothing
    // at the root to open, change nothing.
    let harmless = [
        "cat 12:30 key:value host:8080 a=b,c src/main.rs",
        "godot -s x.gd -gdir=res://tests",
        "cat res://tests https://example.com/a?b=1,2",
        "grep -rn \"url = https://example.com/x\" src",
        "grep -rn \"path=/zz-no-such-root-1118/v1\" src",
        "grep -rn \"://\" src",
        "git log --format=%h:%s -- src",
        "cat ab:/",
    ];
    assert_eq!(judged_outside(&harmless, &wd), Vec::<&str>::new());
}

/// zsh replaces a word that opens with a bare `=` by the path of a command:
/// `cat =ls` reads `/bin/ls`. Quoted or escaped, the `=` is text.
#[test]
fn a_word_zsh_turns_into_a_command_path_cannot_be_placed() {
    let (_dir, wd) = workdir();
    let replaced = [
        "cat =ls",
        "cat =l\"s\"",
        "cat =\"ls\"",
        "head -c 9 =ls",
        // Empty quotes in front of the `=` change nothing for zsh.
        "cat \"\"=ls",
        "cat ''=ls",
        "cat =\"\"ls",
    ];
    assert_eq!(judged_inside(&replaced, &wd), Vec::<&str>::new());
    let text = [
        "cat \"=ls\"",
        "cat '=ls'",
        "cat \\=ls",
        "test a = b",
        "cat a=ls",
        "cat \"=\"ls",
        "cat x\"\"=ls",
        "grep \"=> \" src/main.rs",
        "echo =ls",
    ];
    assert_eq!(judged_outside(&text, &wd), Vec::<&str>::new());
}

/// A parenthesis after a program's word or a redirect's is a glob in zsh:
/// `cat x(id|ps)` opens `xid` or `xps`, whatever those are. After nothing, a
/// keyword, an assignment or a redirect operator it is a subshell, a list or
/// a process substitution.
#[test]
fn a_parenthesis_after_a_word_is_a_glob_in_zsh() {
    let (_dir, wd) = workdir();
    for (line, word) in [
        ("cat x(id|ps)", "x"),
        ("cat (id|ps)id", "cat"),
        ("cat src/*(od)", "src/*"),
        ("cat < x(id|ps)", "cat"),
        ("< x(id|ps)", "x"),
        ("true > x(id|ps)", "true"),
        ("> x(id|ps)", "x"),
        ("cd sr(id|c) && cat main.rs", "sr"),
    ] {
        assert_eq!(
            blocker(line, &wd),
            (word.to_string(), GrantBlockerReason::UnplaceablePath),
            "{line}"
        );
    }
    let nested = [
        "(cd src && cat main.rs)",
        "cat src/main.rs && (cd src; cat main.rs)",
        "if (test -e src); then cat src/main.rs; fi",
        "cat <(cat src/main.rs)",
        "cat src/main.rs | (cat)",
        "cat 'x(id|ps)' \"x(id)\" x\\(id\\)",
    ];
    assert_eq!(judged_outside(&nested, &wd), Vec::<&str>::new());
    // An array assignment runs nothing, and what stops the line is what it
    // lists, read as commands.
    assert!(outside("a=(id ps)", &wd, None).blocked.is_empty());
}

/// An argument that makes a safe-listed program read files the line does not
/// name leaves the line uncovered, and a program is spared the check of its
/// words only when no option of it opens a file.
#[test]
fn an_argument_that_reads_unnamed_files_is_outside() {
    let (_dir, wd) = workdir();
    let reads_more = [
        "grep -R key .",
        "grep -rS key .",
        "egrep --dereference-recursive key .",
        "diff -r a b",
        "diff -ur a b",
        "diff --recursive a b",
        "wc --files0-from=list",
        "du --files0-from=list",
        "file -f list",
        "file --files-from=list",
        "strings @list",
        "jq -n 'import \"m\" as $d {\"search\":\"/etc\"}; $d'",
        "jq 'include \"m\"; .'",
        // `diff` handed a directory compares every file in it.
        "diff src src",
        "diff -q src/main.rs src",
        "diff --from-file=src src/main.rs",
        "cd src && diff main.rs ../src",
        "jq -n '\"m\" | modulemeta'",
        // A long option is taken by any prefix of its name.
        "grep --dereference-r key .",
        "diff --rec a b",
        "wc --files0=list",
        "file --files list",
        // Not spared the check: each opens a file through an option.
        "pgrep -F /etc/hosts",
        "hostname -F /etc/hosts",
        "ps -M /etc/hosts",
    ];
    assert_eq!(judged_inside(&reads_more, &wd), Vec::<&str>::new());
    let reads_what_it_names = [
        "grep -rn key .",
        "diff -u src/main.rs src/main.rs",
        "wc -l src/main.rs",
        "file src/main.rs",
        "strings src/main.rs",
        "jq '.a' data.json",
        "grep --directories=skip key src",
        "diff --report-identical-files src/main.rs src/main.rs",
        "pgrep -f node",
        "ps aux",
        "hostname",
    ];
    assert_eq!(
        judged_outside(&reads_what_it_names, &wd),
        Vec::<&str>::new()
    );
}
