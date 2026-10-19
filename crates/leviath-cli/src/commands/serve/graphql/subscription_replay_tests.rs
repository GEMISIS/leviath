//! Tests for resuming a stream with `since`, for the frame numbers a spawn
//! answers with, and for runs whose spawn frame outruns the run index.

use async_graphql::{Request, Response, futures_util::Stream, futures_util::StreamExt};

use super::tests::{log, spawned, status, test_state, write_run};
use crate::commands::serve::events::{EventBus, ServerEvent, Stamped, send, server_instance};
use crate::commands::serve::types::AppState;
use crate::runstate::{RunMeta, RunStatus, create_run, with_isolated_runs_dir_async};

/// The next frame a subscription yields, as JSON, under its root field.
async fn next(
    stream: &mut (impl Stream<Item = Response> + Unpin),
    root: &str,
) -> serde_json::Value {
    let response = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
        .await
        .expect("the frame arrives")
        .expect("the stream is open");
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    serde_json::to_value(&response.data).expect("serializes")[root].clone()
}

/// The numbers the bus gave every frame waiting on `raw`.
fn numbers(raw: &mut tokio::sync::broadcast::Receiver<Stamped>) -> Vec<u64> {
    std::iter::from_fn(|| raw.try_recv().ok())
        .map(|frame| frame.seq)
        .collect()
}

/// A state whose bus keeps `kept` frames and holds `live` for each receiver.
fn state_keeping(dir: &std::path::Path, live: usize, kept: usize) -> AppState {
    let mut state = test_state(dir);
    state.event_tx = EventBus::new(tokio::sync::broadcast::channel(live).0, kept);
    state
}

/// What a run subscription that reads log lines selects.
const LINES: &str = "__typename
    ... on SubscriptionOpenedEvent { seq serverInstance }
    ... on EventsDroppedEvent { count seq }
    ... on LogLineWrittenEvent { seq line }
    ... on RunSpawnedEvent { seq runId }";

/// A client that comes back with the number it last saw is handed what it
/// missed, in order, and then the live frames.
#[tokio::test]
async fn a_resumed_subscription_replays_what_it_missed_then_goes_live() {
    let dir = tempfile::tempdir().expect("an agents dir");
    let state = test_state(dir.path());
    let tx = state.event_tx.clone();
    let mut raw = tx.subscribe();
    for line in ["one", "two", "three"] {
        send(&tx, log("run-a", line));
    }
    let seqs = numbers(&mut raw);
    let schema = crate::commands::serve::graphql::build_schema(state, false);
    let mut stream = schema.execute_stream(Request::new(format!(
        r#"subscription {{ runEvents(since: {}, serverInstance: "{}") {{ {LINES} }} }}"#,
        seqs[0],
        server_instance()
    )));

    let opened = next(&mut stream, "runEvents").await;
    assert_eq!(opened["__typename"], "SubscriptionOpenedEvent");
    assert_eq!(opened["seq"], seqs[0], "numbered from where the client was");
    for (line, seq) in [("two", seqs[1]), ("three", seqs[2])] {
        let frame = next(&mut stream, "runEvents").await;
        assert_eq!(frame["line"], line);
        assert_eq!(frame["seq"], seq);
    }
    send(&tx, log("run-a", "four"));
    assert_eq!(next(&mut stream, "runEvents").await["line"], "four");
}

/// Frames sent while a subscription is being opened and replayed reach it
/// exactly once: nothing falls between the replay and the live frames, and
/// nothing is in both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_seam_between_replay_and_live_has_no_gap_and_no_repeat() {
    let dir = tempfile::tempdir().expect("an agents dir");
    let state = state_keeping(dir.path(), 8192, 8192);
    let tx = state.event_tx.clone();
    let mut raw = tx.subscribe();
    let total = 400;
    let (halfway_tx, halfway) = tokio::sync::oneshot::channel();
    let producer = {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut halfway_tx = Some(halfway_tx);
            for i in 0..total {
                send(&tx, log("run-a", &i.to_string()));
                if i == 100
                    && let Some(signal) = halfway_tx.take()
                {
                    let _ = signal.send(());
                }
                tokio::task::yield_now().await;
            }
        })
    };
    halfway.await.expect("the producer gets halfway");
    let early = numbers(&mut raw);
    let since = early[50];
    let schema = crate::commands::serve::graphql::build_schema(state, false);
    let mut stream = schema.execute_stream(Request::new(format!(
        "subscription {{ runEvents(since: {since}) {{ {LINES} }} }}"
    )));
    assert_eq!(
        next(&mut stream, "runEvents").await["__typename"],
        "SubscriptionOpenedEvent"
    );
    let mut got = Vec::new();
    loop {
        let frame = next(&mut stream, "runEvents").await;
        assert_eq!(frame["__typename"], "LogLineWrittenEvent", "{frame}");
        got.push(frame["seq"].as_u64().expect("a number"));
        if frame["line"].as_str() == Some(&(total - 1).to_string()) {
            break;
        }
    }
    producer.await.expect("the producer finishes");
    let want: Vec<u64> = early
        .into_iter()
        .chain(numbers(&mut raw))
        .filter(|seq| *seq > since)
        .collect();
    assert_eq!(got, want, "every frame after `since`, once each, in order");
}

