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
