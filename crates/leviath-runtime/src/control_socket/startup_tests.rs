use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::*;
use crate::components::AgentStatus;
use crate::control_socket::{
    ControlClient, ControlRequest, ControlResponse, StartupEvent, bind_control_listener, connect,
    control_id,
};

/// A board says the step it is on, counted or not.
#[test]
fn a_board_says_its_step_and_how_far_along_it_is() {
    let board = StartupBoard::default();
    assert_eq!(board.current(), StartupProgress::default());
    board.begin("converting runs", 982);
    board.done(412);
    board.detail("/home/.leviath/backups/0.6.4-1");
    let now = board.current();
    assert_eq!(now.to_string(), "converting runs 412/982");
    assert_eq!(
        now.detail.as_deref(),
        Some("/home/.leviath/backups/0.6.4-1")
    );
    board.begin("connecting MCP servers", 0);
    let now = board.current();
    assert_eq!(now.to_string(), "connecting MCP servers");
    assert_eq!(now.detail, None, "a new step starts with no detail");
}

/// One request down a connection the gate serves, and its reply.
async fn ask(gate: &ControlGate, lines: &[ControlRequest]) -> Vec<ControlResponse> {
    let dir = tempfile::tempdir().unwrap();
    let id = control_id(dir.path());
    let mut listener = bind_control_listener(&id).unwrap();
    let served = gate.clone();
    tokio::spawn(async move {
        let stream = listener.accept().await.unwrap().unwrap();
        let _ = served
            .serve(stream, None, DaemonIdentity::this_process("test"))
            .await;
    });
    let stream = connect(&id).await.unwrap();
    let (read, mut write) = tokio::io::split(stream);
    let mut replies = BufReader::new(read).lines();
    let mut out = Vec::new();
    for request in lines {
        let mut line = serde_json::to_string(request).unwrap();
        line.push('\n');
        write.write_all(line.as_bytes()).await.unwrap();
        let reply = replies.next_line().await.unwrap().unwrap();
        out.push(serde_json::from_str(&reply).unwrap());
    }
    write.write_all(b"not json\n").await.unwrap();
    let reply = replies.next_line().await.unwrap().unwrap();
    out.push(serde_json::from_str(&reply).unwrap());
    out
}

/// While the gate is closed, every request but `authenticate` and a
/// subscription is answered with the start-up step, and nothing reaches a
/// host; a line that does not parse is still refused as one.
#[tokio::test]
async fn a_closed_gate_answers_every_request_with_the_step_under_way() {
    let board = StartupBoard::default();
    board.begin("converting runs", 10);
    board.done(3);
    let gate = ControlGate::new(board);
    let replies = ask(
        &gate,
        &[
            ControlRequest::Authenticate {
                token: String::new(),
                hello: true,
            },
            ControlRequest::List,
            ControlRequest::Status { run_id: "r".into() },
        ],
    )
    .await;
    assert!(matches!(replies[0], ControlResponse::Welcome { .. }));
    let progress = StartupProgress {
        step: "converting runs".to_string(),
        done: 3,
        total: 10,
        detail: None,
    };
    for reply in &replies[1..3] {
        assert_eq!(
            reply,
            &ControlResponse::Starting {
                progress: progress.clone()
            }
        );
    }
    let ControlResponse::Error { message } = &replies[3] else {
        panic!("{:?}", replies[3]);
    };
    assert!(
        message.starts_with(super::super::INVALID_REQUEST),
        "{message}"
    );
}

/// A subscription taken while the daemon starts is not turned away: it is
/// sent the world's events from the first, once the gate opens. Turned away,
/// it was a stream that never carried anything, since a subscriber does not
/// ask again, and a gateway that reconnected during a restart heard nothing
/// more from the daemon until the next one.
#[tokio::test]
async fn a_subscription_taken_while_starting_carries_the_worlds_first_events() {
    let dir = tempfile::tempdir().unwrap();
    let id = control_id(dir.path());
    let mut listener = bind_control_listener(&id).unwrap();
    let gate = ControlGate::new(StartupBoard::default());
    let serving = gate.clone();
    tokio::spawn(async move {
        let stream = listener.accept().await.unwrap().unwrap();
        let _ = serving
            .serve(stream, None, DaemonIdentity::this_process("test"))
            .await;
    });
    let mut stream = ControlClient::new(id).subscribe().await.unwrap();
    let events = gate.events.clone();
    leviath_testkit::wait_until("the stream is open", || events.subscribers() > 0).await;
    let (world, _keep) = broadcast::channel(4);
    gate.open(mpsc::unbounded_channel().0, world.clone());
    let first = WorldEvent::Log {
        run_id: "r".into(),
        agent_id: "a".into(),
        line: "the world's first event".into(),
    };
    world.send(first.clone()).unwrap();
    assert_eq!(stream.next().await, Some(first));
    assert_eq!(stream.cursor().map(|c| c.after), Some(1));
}