/// A client numbered by another process cannot resume: it is told so right
/// after the greeting, and handed nothing kept.
#[tokio::test]
async fn a_stream_numbered_by_another_server_opens_with_a_gap() {
    let dir = tempfile::tempdir().expect("an agents dir");
    let state = test_state(dir.path());
    let tx = state.event_tx.clone();
    send(&tx, log("run-a", "before"));
    let schema = crate::commands::serve::graphql::build_schema(state, false);
    let mut stream = schema.execute_stream(Request::new(format!(
        r#"subscription {{ runEvents(since: 1, serverInstance: "another") {{ {LINES} }} }}"#
    )));
    let opened = next(&mut stream, "runEvents").await;
    let gap = next(&mut stream, "runEvents").await;
    assert_eq!(gap["__typename"], "EventsDroppedEvent");
    assert_eq!(
        gap["count"],
        i64::MAX,
        "nothing here says how much was missed elsewhere"
    );
    assert_eq!(gap["seq"], opened["seq"]);
    send(&tx, log("run-a", "after"));
    assert_eq!(
        next(&mut stream, "runEvents").await["line"],
        "after",
        "nothing kept is replayed to a stranger"
    );
}

/// A `since` older than what the bus keeps opens with a gap the size of what
/// is gone, and then replays what is still kept.
#[tokio::test]
async fn a_since_older_than_the_ring_opens_with_a_gap_then_replays_what_is_kept() {
    let dir = tempfile::tempdir().expect("an agents dir");
    let state = state_keeping(dir.path(), 64, 2);
    let tx = state.event_tx.clone();
    let mut raw = tx.subscribe();
    for line in ["one", "two", "three", "four"] {
        send(&tx, log("run-a", line));
    }
    let seqs = numbers(&mut raw);
    let schema = crate::commands::serve::graphql::build_schema(state, false);
    let mut stream = schema.execute_stream(Request::new(format!(
        "subscription {{ runEvents(since: {}) {{ {LINES} }} }}",
        seqs[0]
    )));
    next(&mut stream, "runEvents").await;
    let gap = next(&mut stream, "runEvents").await;
    assert_eq!(gap["__typename"], "EventsDroppedEvent");
    assert_eq!(gap["count"], seqs[1] - seqs[0], "the frames that are gone");
    assert_eq!(gap["seq"], seqs[0]);
    assert_eq!(next(&mut stream, "runEvents").await["line"], "three");
    assert_eq!(next(&mut stream, "runEvents").await["line"], "four");
}

/// A bus that keeps nothing reports every resume as a gap.
#[test]
fn a_bus_that_keeps_nothing_counts_every_frame_as_gone() {
    let bus = EventBus::new(tokio::sync::broadcast::channel(4).0, 0);
    let mut raw = bus.subscribe();
    send(&bus, log("run-a", "one"));
    send(&bus, log("run-a", "two"));
    let seqs = numbers(&mut raw);
    let resume = bus.resume(seqs[0] - 1);
    assert!(resume.kept.is_empty());
    assert_eq!(resume.evicted, seqs[1] - seqs[0] + 1);
    assert!(resume.at >= seqs[1]);
}

/// The machine stream resumes the same way, gap first.
#[tokio::test]
async fn the_machine_stream_resumes_too() {
    let dir = tempfile::tempdir().expect("an agents dir");
    let state = state_keeping(dir.path(), 64, 1);
    let tx = state.event_tx.clone();
    let mut raw = tx.subscribe();
    for connected in [false, true] {
        send(
            &tx,
            ServerEvent::DaemonLink {
                connected,
                daemon: None,
                restarted: false,
                restart_advised: None,
            },
        );
    }
    let seqs = numbers(&mut raw);
    let schema = crate::commands::serve::graphql::build_schema(state, false);
    let mut stream = schema.execute_stream(Request::new(format!(
        r#"subscription {{ machineEvents(since: {}, serverInstance: "{}") {{ __typename
             ... on EventsDroppedEvent {{ count }}
             ... on DaemonLinkChangedEvent {{ seq connected }} }} }}"#,
        seqs[0] - 1,
        server_instance()
    )));
    next(&mut stream, "machineEvents").await;
    let gap = next(&mut stream, "machineEvents").await;
    assert_eq!(gap["__typename"], "EventsDroppedEvent");
    assert_eq!(gap["count"], 1);
    let kept = next(&mut stream, "machineEvents").await;
    assert_eq!(kept["seq"], seqs[1]);
    assert_eq!(kept["connected"], true);
}

