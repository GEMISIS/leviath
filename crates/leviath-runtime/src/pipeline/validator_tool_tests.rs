use super::*;

/// `{"ok": true}` passes, `{"ok": false, "reason"}` refuses with the reason,
/// and a refusal without one still refuses, naming the tool.
#[test]
fn a_verdict_is_read_from_the_tools_answer() {
    assert_eq!(verdict_of("check", Some(r#"{"ok": true}"#)), Verdict::Valid);
    assert_eq!(
        verdict_of(
            "check",
            Some(" {\"ok\": false, \"reason\": \"no title\"}\n")
        ),
        Verdict::Invalid("no title".to_string())
    );
    assert_eq!(
        verdict_of("check", Some(r#"{"ok": false, "reason": "  "}"#)),
        Verdict::Invalid("check refused it without saying why".to_string())
    );
    assert_eq!(
        verdict_of("check", Some(r#"{"ok": false}"#)),
        Verdict::Invalid("check refused it without saying why".to_string())
    );
}

/// A failed or refused call, no answer, and an answer that is not a verdict
/// are a validator that could not run, never a pass.
#[test]
fn anything_else_is_a_validator_that_could_not_run() {
    for (answer, says) in [
        (None, "never answered"),
        (
            Some("[error] MCP server 'pw' is not connected"),
            "not connected",
        ),
        (Some("[denied] not allowed"), "not allowed"),
        (Some("looks fine to me"), "not a verdict"),
        (Some(r#"{"ok": "yes"}"#), "not a verdict"),
        (Some(r#"["ok"]"#), "not a verdict"),
    ] {
        match verdict_of("check", answer) {
            Verdict::Unusable(reason) => {
                assert!(reason.starts_with("check: "), "{reason}");
                assert!(reason.contains(says), "{reason}");
            }
            other => panic!("{answer:?} read as {other:?}"),
        }
    }
}

#[test]
fn lane_ids_cannot_collide_with_a_providers() {
    assert_eq!(lane_id("toolu_1"), "validator-toolu_1");
}
