//! When a loop variable is read as the values the line spells out, and every
//! way a line can make that reading false.

use super::*;

/// Every expanded word of `command`, as the line spells it, with the values
/// it resolves to.
fn uses_for(command: &str, posix: bool) -> Vec<(String, Option<Vec<String>>)> {
    let segments = crate::shell_keys::tokenize_for(command, posix).expect("the line tokenizes");
    let loops = Loops::read(command, &segments, posix);
    let mut out = Vec::new();
    for (at, segment) in segments.iter().enumerate() {
        for word in segment.words.iter().chain(&segment.reads) {
            if !word.literal {
                out.push((word.shown.clone(), loops.resolve(word, at)));
            }
        }
    }
    out
}

fn uses(command: &str) -> Vec<(String, Option<Vec<String>>)> {
    uses_for(command, true)
}

fn values(list: &[&str]) -> Option<Vec<String>> {
    Some(list.iter().map(|v| v.to_string()).collect())
}

/// The lines in `commands` where some use of a variable is resolved.
fn resolved_anywhere<'a>(commands: &[&'a str]) -> Vec<&'a str> {
    commands
        .iter()
        .copied()
        .filter(|c| uses(c).iter().any(|(_, found)| found.is_some()))
        .collect()
}

/// The issue's line: inside the body the variable is each listed value.
#[test]
fn a_loop_variable_is_its_listed_values_inside_the_body() {
    assert_eq!(
        uses("for d in a b; do test -e \"$d\" && echo \"$d\"; done"),
        [
            ("$d".to_string(), values(&["a", "b"])),
            ("$d".to_string(), values(&["a", "b"])),
        ]
    );
    // In braces, beside text, more than once in a word, and in a redirect.
    assert_eq!(
        uses("for d in a b; do cat \"src/${d}.rs\" \"$d/$d\" < \"$d\"; done"),
        [
            ("src/${d}.rs".to_string(), values(&["src/a.rs", "src/b.rs"])),
            ("$d/$d".to_string(), values(&["a/a", "b/a", "a/b", "b/b"])),
            ("$d".to_string(), values(&["a", "b"])),
        ]
    );
    // Unquoted is the same word while nothing touches `IFS`, and a flag
    // spelled like the variable is a flag.
    assert_eq!(
        uses("for d in src docs; do test -d $d; done"),
        [("$d".to_string(), values(&["src", "docs"]))]
    );
    // Quoted list words are values too, and a list may hold `=` and `:`.
    assert_eq!(
        uses("for kv in \"a=1\" 'b:2' c,d; do echo \"$kv\"; done"),
        [("$kv".to_string(), values(&["a=1", "b:2", "c,d"]))]
    );
    // An `if` inside the body, and a pipeline, change nothing.
    assert_eq!(
        uses("for f in a b; do if test -e \"$f\"; then cat \"$f\" | head; fi; done"),
        [
            ("$f".to_string(), values(&["a", "b"])),
            ("$f".to_string(), values(&["a", "b"])),
        ]
    );
}

