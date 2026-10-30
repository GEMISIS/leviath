use super::*;

fn wd() -> PathBuf {
    PathBuf::from("/w")
}

fn landed(command: &str) -> Landing {
    landing_for(command, true, &wd(), Some(Path::new("/home/me")))
}

fn known(targets: &[(&str, &[&str])]) -> Landing {
    Landing::Known(
        targets
            .iter()
            .map(|(written, lands)| Target {
                written: written.to_string(),
                lands: lands.iter().map(PathBuf::from).collect(),
                by: REDIRECT.to_string(),
            })
            .collect(),
    )
}

/// `known`, for targets a program writes.
fn written(targets: &[(&str, &str, &[&str])]) -> Landing {
    Landing::Known(
        targets
            .iter()
            .map(|(by, written, lands)| Target {
                written: written.to_string(),
                lands: lands.iter().map(PathBuf::from).collect(),
                by: by.to_string(),
            })
            .collect(),
    )
}

#[test]
fn a_target_lands_under_the_workdir_and_a_discard_lands_nowhere() {
    assert_eq!(
        landed("a > x; b 2>/dev/null >> y"),
        known(&[("x", &["/w/x"]), ("y", &["/w/y"])])
    );
    assert_eq!(landed("> x"), known(&[("x", &["/w/x"])]));
    assert_eq!(landed("ls"), known(&[]));
}

/// `>&word` is a write to `word` unless the word is a descriptor or `-`.
#[test]
fn a_dup_to_a_name_is_a_write() {
    assert_eq!(
        landed("echo x >&src/a.txt"),
        known(&[("src/a.txt", &["/w/src/a.txt"])])
    );
    assert_eq!(
        landed("echo x 2>&1 >& src/b.txt"),
        known(&[("src/b.txt", &["/w/src/b.txt"])])
    );
    assert_eq!(landed("echo x >&2; echo y 1>&-"), known(&[]));
    assert_eq!(landed("echo x >& $FD"), Landing::Unnamed);
    assert_eq!(landed("cat <&3"), known(&[]));
}

/// On a POSIX shell `NUL` is a file like any other; `cmd.exe` discards it.
#[test]
fn nul_is_a_file_on_a_posix_shell() {
    assert_eq!(landed("echo x > nul"), known(&[("nul", &["/w/nul"])]));
    assert_eq!(landing_for("echo x > NUL", false, &wd(), None), known(&[]));
}

/// A plain `cd <dir>` may fail, so a later target is resolved from where the
/// line was and from where it moved to.
#[test]
fn a_plain_cd_moves_where_later_targets_land() {
    assert_eq!(
        landed("cd src && echo x > docs/cd.txt"),
        known(&[("docs/cd.txt", &["/w/docs/cd.txt", "/w/src/docs/cd.txt"])])
    );
    assert_eq!(
        landed("echo a > first; pushd /tmp/x && echo > esc"),
        known(&[("first", &["/w/first"]), ("esc", &["/w/esc", "/tmp/x/esc"])])
    );
    assert_eq!(
        landed("cd ~ && echo > n"),
        known(&[("n", &["/w/n", "/home/me/n"])])
    );
    // Past a handful of moves the line is not followed any further.
    let many = "cd a; ".repeat(7) + "echo > z";
    assert_eq!(landed(&many), Landing::Lost);
}

/// Every other way a line can change directory loses its relative targets,
/// while an absolute one still lands where it says.
#[test]
fn any_other_move_loses_relative_targets() {
    for line in [
        "X=cd; { $X src; }; echo x > docs/var.txt",
        "{ $(printf c)d src; }; echo x > docs/sub.txt",
        "command $X src; echo > a",
        "export OLDPWD=/tmp/x; cd ~-; echo > esc.txt",
        "cd ~+ && echo > a",
        "cd ~root && echo > a",
        "trap 'cd src' EXIT; echo > a",
        "{ cd src; }; echo x > docs/a.txt",
        "command cd src && echo x > docs/a.txt",
        "builtin cd src && echo x > docs/a.txt",
        "CDPATH=. cd src && echo x > docs/a.txt",
        "if true; then cd src; fi; echo x > docs/a.txt",
        "! cd src; echo x > docs/a.txt",
        "export CDPATH=src; cd docs && echo x > docs/b.txt",
        "printf 'cd src' > docs/s.sh; . docs/s.sh; echo x > docs/c.txt",
        "source docs/s.sh; echo x > docs/c.txt",
        "eval 'cd src'; echo x > docs/c.txt",
        "chdir src; echo x > a",
        "popd; echo > a",
        "cd && echo > a",
        "cd - && echo > a",
        "cd -P src && echo > a",
        "cd $DIR && echo > a",
        "$GO src; echo > a",
    ] {
        assert_eq!(landed(line), Landing::Lost, "{line}");
    }
    assert_eq!(
        landing_for("cd ~ && echo > a", true, &wd(), None),
        Landing::Lost
    );
    let absolute = match cfg!(windows) {
        true => "C:/w/docs/a.txt",
        false => "/w/docs/a.txt",
    };
    assert_eq!(
        landed(&format!("{{ cd /tmp/x; }}; echo x > {absolute}")),
        known(&[(absolute, &[absolute])])
    );
}

