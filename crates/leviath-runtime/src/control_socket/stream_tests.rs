//! The numbered event stream: where a subscription starts, what one picking
//! up a dropped stream is sent again, and when a stream ends.

use super::*;
use tokio::io::{AsyncBufReadExt, DuplexStream, ReadHalf, WriteHalf};

fn completed(run_id: &str) -> WorldEvent {
    WorldEvent::Completed {
        run_id: run_id.to_string(),
        agent_id: "a".to_string(),
        status: "complete".to_string(),
        final_output: None,
    }
}

/// A log holding `n` completions, `run-1` to `run-n`, recorded synchronously.
fn log_of(n: u64) -> EventLog {
    let log = EventLog::new();
    for i in 1..=n {
        log.push(completed(&format!("run-{i}")));
    }
    log
}

/// A stream served on one end of a duplex, read line by line on the other.
struct Open {
    lines: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
    /// The subscriber's side; dropping it is the subscriber hanging up.
    say: WriteHalf<DuplexStream>,
    served: tokio::task::JoinHandle<std::io::Result<()>>,
}

fn open(log: &EventLog, from: Option<EventCursor>) -> Open {
    let (client, server) = tokio::io::duplex(1 << 20);
    let (server_read, mut server_write) = tokio::io::split(server);
    let (client_read, say) = tokio::io::split(client);
    let log = log.clone();
    let served = tokio::spawn(async move {
        let mut read = BufReader::new(server_read).lines();
        stream_events(&mut read, &mut server_write, log, from).await
    });
    Open {
        lines: BufReader::new(client_read).lines(),
        say,
        served,
    }
}

impl Open {
    async fn line(&mut self) -> serde_json::Value {
        let line = self.lines.next_line().await.unwrap().expect("a line");
        serde_json::from_str(&line).unwrap()
    }

    /// The opening line's `after`, checking it names `log`'s session.
    async fn opened(&mut self, log: &EventLog) -> u64 {
        let first = self.line().await;
        assert_eq!(first["result"], "subscribed");
        assert_eq!(first["session"], log.session());
        first["after"].as_u64().unwrap()
    }

    /// The next event line's number and run id.
    async fn event(&mut self) -> (u64, String) {
        let line = self.line().await;
        let run = line["run_id"].as_str().unwrap().to_string();
        (line["seq"].as_u64().unwrap(), run)
    }
}

/// A first subscription is not told what happened before it: a gateway that
/// starts against a running daemon must not fire again the webhooks of runs
/// that finished before it was there.
#[tokio::test]
async fn a_first_subscription_is_sent_only_what_follows() {
    let log = log_of(2);
    let mut stream = open(&log, None);
    assert_eq!(stream.opened(&log).await, 2);
    log.push(completed("run-3"));
    assert_eq!(stream.event().await, (3, "run-3".to_string()));
}

/// Picking up a dropped stream on the same daemon sends what came after the
/// last event it was sent, and nothing it already had.
#[tokio::test]
async fn a_stream_picked_up_on_the_same_daemon_is_sent_what_it_missed() {
    let log = log_of(3);
    let from = EventCursor {
        session: log.session().to_string(),
        after: 1,
    };
    let mut stream = open(&log, Some(from));
    assert_eq!(stream.opened(&log).await, 1);
    assert_eq!(stream.event().await, (2, "run-2".to_string()));
    assert_eq!(stream.event().await, (3, "run-3".to_string()));
}

/// The case the log exists for: the daemon was killed and started again,
/// and a run it restored finished before the gateway was back. The gateway's
/// cursor names the dead daemon's session, and none of the new one's events
/// were seen, so all of them are sent, the completion among them.
#[tokio::test]
async fn a_stream_picked_up_from_a_replaced_daemon_is_sent_everything_kept() {
    let log = log_of(2);
    let from = EventCursor {
        session: "a daemon that has since died".to_string(),
        after: 40,
    };
    let mut stream = open(&log, Some(from));
    assert_eq!(stream.opened(&log).await, 0);
    assert_eq!(stream.event().await, (1, "run-1".to_string()));
    assert_eq!(stream.event().await, (2, "run-2".to_string()));
}

/// A cursor past the end of this session is taken as at its end, not as a
/// promise of events to skip when they arrive.
#[tokio::test]
async fn a_cursor_past_the_end_starts_at_the_end() {
    let log = log_of(1);
    let from = EventCursor {
        session: log.session().to_string(),
        after: 9,
    };
    let mut stream = open(&log, Some(from));
    assert_eq!(stream.opened(&log).await, 1);
    log.push(completed("run-2"));
    assert_eq!(stream.event().await, (2, "run-2".to_string()));
}

/// The log keeps the newest [`KEPT`] events and lets the oldest go.
#[test]
fn the_log_keeps_the_newest_events() {
    let total = KEPT as u64 + 5;
    let log = log_of(total);
    let (kept, closed) = log.since(0);
    assert_eq!(kept.len(), KEPT);
    assert_eq!(kept.first().map(|(seq, _)| *seq), Some(6));
    assert_eq!(kept.last().map(|(seq, _)| *seq), Some(total));
    assert!(!closed);
}

/// Recording copies what the world sends, in order, numbering it; once the
/// world drops its sender, a stream is sent what was kept and then ends.
#[tokio::test]
async fn a_recorded_world_is_streamed_and_its_end_ends_the_stream() {
    let (tx, _keep) = broadcast::channel(16);
    let log = EventLog::recording(&tx);
    let mut stream = open(&log, None);
    assert_eq!(stream.opened(&log).await, 0);
    tx.send(completed("first")).unwrap();
    tx.send(completed("second")).unwrap();
    drop(tx);
    assert_eq!(stream.event().await, (1, "first".to_string()));
    assert_eq!(stream.event().await, (2, "second".to_string()));
    assert!(stream.lines.next_line().await.unwrap().is_none(), "ended");
    stream.served.await.unwrap().unwrap();
}

