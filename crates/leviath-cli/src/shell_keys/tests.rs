//! Tests for the shell grant-key parser.
//!
//! Named for the consequence rather than the parser rule, because what these
//! pin is a security property and a prompt count, not a grammar.

use super::*;

fn keys(command: &str) -> Vec<String> {
    command_keys(command)
}

fn word(text: &str) -> Word {
    Word {
        text: text.to_string(),
        literal: true,
        quoted: false,
    }
}

fn expansion(text: &str) -> Word {
    Word {
        literal: false,
        ..word(text)
    }
}

fn quoted(text: &str) -> Word {
    Word {
        quoted: true,
        ..word(text)
    }
}

/// The second word of a single-command line, for asserting on how a word was
/// read rather than on the key it produced.
fn second_word(command: &str) -> Word {
    let segments = tokenize(command).expect("line should tokenize");
    segments
        .into_iter()
        .find(|s| s.len() > 1)
        .expect("no segment with an argument")
        .swap_remove(1)
}

fn arg_of(command: &str) -> String {
    second_word(command).text
}

// ─── The security property ───────────────────────────────────────────────────

/// The invariant everything else exists to preserve. Keys are what the caller
/// intersects, so it is stated as "not a subset".
#[test]
fn approving_ls_does_not_cover_ls_and_curl() {
    let granted: std::collections::HashSet<String> = keys("ls -la").into_iter().collect();
    let attempted = keys("ls && curl https://evil");
    assert!(
        !attempted.iter().all(|k| granted.contains(k)),
        "approving `ls` must not cover `ls && curl evil`: {attempted:?}"
    );
    assert!(attempted.iter().any(|k| k.starts_with("shell:curl")));
}

/// A redirect ends the command it follows, not the line. Truncating the line
/// there - which is what the previous implementation did - meant every program
/// after the first redirect was never keyed, so a grant covered programs the
/// user never saw run. This is the reason the parser was rewritten.
#[test]
fn a_redirect_does_not_hide_the_commands_after_it() {
    assert_eq!(
        keys("cat a 2>/dev/null; echo x; curl https://evil"),
        ["shell:cat a", "shell:curl https://evil", "shell:echo"],
    );
}

/// A separator inside quotes is data. Splitting on it produced keys like
/// `shell:Could not` and `shell:FAILURE:' /tmp/deps.log` out of a single
/// `grep -nE 'FAILED|Could not find|FAILURE:' file`.
#[test]
fn a_quoted_pipe_is_not_a_command_boundary() {
    assert_eq!(
        keys("grep -nE 'FAILED|Could not find|FAILURE:' /tmp/deps.log"),
        ["shell:grep"],
    );
}

// ─── Prompt-count regressions ────────────────────────────────────────────────

/// Loop and conditional keywords are not programs. They produced `shell:for i`
/// and `shell:do if` on a real run.
#[test]
fn loop_keywords_are_not_programs() {
    assert_eq!(
        keys("for i in $(seq 1 11); do if grep -q '^EXIT:' /tmp/x; then echo done; fi; done"),
        ["shell:echo", "shell:grep", "shell:seq"],
    );
}

/// A number is data. Keying it split one grant for `sleep` into three.
#[test]
fn numeric_arguments_do_not_split_a_grant() {
    assert_eq!(keys("sleep 55"), ["shell:sleep"]);
    assert_eq!(keys("sleep 55"), keys("sleep 45"));
    assert_eq!(keys("sleep 55"), keys("sleep 50"));
}

/// `cd`'s argument names no program, so keying it re-prompted the first time a
/// run worked in a different directory. Safe because every shell call is a
/// fresh `sh -c` in the run's workdir, so the `cd` cannot outlive it.
#[test]
fn cd_is_keyed_without_its_path() {
    assert_eq!(keys("cd /Users/me/projects/thing"), ["shell:cd"]);
    assert_eq!(keys("cd /a"), keys("cd /b"));
}

