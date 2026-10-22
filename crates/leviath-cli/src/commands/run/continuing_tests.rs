//! `lev run continue` and `lev run fork` against a daemon that answers each
//! way it can.

use super::*;
use leviath_runtime::control_socket::{bind_control_listener, control_id};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Ask a daemon that answers `reply` to continue run `r`, returning the
/// outcome and the request it was sent.
async fn asked(
    reply: &str,
    fork: bool,
    args: ContinueArgs,
) -> (anyhow::Result<()>, serde_json::Value) {
    let dir = tempfile::tempdir().unwrap();
    let id = control_id(dir.path());
    let mut listener = bind_control_listener(&id).unwrap();
    let reply = reply.to_string();
    let server = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap().unwrap();
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut lines = BufReader::new(read_half).lines();
        let request = lines.next_line().await.unwrap().unwrap();
        write_half.write_all(reply.as_bytes()).await.unwrap();
        write_half.write_all(b"\n").await.unwrap();
        request
    });
    let outcome = execute(&ControlClient::new(id), &args, fork).await;
    let request = serde_json::from_str(&server.await.unwrap()).unwrap();
    (outcome, request)
}

fn args() -> ContinueArgs {
    ContinueArgs {
        run_id: "r-1".to_string(),
        message: "make the cape red".to_string(),
        region: Some("notes".to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_continuation_sends_the_message_and_says_which_run_goes_on() {
    let reply = r#"{"result":"continued","run_id":"r-1","segment":2}"#;
    let (outcome, request) = asked(reply, false, args()).await;
    assert!(outcome.is_ok());
    assert_eq!(request["op"], "continue");
    assert_eq!(request["run_id"], "r-1");
    assert_eq!(request["text"], "make the cape red");
    assert_eq!(request["region"], "notes");
    assert!(request.get("fork").is_none());

    let reply = r#"{"result":"continued","run_id":"r-2","segment":1,"forked_from":"r-1"}"#;
    let (outcome, request) = asked(reply, true, args()).await;
    assert!(outcome.is_ok());
    assert_eq!(request["fork"], true);
}

#[tokio::test]
async fn a_refusal_or_an_unexpected_answer_fails_the_command() {
    let refused = serde_json::to_string(&ControlResponse::Rejected {
        issues: leviath_runtime::continuation::refused(
            leviath_runtime::spec::issues::IssueCode::Conflict,
            leviath_runtime::spec::issues::SpecPath::root(),
            "run 'r-1' has not finished",
        ),
    })
    .unwrap();
    let err = asked(&refused, false, args()).await.0.unwrap_err();
    assert!(err.to_string().contains("has not finished"), "{err}");

    let error = r#"{"result":"error","message":"daemon is shutting down"}"#;
    let err = asked(error, false, args()).await.0.unwrap_err();
    assert!(err.to_string().contains("shutting down"), "{err}");

    let other = r#"{"result":"ok","ok":true}"#;
    let err = asked(other, false, args()).await.0.unwrap_err();
    assert!(err.to_string().contains("something else"), "{err}");
}

#[tokio::test]
async fn an_attachment_that_does_not_read_stops_before_the_daemon_is_asked() {
    let bad = ContinueArgs {
        attach: vec!["/no/such/file.png".to_string()],
        ..args()
    };
    let dir = tempfile::tempdir().unwrap();
    let client = ControlClient::new(control_id(dir.path()));
    assert!(execute(&client, &bad, false).await.is_err());
}

#[tokio::test]
async fn no_daemon_to_ask_fails_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let client = ControlClient::new(control_id(dir.path()));
    assert!(execute(&client, &args(), false).await.is_err());
}