/// `spawnRun` answers with the number its run's frames start above, and a
/// subscription resumed from it is handed the run's spawn frame, even when
/// that frame went out before the client subscribed.
#[tokio::test]
async fn a_spawn_says_where_its_frames_start() {
    with_isolated_runs_dir_async("graphql-spawn-seq", |_runs| async move {
        let dir = tempfile::tempdir().expect("an agents dir");
        let mut state = test_state(dir.path());
        let bus = state.event_tx.clone();
        let (control, _sock, _srv) = crate::commands::serve::testutil::fake_daemon(move |_| {
            // The daemon's spawn frame reaches the bus before the answer.
            send(&bus, spawned("seq-run", None));
            leviath_runtime::control_socket::ControlResponse::Spawned {
                run_id: "seq-run".to_string(),
                warnings: Default::default(),
            }
        });
        state.control = control;
        let schema = crate::commands::serve::graphql::build_schema(state, false);
        let graph = "title = \"raw\"\n\n[[stages]]\nname = \"only\"\n\n[layout]\n\
                     total_budget_tokens = 100\nregions = [{ name = \"plan\", kind = \"pinned\", budget = 100 }]\n";
        let graph: leviath_runtime::spec::graph::RunGraph =
            toml::from_str(graph).expect("the graph parses");
        let answer = schema
            .execute(
                Request::new(
                    "mutation($request: SpawnRunRequest!) { spawnRun(request: $request) {
                       ... on SpawnedOutput { runId seq serverInstance } } }",
                )
                .variables(async_graphql::Variables::from_json(serde_json::json!({
                    "request": { "source": { "graph": serde_json::to_value(&graph).expect("JSON") } }
                }))),
            )
            .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let answer = serde_json::to_value(&answer.data).expect("serializes");
        let spawned = &answer["spawnRun"];
        assert_eq!(spawned["serverInstance"], server_instance());
        // The daemon has written the record by the time the client asks.
        write_run("seq-run", None, "coder", RunStatus::Running);

        let mut stream = schema.execute_stream(Request::new(format!(
            r#"subscription {{ runEvents(filter: {{ id: {{ in: ["seq-run"] }} }},
                 since: {}, serverInstance: {}) {{ {LINES} }} }}"#,
            spawned["seq"], spawned["serverInstance"]
        )));
        next(&mut stream, "runEvents").await;
        let first = next(&mut stream, "runEvents").await;
        assert_eq!(first["__typename"], "RunSpawnedEvent");
        assert_eq!(first["runId"], "seq-run");
    })
    .await;
}

/// One run on disk carrying one metadata entry.
fn write_tagged_run(id: &str, window: &str) {
    let mut meta = RunMeta::new(
        id.to_string(),
        "coder".to_string(),
        "/agents/coder".to_string(),
        "a task".to_string(),
        None,
        "/work".to_string(),
        1,
    );
    meta.metadata
        .insert("window".to_string(), window.to_string());
    create_run(&meta).expect("the run is written");
}

/// A subscription filtered on metadata, the way a client follows its own runs.
fn window_subscription(window: &str, types: &str) -> String {
    format!(
        r#"subscription {{ runEvents(
             filter: {{ metadata: {{ some: {{ key: {{ eq: "window" }}, value: {{ eq: "{window}" }} }} }} }}
             {types}) {{ {LINES} }} }}"#
    )
}

/// A subscription to window `w1`, with `anchor` already in it.
///
/// The anchor is how a test knows the subscription has looked at a frame:
/// frames are handled in order, so once the anchor's line arrives, every
/// frame sent before it has been weighed. Without that, a record written
/// "after" a spawn frame would be on disk before the subscription read it.
async fn window_stream(
    types: &str,
) -> (
    EventBus,
    impl Stream<Item = Response> + Unpin,
    tempfile::TempDir,
) {
    write_tagged_run("anchor", "w1");
    let dir = tempfile::tempdir().expect("an agents dir");
    let state = state_keeping(dir.path(), 256, 16);
    let tx = state.event_tx.clone();
    let schema = crate::commands::serve::graphql::build_schema(state, false);
    let mut stream = schema.execute_stream(Request::new(window_subscription("w1", types)));
    next(&mut stream, "runEvents").await;
    (tx, stream, dir)
}

