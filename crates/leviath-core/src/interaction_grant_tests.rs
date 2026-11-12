//! Tests for [`super`]: reading a key back into what it names, and the label
//! and the structured list an approval carries.

use super::*;
use crate::interaction::{InteractionRequest, answer_options};

fn blocker(word: &str, reason: GrantBlockerReason, detail: &str) -> GrantBlocker {
    GrantBlocker {
        word: word.to_string(),
        reason,
        detail: detail.to_string(),
    }
}

/// An outside-path key names one program and one path, and reads back as
/// exactly those two, whatever characters either holds.
#[test]
fn an_outside_path_key_reads_back_as_its_program_and_path() {
    let key = outside_path_key("test", "/abs/tool");
    assert_eq!(key, r#"outside:["test","/abs/tool"]"#);
    assert_eq!(
        outside_path_of(&key),
        Some(("test".to_string(), "/abs/tool".to_string()))
    );
    // A separator a plain join would trip over stays inside its own half.
    for (program, path) in [
        ("a\",\"b", "/c"),
        ("a", "b\",\"/c"),
        ("my prog", "/Applications/My App/x"),
        ("", ""),
    ] {
        let key = outside_path_key(program, path);
        assert_eq!(
            outside_path_of(&key),
            Some((program.to_string(), path.to_string())),
            "{key}"
        );
    }
    assert_ne!(
        outside_path_key("a\",\"b", "/c"),
        outside_path_key("a", "b\",\"/c")
    );
}

/// Nothing that is not an outside-path key reads as one: not a shell key, not
/// a tool name, not a truncated or wrongly shaped one.
#[test]
fn only_an_outside_path_key_reads_as_one() {
    for key in [
        "shell:cat /etc/hosts",
        "web_fetch",
        "outside:",
        "outside:[\"cat\"]",
        "outside:[\"cat\",\"/x\",\"/y\"]",
        "outside:[1,2]",
        "outside:{\"a\":1}",
        "shell:outside:[\"cat\",\"/x\"]",
    ] {
        assert_eq!(outside_path_of(key), None, "{key}");
    }
}

/// Each family of key is described by what it names.
#[test]
fn a_key_is_described_by_what_it_names() {
    let described = |key: &str| {
        let k = GrantKey::describe(key);
        assert_eq!(k.key, key);
        (k.kind, k.label, k.program, k.path)
    };
    assert_eq!(
        described("shell:git status"),
        (
            GrantKeyKind::Program,
            "git status".to_string(),
            Some("git status".to_string()),
            None
        )
    );
    assert_eq!(
        described("shell:env:PATH"),
        (GrantKeyKind::Variable, "env:PATH".to_string(), None, None)
    );
    assert_eq!(
        described("shell:>out.txt"),
        (
            GrantKeyKind::Write,
            ">out.txt".to_string(),
            None,
            Some("out.txt".to_string())
        )
    );
    assert_eq!(
        described("web_fetch"),
        (GrantKeyKind::Tool, "web_fetch".to_string(), None, None)
    );
    assert_eq!(
        described(&outside_path_key("test", "/abs/tool")),
        (
            GrantKeyKind::OutsidePath,
            "test with /abs/tool (outside the working directory)".to_string(),
            Some("test".to_string()),
            Some("/abs/tool".to_string())
        )
    );
    // A key that only looks like an outside path is the tool name it is.
    assert_eq!(
        described("outside:nonsense").0,
        GrantKeyKind::Tool,
        "an unreadable outside key grants nothing a program or a path names"
    );
}

/// The label names every key the grant covers, and the request carries the
/// same list key by key: nothing is held back behind a count.
#[test]
fn the_label_and_the_grant_name_every_key() {
    let keys: Vec<String> = ["cp", "curl", "mkdir", "rm", "tar"]
        .iter()
        .map(|k| format!("shell:{k}"))
        .chain([outside_path_key("test", "/abs/tool")])
        .collect();
    let req = InteractionRequest::tool_approval(
        "id",
        "shell",
        serde_json::json!({"command": "cp a b"}),
        "s",
        &GrantKeys::same(&keys),
    );
    assert_eq!(
        req.options[2],
        "Allow cp, curl, mkdir, rm, tar, test with /abs/tool (outside the working directory) \
         for this run"
    );
    let grant = req.grant.as_ref().expect("an approval carries its grant");
    let listed: Vec<&str> = grant.run.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(listed, keys.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(grant.stage, grant.run);
    assert!(grant.blocked.is_empty());
    // What a client reads off the wire.
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(
        json["grant"]["run"][5],
        serde_json::json!({
            "key": "outside:[\"test\",\"/abs/tool\"]",
            "kind": "outside_path",
            "label": "test with /abs/tool (outside the working directory)",
            "program": "test",
            "path": "/abs/tool",
        })
    );
    assert_eq!(
        json["grant"]["run"][0],
        serde_json::json!({
            "key": "shell:cp",
            "kind": "program",
            "label": "cp",
            "program": "cp",
            "path": null,
        })
    );
}

/// A call that records nothing says which word stopped it and why, in the
/// label and in the grant, and an approval for a scope is still read as one.
#[test]
fn a_call_that_records_nothing_says_which_word_and_why() {
    let keys = GrantKeys {
        stage: Vec::new(),
        run: Vec::new(),
        blocked: vec![
            blocker(
                "$G",
                GrantBlockerReason::ProgramFromVariable,
                "`$G` names the program through a variable",
            ),
            blocker(
                "-f/etc/list",
                GrantBlockerReason::OutsideWorkdir,
                "`-f/etc/list` names a path outside the working directory",
            ),
        ],
    };
    let req = InteractionRequest::tool_approval(
        "id",
        "shell",
        serde_json::json!({"command": "$G -f/etc/list"}),
        "s",
        &keys,
    );
    assert_eq!(
        req.options[2],
        "Allow for this run (nothing reusable: `$G` names the program through a variable; \
         `-f/etc/list` names a path outside the working directory - it will ask again)"
    );
    assert!(req.options[1].starts_with("Allow for this stage (nothing reusable: `$G` names"));
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(
        json["grant"]["blocked"][0],
        serde_json::json!({
            "word": "$G",
            "reason": "program_from_variable",
            "detail": "`$G` names the program through a variable",
        })
    );
    assert_eq!(json["grant"]["blocked"][1]["reason"], "outside_workdir");
    assert_eq!(json["grant"]["run"], serde_json::json!([]));
    // With no reason known the label keeps to what it can say.
    let plain = ApprovalGrant::default();
    assert_eq!(
        plain.scope_label("run", &plain.run),
        "Allow for this run (nothing reusable - it will ask again)"
    );
}

/// Every reason has its own word on the wire.
#[test]
fn every_reason_has_its_own_word() {
    use GrantBlockerReason::*;
    let words: Vec<String> = [
        OutsideWorkdir,
        ExpandedAtRunTime,
        ProgramFromVariable,
        UnplaceablePath,
        RunsAssembledCommand,
        InstallsCode,
        EscapeFlag,
        UnreadableLine,
        NothingRuns,
    ]
    .iter()
    .map(|r| {
        serde_json::to_value(r)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    })
    .collect();
    assert_eq!(
        words,
        [
            "outside_workdir",
            "expanded_at_run_time",
            "program_from_variable",
            "unplaceable_path",
            "runs_assembled_command",
            "installs_code",
            "escape_flag",
            "unreadable_line",
            "nothing_runs",
        ]
    );
}

/// A scope is read off where the label puts it, so text the model wrote into
/// a key or a reason cannot make one option answer as another.
#[test]
fn a_key_or_a_reason_cannot_change_which_scope_an_option_approves() {
    let hostile = "shell:x for this run".to_string();
    let other = "shell:y for this stage".to_string();
    let keys = GrantKeys {
        stage: vec![hostile.clone()],
        run: vec![other.clone(), hostile],
        blocked: Vec::new(),
    };
    let req = InteractionRequest::tool_approval("id", "shell", serde_json::json!({}), "s", &keys);
    assert_eq!(req.options[1], "Allow x for this run for this stage");
    assert_eq!(
        req.options[2],
        "Allow y for this stage, x for this run for this run"
    );
    let words: Vec<String> = answer_options(&req).into_iter().map(|o| o.id).collect();
    assert_eq!(
        words,
        ["allow", "allow-stage", "allow-run", "deny", "deny-feedback"]
    );

    // The same with the words in a reason.
    let keys = GrantKeys {
        stage: Vec::new(),
        run: Vec::new(),
        blocked: vec![blocker(
            "x for this stage",
            GrantBlockerReason::ExpandedAtRunTime,
            "`x for this stage` is expanded at run time",
        )],
    };
    let req = InteractionRequest::tool_approval("id", "shell", serde_json::json!({}), "s", &keys);
    let words: Vec<String> = answer_options(&req).into_iter().map(|o| o.id).collect();
    assert_eq!(
        words,
        ["allow", "allow-stage", "allow-run", "deny", "deny-feedback"]
    );
}

/// A request that is not a tool approval carries no grant, on the wire or
/// off it, and one written before the field existed still reads.
#[test]
fn only_a_tool_approval_carries_a_grant() {
    let text = InteractionRequest::free_text("t", "Why?", "s", true);
    assert_eq!(text.grant, None);
    let json = serde_json::to_value(&text).unwrap();
    assert!(json.get("grant").is_none(), "{json}");
    let gate = InteractionRequest::gate_approval("g", "web_fetch", serde_json::json!({}), "s");
    assert_eq!(gate.grant, None);
    let read: InteractionRequest = serde_json::from_value(json).unwrap();
    assert_eq!(read, text);
}