/// What does not move leaves a line followed: bindings with no program, and
/// a known program behind `command`.
#[test]
fn what_does_not_move_keeps_the_line_followed() {
    assert_eq!(
        landed("export X=1; command ls; echo > a"),
        known(&[("a", &["/w/a"])])
    );
    assert_eq!(
        landed("cd ~/src && echo > a"),
        known(&[("a", &["/w/a", "/home/me/src/a"])])
    );
}

#[test]
fn a_line_that_does_not_parse_or_names_nothing_is_said_to() {
    assert_eq!(landed("echo 'open > a"), Landing::Unparsed);
    assert_eq!(landed("echo 'open"), known(&[]));
    assert_eq!(landed("echo > $OUT"), Landing::Unnamed);
}

/// A redirect to the home directory lands there, not under the workdir, and
/// one to another user's home is somewhere only the shell knows.
#[test]
fn a_redirect_expands_a_tilde() {
    assert_eq!(
        landed("echo x > ~/.bashrc"),
        known(&[("~/.bashrc", &["/home/me/.bashrc"])])
    );
    assert_eq!(landed("echo x > ~root/.bashrc"), Landing::Lost);
    assert_eq!(
        landing_for("echo x > ~/.bashrc", true, &wd(), None),
        Landing::Lost
    );
}

/// The live escape: every program on the line writes under the `cd`, and the
/// `cd` may fail, so each target lands in both places.
#[test]
fn the_programs_that_write_are_targets_after_a_cd() {
    assert_eq!(
        landed("cd /tmp && rm -rf uidtest && mkdir uidtest && echo x > project.godot"),
        written(&[
            ("rm", "uidtest", &["/w/uidtest", "/tmp/uidtest"]),
            ("mkdir", "uidtest", &["/w/uidtest", "/tmp/uidtest"]),
            (
                "redirect",
                "project.godot",
                &["/w/project.godot", "/tmp/project.godot"]
            ),
        ])
    );
    assert_eq!(landed("cd $DIR && rm -rf build"), Landing::Lost);
    assert_eq!(landed("rm -rf $DIR"), Landing::Unnamed);
    // Every joiner the line can use is followed the same way.
    assert_eq!(
        landed("cd src || mkdir -p a; touch b | tee c"),
        written(&[
            ("mkdir", "a", &["/w/a", "/w/src/a"]),
            ("touch", "b", &["/w/b", "/w/src/b"]),
            ("tee", "c", &["/w/c", "/w/src/c"]),
        ])
    );
}