/// Outside the body the variable is whatever it was: before the loop, after
/// it (`false && for ...` never ran it), and in a loop with no body this
/// follows.
#[test]
fn outside_the_body_the_variable_is_not_read() {
    let lines = [
        "for d in a b; do :; done; cat \"$d\"",
        "cat \"$d\"; for d in a b; do :; done",
        "false && for d in a b; do :; done; cat \"$d\"",
        "for d in a b; do :; done | cat; cat \"$d\"",
        // No `do` after the header (zsh's short loop), and no `done`.
        "for d in a b; cat \"$d\"",
        "for d in a b; do cat \"$d\"",
        "for d in a b; { cat \"$d\"; }",
        // A stray `done` closes nothing, and the loop it follows is over.
        "for d in a b; do :; done; done; cat \"$d\"",
        // `fi done` closes the loop, as `bash` reads it.
        "for d in a; do if true; then :; fi done; cat \"$d\"",
    ];
    assert_eq!(resolved_anywhere(&lines), Vec::<&str>::new());
    // The body ends at its own `done`, not at a nested loop's.
    assert_eq!(
        uses(
            "for d in a b; do while read l; do cat \"$d\"; done < x; cat \"$d\"; done; cat \"$d\""
        ),
        [
            ("$d".to_string(), values(&["a", "b"])),
            ("$d".to_string(), values(&["a", "b"])),
            ("$d".to_string(), None),
        ]
    );
    // An `until` loop nests the same way, and so does a brace group.
    assert_eq!(
        uses("for d in a; do until test -e \"$d\"; do sleep 1; done; { cat \"$d\"; }; done"),
        [
            ("$d".to_string(), values(&["a"])),
            ("$d".to_string(), values(&["a"])),
        ]
    );
    // `fi done` ends the body where `bash` ends it.
    assert_eq!(
        uses("for d in a; do if true; then cat \"$d\"; fi done"),
        [("$d".to_string(), values(&["a"]))]
    );
    // An escaped or quoted `done` is a command called `done`, so the body
    // runs on past it.
    assert_eq!(
        uses("for d in a; do cat \"$d\"; \\done; 'done'; cat \"$d\"; done"),
        [
            ("$d".to_string(), values(&["a"])),
            ("$d".to_string(), values(&["a"])),
        ]
    );
}

/// Anything else on the line that names the variable may rebind it, so no
/// loop over it is read.
#[test]
fn a_variable_named_anywhere_else_is_not_read() {
    let lines = [
        // Reassigned in the body.
        "for d in a b; do d=/etc/passwd; cat \"$d\"; done",
        "for d in a b; do d+=x; cat \"$d\"; done",
        "for d in a b; do read d; cat \"$d\"; done",
        "for d in a b; do read -r d < x; cat \"$d\"; done",
        "for d in a b; do unset d; cat \"$d\"; done",
        "for d in a b; do export d; cat \"$d\"; done",
        "for d in a b; do local d=x; cat \"$d\"; done",
        "for d in a b; do printf -v d %s /etc/passwd; cat \"$d\"; done",
        // The name glued to a flag: `printf -vd`, `read -ad`.
        "for d in a b; do printf -vd %s /etc/passwd; cat \"$d\"; done",
        "for d in a b; do read -ad < x; cat \"$d\"; done",
        // Assigned by an expansion (zsh's `::=`), or expanded by more than name.
        "for d in a b; do y=${d::=/etc/passwd}; cat \"$d\"; done",
        "for d in a b; do echo \"${d:-x}\"; cat \"$d\"; done",
        "for d in a b; do echo \"${#d}\"; cat \"$d\"; done",
        // Bound before the loop, where a `readonly` makes the loop's own
        // assignment fail.
        "readonly d=/etc/passwd; for d in a b; do cat \"$d\"; done",
        "d=/etc/passwd; for d in a b; do cat \"$d\"; done",
        // Bound after it.
        "for d in a b; do cat \"$d\"; done; d=x",
        // Named as a word of its own, or inside one.
        "for d in a b; do tr d x; cat \"$d\"; done",
        "for d in a b; do cat \"$d\" x.d; done",
        "for d in d e; do cat \"$d\"; done",
        "for d in a b; do cat --d \"$d\"; done",
        // A nested or a second loop over the same name.
        "for d in a b; do for d in /etc/passwd; do :; done; cat \"$d\"; done",
        "for d in a b; do cat \"$d\"; done; for d in c; do cat \"$d\"; done",
        "for d in a b; do for d; do cat \"$d\"; done; done",
    ];
    assert_eq!(resolved_anywhere(&lines), Vec::<&str>::new());
}