/// An environment assignment is not the program being run.
#[test]
fn an_env_assignment_is_not_the_program() {
    assert_eq!(keys("FOO=1 cargo test --lib"), ["shell:cargo test"]);
    assert_eq!(keys("V=0.13.2"), Vec::<String>::new());
}

/// A subshell paren is a command boundary, not part of the program's name.
#[test]
fn a_subshell_paren_is_a_boundary() {
    assert_eq!(
        keys("(ninja -C build all > /tmp/log 2>&1; echo done) &"),
        ["shell:echo", "shell:ninja"],
    );
}

// ─── Behaviour carried over from the previous implementation ─────────────────

#[test]
fn a_subcommand_narrows_the_grant() {
    assert_eq!(keys("git diff HEAD~1"), ["shell:git diff"]);
    assert_ne!(keys("git diff HEAD~1"), keys("git push --force"));
}

#[test]
fn a_flag_is_not_part_of_the_key() {
    assert_eq!(keys("cargo test --lib"), ["shell:cargo test"]);
    assert_eq!(keys("cargo test --lib"), keys("cargo test --doc"));
    assert_eq!(keys("ls -la"), keys("ls -l"));
}

#[test]
fn a_compound_line_grants_each_command_in_it() {
    assert_eq!(keys("rm -rf __pycache__; ls -la"), ["shell:ls", "shell:rm"]);
    assert_eq!(
        keys(r#"test -f test.py && echo "created" || echo "missing""#),
        ["shell:echo", "shell:test"],
    );
    assert_eq!(
        keys("python3 test.py | od -c | tail -5"),
        ["shell:od", "shell:python3 test.py", "shell:tail"],
    );
}

/// A bare argument narrows the key, so approving one script is not approving
/// every script. A quoted or expanded one does not: it is the program's data,
/// and keying it meant every distinct `grep` pattern was its own grant.
#[test]
fn a_bare_argument_narrows_the_key_but_data_does_not() {
    assert_eq!(keys("python3 test.py"), ["shell:python3 test.py"]);
    assert_ne!(keys("python3 test.py"), keys("python3 evil.py"));
    assert_eq!(keys("python3 'test.py'"), ["shell:python3"]);
    assert_eq!(keys(r#"python3 "$SCRIPT""#), ["shell:python3"]);
    assert_eq!(keys("grep '^EXIT:' log"), keys("grep '^DONE:' log"));
}

/// A run full of progress `echo`s must not re-prompt on every one.
#[test]
fn echo_payload_is_never_folded_in() {
    assert_eq!(keys(r#"echo "exit code: $?""#), ["shell:echo"]);
    assert_eq!(keys(r#"echo "done""#), keys(r#"echo "starting""#));
}

#[test]
fn a_substituted_command_gets_its_own_key() {
    assert_eq!(
        keys("echo $(curl https://evil)"),
        ["shell:curl https://evil", "shell:echo"],
    );
    assert_eq!(
        keys("echo $(echo $(whoami))"),
        ["shell:echo", "shell:whoami"]
    );
    // Inside double quotes it still runs, so it is still lifted out.
    assert_eq!(keys(r#"echo "$(whoami)""#), ["shell:echo", "shell:whoami"]);
    // Inside single quotes nothing runs, so nothing is lifted.
    assert_eq!(keys("echo '$(whoami)'"), ["shell:echo"]);
}

#[test]
fn a_redirect_target_is_not_a_command() {
    assert_eq!(
        keys("cat /etc/passwd > /tmp/out"),
        ["shell:cat /etc/passwd"]
    );
    assert_eq!(keys("cat a >> /tmp/out"), ["shell:cat a"]);
    assert_eq!(keys("sort < /tmp/in"), ["shell:sort"]);
    assert_eq!(keys("ls>out"), ["shell:ls"]);
    assert_eq!(keys("ninja -C build &> /tmp/log"), ["shell:ninja"]);
}

/// A line whose shape is ambiguous is refused outright: "approve once, ask
/// again" is the safe direction.
#[test]
fn an_unreadable_line_is_not_grantable() {
    for command in [
        "echo `whoami`",         // backticks: nesting is ambiguous
        r#"echo "`whoami`""#,    // and inside double quotes too
        "echo $(unbalanced",     // no closing paren
        "echo 'unterminated",    // no closing quote
        r#"echo "unterminated"#, // same, double
        "cat <<EOF",             // a heredoc body has its own grammar
        "echo trailing\\",       // a backslash with nothing to escape
        r#"cat "a\"#,            // the same, inside double quotes
        r#"cat "$(unbalanced""#, // a substitution opened inside a quoted word
        r#"echo $(cat "oops)"#,  // a substitution whose own contents do not read
        "$CMD --flag",           // the program itself is an expansion
        "   ",                   // no program at all
        "&& ||",                 // separators only
    ] {
        assert_eq!(
            keys(command),
            Vec::<String>::new(),
            "{command:?} must not be grantable"
        );
    }
}

/// One unreadable command poisons the whole line, rather than the line silently
/// granting only the parts that could be read.
#[test]
fn one_unreadable_command_makes_the_whole_line_ungrantable() {
    assert_eq!(keys("ls && $CMD"), Vec::<String>::new());
}

#[test]
fn keys_are_sorted_and_deduped() {
    assert_eq!(keys("ls; ls; cat a; ls"), ["shell:cat a", "shell:ls"]);
}

// ─── Escapes ─────────────────────────────────────────────────────────────────

/// A backslash outside quotes makes the next character data, so an escaped
/// separator does not split the line.
#[test]
fn an_escaped_separator_is_not_a_boundary() {
    assert_eq!(keys(r"echo a\;b"), ["shell:echo"]);
    assert_eq!(keys(r"cat my\ file"), ["shell:cat my file"]);
}

/// Inside double quotes a backslash escapes only the four characters that would
/// otherwise be special; anywhere else it stands for itself. Asserted on the
/// word values rather than the keys, because a quoted word never reaches a key.
#[test]
fn a_backslash_in_double_quotes_only_escapes_the_specials() {
    assert_eq!(arg_of(r#"cat "a\$b""#), "a$b");
    assert_eq!(arg_of(r#"cat "a\nb""#), r"a\nb");
    assert_eq!(arg_of(r#"cat "a\`b""#), "a`b");
    // An escaped `$` is not an expansion, so the word stays determined.
    assert!(second_word(r#"cat "a\$b""#).literal);
}

/// Arithmetic computes a number and runs nothing, so it must not be read as a
/// command substitution. `$((i*5))` produced the key `shell:i*5`.
#[test]
fn arithmetic_expansion_runs_nothing() {
    assert_eq!(keys("echo done-$((i*5))s"), ["shell:echo"]);
    assert_eq!(keys("cat $(( (a+b) * 2 ))"), ["shell:cat"]);
    assert_eq!(keys("echo $((1+2"), Vec::<String>::new(), "never closed");
    assert_eq!(keys("echo $((1+2)"), Vec::<String>::new(), "half-closed");
}

/// A builtin that launches nothing contributes no key, rather than a key naming
/// whatever data followed it.
#[test]
fn a_no_op_builtin_contributes_no_key() {
    assert_eq!(keys("export JAVA_HOME=/opt/jdk"), Vec::<String>::new());
    assert_eq!(keys("unset JAVA_HOME && ./gradlew"), ["shell:./gradlew"]);
    assert_eq!(keys("ls; break"), ["shell:ls"]);
}

/// A program that runs a command assembled somewhere this cannot see can never
/// be granted: the line names none of what will actually execute.
#[test]
fn a_program_that_runs_assembled_code_is_ungrantable() {
    assert_eq!(keys(r#"eval "$CMD""#), Vec::<String>::new());
    assert_eq!(keys("ls && eval x"), Vec::<String>::new());
    assert_eq!(keys("source ./env.sh"), Vec::<String>::new());
}

/// A bare `$` that is not a substitution still marks the word as expanded.
#[test]
fn a_bare_expansion_marks_the_word() {
    assert_eq!(keys("cat $HOME/notes"), ["shell:cat"]);
    assert_eq!(keys(r#"cat "$HOME/notes""#), ["shell:cat"]);
}

// ─── The unit pieces ─────────────────────────────────────────────────────────

#[test]
fn segment_key_reports_all_three_states() {
    assert_eq!(segment_key(&[]), SegmentKey::NothingRuns);
    assert_eq!(segment_key(&[word("done")]), SegmentKey::NothingRuns);
    assert_eq!(segment_key(&[expansion("$CMD")]), SegmentKey::Unreadable);
    assert_eq!(
        segment_key(&[word("ls")]),
        SegmentKey::Key("ls".to_string())
    );
    assert_eq!(
        segment_key(&[word("then"), word("git"), word("status")]),
        SegmentKey::Key("git status".to_string())
    );
    // A line of nothing but keywords and assignments runs nothing.
    assert_eq!(
        segment_key(&[word("then"), word("A=1")]),
        SegmentKey::NothingRuns
    );
}

#[test]
fn is_assignment_accepts_only_a_shell_variable_name() {
    assert!(is_assignment(&word("FOO=1")));
    assert!(is_assignment(&word("_x=")));
    assert!(!is_assignment(&word("cargo")), "no equals sign");
    assert!(!is_assignment(&word("=1")), "empty name");
    assert!(
        !is_assignment(&word("1FOO=x")),
        "names cannot start with a digit"
    );
    assert!(
        !is_assignment(&word("a-b=x")),
        "hyphen is not a name character"
    );
}

#[test]
fn folds_into_key_rejects_every_kind_of_non_program() {
    assert!(folds_into_key("git", &word("status")));
    assert!(!folds_into_key("git", &expansion("$SUB")), "an expansion");
    assert!(!folds_into_key("grep", &quoted("^EXIT:")), "quoted data");
    assert!(!folds_into_key("cd", &word("/tmp")), "a never-fold program");
    assert!(!folds_into_key("cat", &word("")), "an empty word");
    assert!(!folds_into_key("ls", &word("-la")), "a flag");
    assert!(!folds_into_key("sleep", &word("55")), "a number");
}

/// An empty argument is a real word - `cat ""` - and must not be folded.
#[test]
fn an_empty_argument_is_not_folded() {
    assert_eq!(keys(r#"cat """#), ["shell:cat"]);
}

#[test]
fn take_substitution_balances_nested_parens() {
    let mut chars = "a $(b) c) rest".chars().peekable();
    assert_eq!(take_substitution(&mut chars).as_deref(), Some("a $(b) c"));
    assert_eq!(chars.collect::<String>(), " rest");

    let mut unbalanced = "a (b".chars().peekable();
    assert_eq!(take_substitution(&mut unbalanced), None);
}

// ─── is_valid_prefix ─────────────────────────────────────────────────────────

/// A config entry is valid exactly when it derives back to itself, so there is
/// one grammar rather than two that could drift apart.
#[test]
fn a_valid_prefix_is_one_that_derives_back_to_itself() {
    for good in ["ls", "cargo test", "git status", "./gradlew"] {
        assert!(is_valid_prefix(good), "{good:?} should be a valid entry");
    }
    for bad in [
        "ls; curl evil", // more than one command
        "ls > /tmp/x",   // a redirect
        "sleep 5",       // an argument that would not be keyed
        "ls -la",        // a flag that would not be keyed
        "$CMD",          // an expansion
        "",              // nothing
        "for",           // a keyword
    ] {
        assert!(!is_valid_prefix(bad), "{bad:?} should be rejected");
    }
}