/// Events that overran the world's channel before the recorder read them are
/// lost, and recording goes on with the next.
#[tokio::test]
async fn a_lagging_recorder_skips_what_it_missed_and_carries_on() {
    let (tx, _keep) = broadcast::channel(1);
    let log = EventLog::recording(&tx);
    // Sent before the recorder task first runs: only the last fits.
    tx.send(completed("lost")).unwrap();
    tx.send(completed("kept")).unwrap();
    drop(tx);
    let watched = log.clone();
    leviath_testkit::wait_until("the recorder saw the world close", move || {
        watched.since(0).1
    })
    .await;
    let (events, _) = log.since(0);
    assert_eq!(events, vec![(1, completed("kept"))]);
}

/// A subscriber that hangs up ends its stream even when no event comes, so
/// an idle daemon does not keep a task per gone subscriber for ever.
#[tokio::test]
async fn a_subscriber_hanging_up_ends_its_stream() {
    let log = log_of(0);
    let Open { lines, say, served } = open(&log, None);
    drop((lines, say));
    tokio::time::timeout(std::time::Duration::from_secs(5), served)
        .await
        .expect("the hang-up ends the stream promptly")
        .unwrap()
        .unwrap();
}

/// A line the subscriber sends mid-stream is chatter, not a request: the
/// stream keeps delivering events after it.
#[tokio::test]
async fn subscriber_chatter_is_ignored() {
    use tokio::io::AsyncWriteExt as _;
    let log = log_of(0);
    let mut stream = open(&log, None);
    stream.opened(&log).await;
    stream.say.write_all(b"hello?\n").await.unwrap();
    tokio::task::yield_now().await;
    log.push(completed("after-chatter"));
    assert_eq!(stream.event().await, (1, "after-chatter".to_string()));
}

/// A write that fails, because the subscriber's read side is gone, ends the
/// stream.
#[tokio::test]
async fn a_failed_write_ends_the_stream() {
    let log = log_of(1);
    // The same halves `open` serves, so this is the same instantiation of
    // the stream and its arms count together.
    let (client, server) = tokio::io::duplex(64);
    let (server_read, mut server_write) = tokio::io::split(server);
    drop(client);
    let mut read = BufReader::new(server_read).lines();
    let from = EventCursor {
        session: "elsewhere".to_string(),
        after: 0,
    };
    stream_events(&mut read, &mut server_write, log, Some(from))
        .await
        .unwrap();
}

/// Every log is its own session, so numbers from one daemon are never read
/// as another's.
#[test]
fn every_log_is_its_own_session() {
    let (a, b) = (EventLog::default(), EventLog::new());
    assert_eq!(a.session().len(), 32);
    assert_ne!(a.session(), b.session());
    assert_eq!(a.subscribers(), 0);
}

/// A daemon on `id` serving one connection with `log`, the way the gate
/// serves every connection with its one log.
fn daemon_with(
    id: &crate::control_socket::ControlId,
    log: &EventLog,
) -> tokio::task::JoinHandle<()> {
    let mut listener = crate::control_socket::bind_control_listener(id).unwrap();
    let log = log.clone();
    tokio::spawn(async move {
        let stream = listener.accept().await.unwrap().unwrap();
        let (op_tx, _op_rx) = tokio::sync::mpsc::unbounded_channel();
        let identity = crate::control_socket::DaemonIdentity::this_process(
            crate::control_socket::DaemonIdentity::unknown_build(),
        );
        let _ =
            crate::control_socket::handle_connection_as(stream, op_tx, log, None, identity).await;
    })
}

/// The client end to end: a stream says where it got to, and a client that
/// picks it up after the daemon was replaced is sent what the new daemon
/// sent before it was back - the completion of a run that the new daemon
/// restored and finished at once.
#[tokio::test]
async fn a_client_picks_up_after_a_replaced_daemon_with_what_it_missed() {
    let dir = tempfile::tempdir().unwrap();
    let id = crate::control_socket::control_id(dir.path());
    let client = crate::control_socket::ControlClient::new(id.clone());

    let (tx, _keep) = broadcast::channel(16);
    let first = EventLog::recording(&tx);
    let served = daemon_with(&id, &first);
    let mut stream = client.subscribe().await.unwrap();
    leviath_testkit::wait_until("the stream is open", || first.subscribers() > 0).await;
    tx.send(completed("seen")).unwrap();
    assert_eq!(stream.next().await, Some(completed("seen")));
    let cursor = stream.cursor().expect("the daemon said where it starts");
    assert_eq!(cursor.session, first.session());
    assert_eq!(cursor.after, 1);
    drop(tx);
    assert!(stream.next().await.is_none(), "the daemon went away");
    served.await.unwrap();

    // The replacement finishes a run before the client is back.
    let second = log_of(1);
    let served = daemon_with(&id, &second);
    let mut stream = client.subscribe_from(Some(&cursor)).await.unwrap();
    assert_eq!(stream.next().await, Some(completed("run-1")));
    assert_eq!(
        stream.cursor(),
        Some(EventCursor {
            session: second.session().to_string(),
            after: 1,
        })
    );
    drop(stream);
    served.await.unwrap();
}
