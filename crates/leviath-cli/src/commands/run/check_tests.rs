//! `lev run --check`: the summary as printed, and every answer the daemon
//! can give.

use std::collections::BTreeMap;

use leviath_runtime::control_socket::{ControlId, bind_control_listener, control_id};
use leviath_runtime::spec::inputs::InputValues;
use leviath_runtime::spec::launch::LaunchPolicy;
use leviath_runtime::spec::names::{
    BlueprintName, BlueprintPath, BlueprintRef, ChoiceName, InputName, ModelId, ProviderName,
    StageName, ToolName,
};
use leviath_runtime::spec::summary::StageSummary;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::*;

fn summary() -> SpawnSummary {
    SpawnSummary {
        title: "coder".to_string(),
        origin: SpecOrigin::Blueprint {
            blueprint: BlueprintRef::parse("coder").unwrap(),
            version: "1.2.0".to_string(),
        },
        entry_stage: StageName::new("plan").unwrap(),
        stages: vec![
            StageSummary {
                stage: StageName::new("plan").unwrap(),
                provider: ProviderName::new("anthropic").unwrap(),
                model: ModelId::new("claude-sonnet-5").unwrap(),
                tools: vec![
                    ToolName::new("read_file").unwrap(),
                    ToolName::new("grep").unwrap(),
                ],
            },
            StageSummary {
                stage: StageName::new("ship").unwrap(),
                provider: ProviderName::new("openai").unwrap(),
                model: ModelId::new("gpt-5").unwrap(),
                tools: Vec::new(),
            },
        ],
        inputs: InputValues(BTreeMap::from([
            (
                InputName::new("task").unwrap(),
                InputValue::Text(format!("fix the bug\n{}", "x".repeat(80))),
            ),
            (
                InputName::new("tone").unwrap(),
                InputValue::Choice(ChoiceName::new("calm").unwrap()),
            ),
            (InputName::new("depth").unwrap(), InputValue::Int(3)),
        ])),
        launch: LaunchPolicy {
            unattended: Unattended::Off,
            allow: Vec::new(),
            max_depth: 2,
            seed_commands: true,
            capture_model_input: false,
        },
        workdir: std::path::PathBuf::from("/work"),
    }
}

#[test]
fn a_summary_reads_as_the_run_it_would_be() {
    let text = check_report(&summary(), false);
    assert!(
        text.starts_with("coder would run (blueprint coder, version 1.2.0)"),
        "{text}"
    );
    assert!(text.contains("  starts in: plan"), "{text}");
    assert!(text.contains("  workdir:   /work"), "{text}");
    assert!(text.contains("  unattended: no"), "{text}");
    assert!(!text.contains("allowed:"), "{text}");
    assert!(text.contains("    depth = 3"), "{text}");
    assert!(text.contains("    tone = calm"), "{text}");
    assert!(text.contains("    task = \"fix the bug xxx"), "{text}");
    assert!(text.contains("x\"..."), "a long value is cut: {text}");
    assert!(
        text.contains("    plan  anthropic/claude-sonnet-5  tools: read_file, grep"),
        "{text}"
    );
    assert!(text.contains("    ship  openai/gpt-5  no tools"), "{text}");

    let mut other = summary();
    other.origin = SpecOrigin::BlueprintFile {
        path: BlueprintPath::new("/agents/coder").unwrap(),
        name: BlueprintName::new("coder").unwrap(),
        digest: None,
        version: "0.1.0".to_string(),
    };
    other.launch.unattended = Unattended::All;
    other.launch.allow = vec![ToolName::new("shell").unwrap()];
    other.inputs = InputValues::default();
    let text = check_report(&other, false);
    assert!(
        text.contains("(blueprint in /agents/coder, version 0.1.0)"),
        "{text}"
    );
    assert!(text.contains("  unattended: yes"), "{text}");
    assert!(text.contains("  allowed:   shell"), "{text}");
    assert!(!text.contains("inputs:"), "{text}");
    other.origin = SpecOrigin::Raw;
    other.launch.unattended =
        Unattended::Profile(leviath_runtime::spec::names::ProfileName::new("careful").unwrap());
    let text = check_report(&other, false);
    assert!(text.contains("(a graph from the request)"), "{text}");
    assert!(text.contains("under the 'careful' profile"), "{text}");

    let json: serde_json::Value = serde_json::from_str(&check_report(&summary(), true)).unwrap();
    assert_eq!(json["title"], "coder");
}

/// A daemon that answers one request with `reply`, at a fresh id.
fn daemon(dir: &std::path::Path, reply: String) -> (ControlId, tokio::task::JoinHandle<()>) {
    let id = control_id(dir);
    let mut listener = bind_control_listener(&id).unwrap();
    let handle = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap().unwrap();
        let (read, mut write) = tokio::io::split(stream);
        let _ = BufReader::new(read).lines().next_line().await;
        write.write_all(reply.as_bytes()).await.unwrap();
        write.write_all(b"\n").await.unwrap();
    });
    (id, handle)
}

/// Ask a daemon answering `reply` to check a run.
async fn check(reply: String, json: bool) -> anyhow::Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let (id, server) = daemon(dir.path(), reply);
    let result = send_check(&ControlClient::new(id), &LocalRun::default(), json).await;
    server.await.unwrap();
    result
}

#[tokio::test]
async fn every_answer_to_a_check_is_reported() {
    let valid = serde_json::to_string(&ControlResponse::Valid {
        summary: Box::new(summary()),
    })
    .unwrap();
    check(valid.clone(), false).await.unwrap();
    check(valid, true).await.unwrap();

    for json in [false, true] {
        let err = check(
            crate::test_support::rejected_reply("no such stage").to_string(),
            json,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().starts_with("1 problem with this run:"),
            "{err}"
        );
        assert!(err.to_string().contains("no such stage"), "{err}");
    }
    let err = check(r#"{"result":"error","message":"boom"}"#.to_string(), false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("the check failed: boom"), "{err}");
    let err = check(r#"{"result":"ok","ok":true}"#.to_string(), false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unexpected"), "{err}");

    let dir = tempfile::tempdir().unwrap();
    let id = control_id(&dir.path().join("nobody"));
    let err = send_check(&ControlClient::new(id), &LocalRun::default(), false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not reachable"), "{err}");
}
