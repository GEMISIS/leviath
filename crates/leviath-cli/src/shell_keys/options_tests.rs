//! Which lines change how the shell reads them, and which words spell an
//! option that lets a program escape.

use super::Spelling;
use crate::shell_keys::{command_keys, tokenize_for};

/// A letter is found alone or in a cluster behind one dash, and a long name
/// by any prefix of it, with or without a value: `getopt_long` and git take
/// an abbreviation.
#[test]
fn an_option_is_recognised_however_it_is_spelled() {
    let letter = Spelling::Letter('C');
    for word in ["-C", "-bC", "-Cb"] {
        assert!(letter.is(word), "{word}");
    }
    for word in ["C", "--C", "-b", "--compile", ""] {
        assert!(!letter.is(word), "{word}");
    }
    let long = Spelling::Long("--compile");
    for word in ["--compile", "--compile=x", "--comp", "--co", "--c", "--c=x"] {
        assert!(long.is(word), "{word}");
    }
    for word in [
        "--",
        "--=x",
        "-c",
        "-C",
        "--checking",
        "--compress",
        "compile",
        "",
    ] {
        assert!(!long.is(word), "{word}");
    }
}

/// The options that let a safe-listed program write or run something make
/// the line one no key names, abbreviated and clustered spellings included.
#[test]
fn an_escaping_option_is_found_in_every_spelling() {
    for line in [
        "git diff --output=x",
        "git log --output x",
        "git branch --edit-description",
        "git branch --edit-desc",
        "git branch --e",
        "file -C -m magic",
        "file -bC -m magic",
        "file --compile -m magic",
        "file --comp -m magic",
        // Named from a directory, it is the same program.
        "/usr/bin/git diff --output=x",
        "./file -C -m magic",
    ] {
        assert!(command_keys(line).is_empty(), "{line}");
    }
    for (line, key) in [
        ("git diff --exit-code", "shell:git diff"),
        ("git log --oneline -5", "shell:git log"),
        ("git branch --list", "shell:git branch"),
        ("file -b README.md", "shell:file"),
        ("file --mime-type README.md", "shell:file"),
        ("cat --output=x", "shell:cat"),
    ] {
        assert_eq!(command_keys(line), [key], "{line}");
    }
}

/// The word of `command`'s first segment that changes the shell's options.
fn changed_by(command: &str) -> Option<String> {
    let segments = tokenize_for(command, true).expect("the line tokenizes");
    super::changes_the_shell(&segments[0].words).map(|word| word.shown.clone())
}

/// The options that only say when to stop and what to print are left alone,
/// and so are the positional parameters.
#[test]
fn the_everyday_set_lines_change_nothing() {
    for line in [
        "set -e",
        "set -euo pipefail",
        "set -eu -o pipefail",
        "set +x",
        "set -o errexit -o nounset",
        "set +o xtrace",
        "set -f",
        "set -C",
        "set -",
        "set -o",
        "set",
        "set -- a b",
        "set -- -k \"$x\"",
        "set a -k",
        "ls",
        "",
    ] {
        assert_eq!(changed_by(line), None, "{line}");
    }
    assert_eq!(command_keys("set -euo pipefail; ls"), ["shell:ls"]);
}

/// Anything that changes how a word is expanded or split makes the line one
/// no key speaks for: zsh's `^x` is every name but `x` under EXTENDED_GLOB.
#[test]
fn an_option_that_changes_how_words_are_read_is_named() {
    for (line, word) in [
        ("setopt extendedglob", "setopt"),
        ("unsetopt equals", "unsetopt"),
        ("emulate sh", "emulate"),
        ("shopt -s dotglob", "shopt"),
        ("set -o extendedglob", "extendedglob"),
        ("set -o shwordsplit", "shwordsplit"),
        ("set +o noglob -o globdots", "globdots"),
        ("set -k", "-k"),
        ("set -a", "-a"),
        ("set -ey", "-ey"),
        ("set -eo \"$name\"", "$name"),
        ("set \"$opts\"", "$opts"),
        ("set -e $more", "$more"),
    ] {
        assert_eq!(changed_by(line).as_deref(), Some(word), "{line}");
        let with_a_read = format!("{line}; cat ^README");
        assert!(command_keys(&with_a_read).is_empty(), "{with_a_read}");
    }
}