/// A host that answers a status query, and counts the queries.
fn fake_host() -> (mpsc::UnboundedSender<ControlOp>, Arc<Mutex<usize>>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let asked = Arc::new(Mutex::new(0));
    let count = asked.clone();
    tokio::spawn(async move {
        while let Some(op) = rx.recv().await {
            if let ControlOp::Status { reply, .. } = op {
                *count.lock().unwrap() += 1;
                let _ = reply.send(Some(AgentStatus::Active));
            }
        }
    });
    (tx, asked)
}

/// An open gate sends each connection to the host.
#[tokio::test]
async fn an_open_gate_serves_through_the_host() {
    let gate = ControlGate::new(StartupBoard::default());
    let (tx, asked) = fake_host();
    gate.open(tx, broadcast::channel(1).0);
    let replies = ask(&gate, &[ControlRequest::Status { run_id: "r".into() }]).await;
    assert_eq!(
        replies[0],
        ControlResponse::Status {
            status: Some(AgentStatus::Active)
        }
    );
    assert_eq!(*asked.lock().unwrap(), 1);
}

/// A client asking a daemon that is still starting shows each answer to its
/// watch, asks again until the daemon is ready, and then gets the real
/// reply, with the watch told the wait is over.
#[tokio::test]
async fn a_client_waits_out_a_starting_daemon_and_shows_it_the_way() {
    let dir = tempfile::tempdir().unwrap();
    let id = control_id(dir.path());
    let mut listener = bind_control_listener(&id).unwrap();
    let board = StartupBoard::default();
    board.begin("upgrading blueprints", 2);
    let gate = ControlGate::new(board.clone());
    let serving = gate.clone();
    tokio::spawn(async move {
        while let Ok(Some(stream)) = listener.accept().await {
            let gate = serving.clone();
            tokio::spawn(async move {
                let _ = gate
                    .serve(stream, None, DaemonIdentity::this_process("test"))
                    .await;
            });
        }
    });
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = seen.clone();
    let (tx, asked) = fake_host();
    let opened = gate.clone();
    let client = ControlClient::new(id.clone()).with_startup_watch(Arc::new(move |event| {
        let line = match event {
            StartupEvent::Progress(progress) => progress.to_string(),
            StartupEvent::Ready => "ready".to_string(),
        };
        let mut log = log.lock().unwrap();
        log.push(line);
        // The daemon gets on with it while the client waits, and is ready
        // after the second answer.
        board.done(log.len() as u64);
        if log.len() == 2 {
            opened.open(tx.clone(), broadcast::channel(1).0);
        }
    }));
    client.wait_until_started().await.unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        [
            "upgrading blueprints 0/2",
            "upgrading blueprints 1/2",
            "ready"
        ]
    );
    assert_eq!(
        *asked.lock().unwrap(),
        1,
        "the request reached the host once"
    );

    // A ready daemon is not waited on, and a client nobody watches waits in
    // silence.
    seen.lock().unwrap().clear();
    client.wait_until_started().await.unwrap();
    assert!(seen.lock().unwrap().is_empty());
    ControlClient::new(id).wait_until_started().await.unwrap();
}

/// The daemon's accept loop serves every connection through the gate, with
/// the token checked.
#[tokio::test]
async fn the_accept_loop_serves_each_connection_through_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    let id = control_id(dir.path());
    let listener = bind_control_listener(&id).unwrap();
    let token = crate::control_socket::ControlToken::create(dir.path()).unwrap();
    let gate = ControlGate::new(StartupBoard::default());
    let (tx, asked) = fake_host();
    gate.open(tx, broadcast::channel(1).0);
    let task = gate.accept_all(
        listener,
        token.clone(),
        DaemonIdentity::this_process("test"),
    );
    let client = ControlClient::new(id).with_token(token);
    for _ in 0..2 {
        client.wait_until_started().await.unwrap();
    }
    assert_eq!(*asked.lock().unwrap(), 2);
    task.abort();
}