/// Which arguments each writer writes: every operand, the destination, the
/// `of=`, or git's repository and the place it makes one.
#[test]
fn each_writer_names_its_targets() {
    let w = |by: &str, name: &str| -> (String, String, Vec<String>) {
        (by.to_string(), name.to_string(), vec![format!("/w/{name}")])
    };
    let as_written = |targets: &[(String, String, Vec<String>)]| -> Landing {
        Landing::Known(
            targets
                .iter()
                .map(|(by, name, lands)| Target {
                    written: name.clone(),
                    lands: lands.iter().map(PathBuf::from).collect(),
                    by: by.clone(),
                })
                .collect(),
        )
    };
    for (line, targets) in [
        ("rm -rf a b", vec![w("rm", "a"), w("rm", "b")]),
        ("rmdir -- a", vec![w("rmdir", "a")]),
        ("mkdir -m 755 -p a", vec![w("mkdir", "a")]),
        ("mkdir -pm755 a", vec![w("mkdir", "a")]),
        ("mkdir --mode=755 a", vec![w("mkdir", "a")]),
        ("mkdir --mode 755 a", vec![w("mkdir", "a")]),
        ("touch -r ref -d 2024-01-01 a", vec![w("touch", "a")]),
        ("truncate -s 0 a", vec![w("truncate", "a")]),
        ("tee -a a b", vec![w("tee", "a"), w("tee", "b")]),
        ("mv a b", vec![w("mv", "a"), w("mv", "b")]),
        ("mv -t d a", vec![w("mv", "a"), w("mv", "d")]),
        ("cp a b c", vec![w("cp", "c")]),
        ("cp -t d a b", vec![w("cp", "d")]),
        ("cp -rt d a b", vec![w("cp", "d")]),
        ("cp -td a", vec![w("cp", "d")]),
        ("cp --target-directory=d a", vec![w("cp", "d")]),
        ("cp -S .bak a b", vec![w("cp", "b")]),
        ("ln -s a b", vec![w("ln", "b")]),
        ("install -m 644 a b", vec![w("install", "b")]),
        ("dd if=a of=b bs=1M", vec![w("dd", "b")]),
        ("dd if=a", vec![]),
        ("cp", vec![]),
        ("/bin/rm -f a", vec![w("rm", "a")]),
        ("FOO=1 command rm a", vec![w("rm", "a")]),
        ("exec builtin rm a", vec![w("rm", "a")]),
        ("rm -f - a", vec![w("rm", "-"), w("rm", "a")]),
        ("git -C d commit -m x", vec![w("git", "d")]),
        (
            "git --git-dir=d/.git --work-tree d add .",
            vec![w("git", "d/.git"), w("git", "d")],
        ),
        ("git -C d status", vec![]),
        ("git add . && git commit -m x && tee a", vec![w("tee", "a")]),
        ("git -C d -c x=y log", vec![]),
        ("git init d", vec![w("git", "d")]),
        (
            "git clone -b main url d",
            vec![w("git", "url"), w("git", "d")],
        ),
        (
            "git -C d worktree add w",
            vec![w("git", "d"), w("git", "w")],
        ),
        ("git", vec![]),
        ("git -C", vec![]),
        ("ls /tmp; cat /etc/hosts; echo rm /tmp/x", vec![]),
    ] {
        assert_eq!(landed(line), as_written(&targets), "{line}");
    }
}

/// A target only the shell can name, wherever it sits in the arguments.
#[test]
fn an_expanded_target_is_unnamed() {
    for line in [
        "rm -rf \"$DIR\"",
        "mkdir -p $(mktemp -d)/x",
        "cp a $OUT",
        "cp -t $OUT a",
        "cp -t$OUT a",
        "dd if=a of=$OUT",
        "dd if=a $SPEC",
        "git -C $REPO commit",
        "tee a $B",
    ] {
        assert_eq!(landed(line), Landing::Unnamed, "{line}");
    }
    // An expansion that is not a target is not a write.
    assert_eq!(landed("cp $SRC a"), written(&[("cp", "a", &["/w/a"])]));
    assert_eq!(
        landed("mkdir -m $MODE a"),
        written(&[("mkdir", "a", &["/w/a"])])
    );
    assert_eq!(
        landed("touch -r $REF a"),
        written(&[("touch", "a", &["/w/a"])])
    );
}

/// A glob that stays under its directory lands there as written when nothing
/// matches it; one that could match `..`, a brace list that climbs, and
/// `~user` are somewhere this cannot list.
#[test]
fn a_glob_or_brace_target_is_placed_or_lost() {
    assert_eq!(
        landed("rm -f *.tmp"),
        written(&[("rm", "*.tmp", &["/w/*.tmp"])])
    );
    assert_eq!(
        landed("mkdir -p src/{a,b}"),
        written(&[("mkdir", "src/{a,b}", &["/w/src/{a,b}"])])
    );
    for line in [
        "rm -rf .*",
        "rm -rf ../*",
        "rm -rf /tmp/*/..",
        "mkdir -p {..,x}/y",
        "rm -rf {/tmp/x,y}",
        "rm -rf {a,/tmp/x}",
        "rm -rf {~,x}",
        "rm -rf ~root/x",
        "cd $DIR && rm -f *.tmp",
    ] {
        assert_eq!(landed(line), Landing::Lost, "{line}");
    }
}