/// Nested loops over different names each hold inside their own body.
#[test]
fn nested_loops_are_read_each_in_its_own_body() {
    assert_eq!(
        uses("for a in 1 2; do for b in x y; do cat \"$a$b\"; done; cat \"$a\" \"$b\"; done"),
        [
            ("$a$b".to_string(), values(&["1x", "2x", "1y", "2y"])),
            ("$a".to_string(), values(&["1", "2"])),
            ("$b".to_string(), None),
        ]
    );
    // A word naming a variable no loop binds is not resolved at all.
    assert_eq!(
        uses("for a in 1 2; do cat \"$a$HOME\"; done"),
        [("$a$HOME".to_string(), None)]
    );
}

/// The list has to be spelled out: a glob, a brace list, a home directory,
/// an expansion, a substitution, an option or nothing at all is not.
#[test]
fn a_list_that_is_not_spelled_out_is_not_read() {
    let lines = [
        "for f in *.rs; do cat \"$f\"; done",
        "for f in src/?.rs; do cat \"$f\"; done",
        "for f in a[12]; do cat \"$f\"; done",
        "for f in {a,b}; do cat \"$f\"; done",
        "for f in ~/x; do cat \"$f\"; done",
        "for f in a $x; do cat \"$f\"; done",
        "for f in a \"$x\"; do cat \"$f\"; done",
        "for f in $(ls); do cat \"$f\"; done",
        "for f in a 'b c'; do cat \"$f\"; done",
        "for f in a ''; do cat \"$f\"; done",
        "for f in -v x; do cat \"$f\"; done",
        "for f in =ls; do cat \"$f\"; done",
        "for f in a\\ b; do cat \"$f\"; done",
        "for f in; do cat \"$f\"; done",
        "for f; do cat \"$f\"; done",
        // Not `for NAME in`: zsh's two names, a `select`, a quoted keyword, a
        // redirect on the header.
        "for a b in 1 2; do cat \"$a\"; done",
        "select f in a b; do cat \"$f\"; done",
        "for f 'in' a b; do cat \"$f\"; done",
        "for f in a b < list; do cat \"$f\"; done",
        "for f in a b > out; do cat \"$f\"; done",
        "for 'f' in a b; do cat \"$f\"; done",
    ];
    assert_eq!(resolved_anywhere(&lines), Vec::<&str>::new());
}

/// A variable a shell sets on its own says nothing about what it holds:
/// `true /etc/passwd; cat "$_"` reads `/etc/passwd`, `cd` rewrites `$PWD`,
/// and zsh's `set -- x` rewrites `$argv`.
#[test]
fn a_variable_the_shell_sets_is_not_read() {
    let lines = [
        "for _ in a; do true /etc/passwd; cat \"$_\"; done",
        "for PWD in a/b/c; do cd .; cat \"$PWD/../../x\"; done",
        "for PATH in tools; do cat \"$PATH/x\"; done",
        "for REPLY in a; do cat \"$REPLY\"; done",
        "for argv in a; do set -- /etc/passwd; cat \"$argv\"; done",
        "for path in tools; do cat \"$path/x\"; done",
        "for status in a; do cat \"$status\"; done",
        "for reply in a; do cat \"$reply\"; done",
        "for File in a; do cat \"$File\"; done",
        "for f_X in a; do cat \"$f_X\"; done",
    ];
    assert_eq!(resolved_anywhere(&lines), Vec::<&str>::new());
    assert!(is_ordinary("d") && is_ordinary("file_2"));
}