/// Wait until every frame sent so far has been weighed.
async fn settle(tx: &EventBus, stream: &mut (impl Stream<Item = Response> + Unpin)) {
    send(tx, log("anchor", "sync"));
    let frame = next(stream, "runEvents").await;
    assert_eq!(frame["line"], "sync", "nothing else came first: {frame}");
}

/// A run whose spawn frame outruns its record still joins a metadata filter,
/// with every frame it sent meanwhile, spawn first.
///
/// The index cannot answer for the run on its spawn frame and can on the
/// next. The filter names nothing a status frame can change, so only the
/// run being pending gets it asked again.
#[tokio::test]
async fn a_run_the_index_has_not_seen_yet_joins_a_metadata_filter() {
    with_isolated_runs_dir_async("sub-pending-joins", |_runs| async move {
        let (tx, mut stream, _dir) = window_stream("").await;

        // The first lookup finds no record; the second does.
        send(&tx, spawned("tagged", None));
        send(&tx, log("tagged", "held"));
        settle(&tx, &mut stream).await;
        write_tagged_run("tagged", "w1");
        send(&tx, log("tagged", "admitted"));
        send(&tx, log("tagged", "live"));

        let first = next(&mut stream, "runEvents").await;
        assert_eq!(first["__typename"], "RunSpawnedEvent");
        assert_eq!(first["runId"], "tagged");
        for line in ["held", "admitted", "live"] {
            assert_eq!(next(&mut stream, "runEvents").await["line"], line);
        }
    })
    .await;
}

/// A pending run the index then says does not match is dropped with its
/// frames, and a run it never answers for is given up on.
#[tokio::test]
async fn a_pending_run_that_does_not_match_or_never_appears_is_dropped() {
    with_isolated_runs_dir_async("sub-pending-drops", |_runs| async move {
        let (tx, mut stream, _dir) = window_stream("").await;

        send(&tx, spawned("other", None));
        send(&tx, spawned("ghost", None));
        for i in 0..=super::HOLD_LIMIT {
            send(&tx, log("ghost", &i.to_string()));
        }
        settle(&tx, &mut stream).await;
        // Given up on: its record turning up later changes nothing.
        write_tagged_run("ghost", "w1");
        write_tagged_run("other", "w2");
        send(&tx, log("ghost", "too late"));
        send(&tx, log("other", "not this window"));
        send(&tx, log("other", "still not"));
        settle(&tx, &mut stream).await;
    })
    .await;
}

/// A spawn frame the client filtered out by type still brings its run into
/// scope, so the frames it did ask for arrive.
#[tokio::test]
async fn a_spawn_frame_not_asked_for_still_widens_the_scope() {
    with_isolated_runs_dir_async("sub-pending-types", |_runs| async move {
        let (tx, mut stream, _dir) = window_stream("types: [LOG_LINE_WRITTEN]").await;

        send(&tx, spawned("typed", None));
        send(&tx, status("typed"));
        send(&tx, log("typed", "held"));
        settle(&tx, &mut stream).await;
        write_tagged_run("typed", "w1");
        send(&tx, status("typed"));
        send(&tx, log("typed", "after"));

        assert_eq!(next(&mut stream, "runEvents").await["line"], "held");
        assert_eq!(next(&mut stream, "runEvents").await["line"], "after");
    })
    .await;
}

/// Pending runs are capped: past the cap the oldest is given up on, so spawns
/// whose records never arrive cannot grow the map without bound.
#[tokio::test]
async fn the_oldest_pending_run_is_given_up_on_past_the_cap() {
    with_isolated_runs_dir_async("sub-pending-cap", |_runs| async move {
        let (tx, mut stream, _dir) = window_stream("").await;
        // Settled every 64 spawns, so the 256-frame bus never laps it.
        for i in 0..=super::PENDING_LIMIT {
            send(&tx, spawned(&format!("cap-{i}"), None));
            if i % 64 == 63 {
                settle(&tx, &mut stream).await;
            }
        }
        settle(&tx, &mut stream).await;
        // The first was pushed out by the last; the second is still pending.
        write_tagged_run("cap-0", "w1");
        write_tagged_run("cap-1", "w1");
        send(&tx, log("cap-0", "evicted"));
        send(&tx, log("cap-1", "kept"));
        let joined = next(&mut stream, "runEvents").await;
        assert_eq!(joined["__typename"], "RunSpawnedEvent");
        assert_eq!(joined["runId"], "cap-1");
        assert_eq!(next(&mut stream, "runEvents").await["line"], "kept");
    })
    .await;
}
