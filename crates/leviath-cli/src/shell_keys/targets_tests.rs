use super::*;

fn wd() -> PathBuf {
    PathBuf::from("/w")
}

fn landed(command: &str) -> Landing {
    landing_for(command, true, &wd(), Some(Path::new("/home/me")))
}

fn known(pairs: &[(&str, &str)]) -> Landing {
    Landing::Known(
        pairs
            .iter()
            .map(|(t, p)| (t.to_string(), PathBuf::from(p)))
            .collect(),
    )
}

#[test]
fn a_target_lands_under_the_workdir_and_a_discard_lands_nowhere() {
    assert_eq!(
        landed("a > x; b 2>/dev/null >> y"),
        known(&[("x", "/w/x"), ("y", "/w/y")])
    );
    assert_eq!(landed("> x"), known(&[("x", "/w/x")]));
    assert_eq!(landed("ls"), known(&[]));
}

/// `>&word` is a write to `word` unless the word is a descriptor or `-`.
#[test]
fn a_dup_to_a_name_is_a_write() {
    assert_eq!(
        landed("echo x >&src/a.txt"),
        known(&[("src/a.txt", "/w/src/a.txt")])
    );
    assert_eq!(
        landed("echo x 2>&1 >& src/b.txt"),
        known(&[("src/b.txt", "/w/src/b.txt")])
    );
    assert_eq!(landed("echo x >&2; echo y 1>&-"), known(&[]));
    assert_eq!(landed("echo x >& $FD"), Landing::Unnamed);
    assert_eq!(landed("cat <&3"), known(&[]));
}

/// On a POSIX shell `NUL` is a file like any other; `cmd.exe` discards it.
#[test]
fn nul_is_a_file_on_a_posix_shell() {
    assert_eq!(landed("echo x > nul"), known(&[("nul", "/w/nul")]));
    assert_eq!(landing_for("echo x > NUL", false, &wd(), None), known(&[]));
}

/// A `cd` may fail, so a target is resolved from where the line was and from
/// where it moved to.
#[test]
fn a_cd_moves_where_later_targets_land() {
    assert_eq!(
        landed("cd src && echo x > docs/cd.txt"),
        known(&[
            ("docs/cd.txt", "/w/docs/cd.txt"),
            ("docs/cd.txt", "/w/src/docs/cd.txt")
        ])
    );
    assert_eq!(
        landed("echo a > first; pushd /tmp/x && echo > esc"),
        known(&[
            ("first", "/w/first"),
            ("esc", "/w/esc"),
            ("esc", "/tmp/x/esc")
        ])
    );
    assert_eq!(
        landed("cd ~ && echo > n"),
        known(&[("n", "/w/n"), ("n", "/home/me/n")])
    );
}

/// A move to somewhere not spelled out leaves every later target unknown.
#[test]
fn a_move_that_cannot_be_read_loses_the_line() {
    for line in [
        "cd && echo > a",
        "cd - && echo > a",
        "popd; echo > a",
        "cd $DIR && echo > a",
    ] {
        assert_eq!(landed(line), Landing::Lost, "{line}");
    }
    assert_eq!(
        landing_for("cd ~ && echo > a", true, &wd(), None),
        Landing::Lost
    );
    // A move after the last write loses nothing that was written.
    assert_eq!(landed("echo > a; cd $DIR"), known(&[("a", "/w/a")]));
    // Past a handful of moves the line is not followed any further.
    let many = "cd a; ".repeat(7) + "echo > z";
    assert_eq!(landed(&many), Landing::Lost);
}

#[test]
fn a_line_that_does_not_parse_or_names_nothing_is_said_to() {
    assert_eq!(landed("echo 'open > a"), Landing::Unparsed);
    assert_eq!(landed("echo 'open"), known(&[]));
    assert_eq!(landed("echo > $OUT"), Landing::Unnamed);
}