/// A construct that can put a keyword where the shell sees none, or hide one
/// the shell does see, leaves the whole line unread: `[[ -n x || done ]]`
/// has no `done` in it, and neither has a comment.
#[test]
fn a_line_that_may_not_nest_as_it_reads_is_not_read() {
    let lines = [
        "u=1; if false; then for d in a b; do :; done; fi; cat \"$d\"; [[ -n x || done ]]",
        "for d in a b; do [[ -e \"$d\" ]] && cat \"$d\"; done",
        "for d in a b; do (cat \"$d\"); done",
        "(for d in a b; do cat \"$d\"; done)",
        "for d in a b; do cat \"$d\" \"$(pwd)\"; done",
        "for d in a b; do cat \"$d\"; done; echo $((1+1))",
        "for d in a b; do cat \"$d\"; done; echo \"${x:?}\"",
        "for d in a b; do cat \"$d\"; done; echo $'x'",
        "for d in a b; do cat \"$d\" <<EOF\nx\nEOF\ndone",
        "true # ; for d in a b; do\ncat \"$d\"\ntrue # ; done",
        "for d in a b; do cat \"$d\"; done # the files",
        "for d in a b; do cat \"$d\"; \\\ndone",
        "for d in a b; do cat \"$d\"; done; echo a\\\r\nb",
        "x=(a b); for d in a b; do cat \"$d\"; done",
        "case x in y) for d in a b; do cat \"$d\"; done;; esac",
    ];
    assert_eq!(resolved_anywhere(&lines), Vec::<&str>::new());
    // And on `cmd.exe`, which has no such loop.
    assert_eq!(
        uses_for("for d in a b; do cat \"$d\"; done", false),
        [("$d".to_string(), None)]
    );
}

/// A word that starts with `-` is an option, and an expansion in an option
/// stays one: `grep -$d` with `d` set to `R` would follow every symlink.
#[test]
fn a_word_that_is_an_option_is_not_read() {
    assert_eq!(
        uses("for d in R; do grep -$d key . \"--file=$d\"; done"),
        [("-$d".to_string(), None), ("--file=$d".to_string(), None)]
    );
    // The name glued behind a flag also counts as naming the variable, so
    // the other uses on such a line are not read either.
    assert_eq!(
        uses("for d in R; do grep --file=x \"$d\"; done"),
        [("$d".to_string(), values(&["R"]))]
    );
}

/// `bash` and `sh` split an unquoted use on `IFS`, and `IFS=:` cuts
/// `a:/etc/hosts` into `a` and `/etc/hosts`. A line that mentions `IFS` has
/// only its quoted uses read.
#[test]
fn an_unquoted_use_is_read_only_with_the_default_ifs() {
    assert_eq!(
        uses("IFS=:; for u in a:/etc/hosts; do cat $u \"$u\"; done"),
        [
            ("$u".to_string(), None),
            ("$u".to_string(), values(&["a:/etc/hosts"])),
        ]
    );
    assert_eq!(
        uses("for u in a b; do cat $u; done; echo \"$IFS\""),
        [("$u".to_string(), None), ("$IFS".to_string(), None)]
    );
}

/// A word naming several variables is every combination of their values, up
/// to a bound past which it is left alone.
#[test]
fn a_word_with_too_many_spellings_is_not_read() {
    let line = "for a in 1 2 3 4 5; do for b in 1 2 3 4 5; do for c in 1 2 3 4 5; do \
                cat \"$a$b\" \"$a$b$c\"; done; done; done";
    let found = uses(line);
    assert_eq!(found[0].1.as_ref().map(Vec::len), Some(25));
    assert_eq!(found[1].1, None);
}

/// What counts as naming a variable other than using it.
#[test]
fn a_mention_is_any_spelling_but_a_plain_use() {
    for (shown, count) in [
        ("$d", 0),
        ("${d}", 0),
        ("src/$d/${d}.rs", 0),
        ("d", 1),
        ("d=x", 1),
        ("x=d", 1),
        ("a.d", 1),
        ("${d:-x}", 1),
        ("${d[1]}", 1),
        ("${#d}", 1),
        ("${x:-$d}", 0),
        ("$dd", 0),
        ("add", 0),
        ("d_x", 0),
        ("d d", 2),
        // A flag by that name is a flag; the name glued after another flag
        // may be what it assigns.
        ("-d", 0),
        ("-vd", 1),
        ("-rad", 1),
        ("-dx", 0),
        ("--d", 1),
        ("--id", 0),
    ] {
        assert_eq!(mentions(shown, "d"), count, "{shown}");
    }
    assert_eq!(mentions("-file", "file"), 0);
    assert_eq!(mentions("-vfile", "file"), 1);
}
