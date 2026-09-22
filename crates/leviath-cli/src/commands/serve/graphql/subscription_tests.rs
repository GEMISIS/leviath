//! Tests for the live streams: what opens one, what each keeps, what it drops,
//! and what it says when it falls behind.

use async_graphql::{Request, Value, futures_util::StreamExt};

use crate::commands::serve::core::runs::MAX_IDS;
use crate::commands::serve::events::{ServerEvent, Stamped};
use crate::commands::serve::types::AppState;
use crate::commands::serve::update_job;
use crate::runstate::{RunMeta, RunStatus, create_run, with_isolated_runs_dir_async};

/// A status frame for one run.
fn status(run_id: &str) -> ServerEvent {
    ServerEvent::AgentStatus {
        agent_id: format!("agent-{run_id}"),
        run_id: run_id.to_string(),
        status: "running".to_string(),
        stage: "build".to_string(),
        iteration: 1,
        tool_calls: 2,
        accepts_messages: true,
        wait_reason: None,
        title: None,
    }
}

/// A log frame for one run.
fn log(run_id: &str, line: &str) -> ServerEvent {
    ServerEvent::Log {
        agent_id: format!("agent-{run_id}"),
        run_id: run_id.to_string(),
        line: line.to_string(),
    }
}

/// A spawn frame, optionally naming the run that started it.
fn spawned(run_id: &str, parent: Option<&str>) -> ServerEvent {
    ServerEvent::AgentSpawned {
        agent_id: format!("agent-{run_id}"),
        run_id: run_id.to_string(),
        parent_id: parent.map(str::to_string),
        blueprint: "coder".to_string(),
    }
}

/// An `AppState` with a temp agents directory, so nothing here reads the
/// reader's own `~/.leviath/agents`.
fn test_state(dir: &std::path::Path) -> AppState {
    crate::commands::serve::testutil::state_with_agent_paths(vec![dir.to_path_buf()])
}

/// One run on disk, so the run index has something to answer a filter with.
fn write_run(id: &str, parent: Option<&str>, blueprint: &str, status: RunStatus) {
    let mut meta = RunMeta::new(
        id.to_string(),
        blueprint.to_string(),
        format!("/agents/{blueprint}"),
        "a task".to_string(),
        None,
        "/work".to_string(),
        1,
    );
    meta.parent_run_id = parent.map(str::to_string);
    meta.status = status;
    create_run(&meta).expect("the run is written");
}

/// Run one subscription, send the given frames, and collect what arrives.
///
/// The subscription is started before anything is sent, which is also the
/// server's own order: subscribe, then let frames flow. The opening frame is
/// included in what comes back, because it is the first thing every
/// subscription says.
async fn frames_on(
    state: AppState,
    query: &str,
    events: Vec<ServerEvent>,
    want: usize,
) -> Vec<serde_json::Value> {
    let tx = state.event_tx.clone();
    let schema = crate::commands::serve::graphql::build_schema(state, false);

    let mut stream = schema.execute_stream(Request::new(query));
    // The first poll is what registers the receiver on the broadcast, and it is
    // driven by the collector task below before anything is sent.
    let collector = tokio::spawn(async move {
        let mut out = Vec::new();
        while out.len() < want {
            match stream.next().await {
                Some(response) => {
                    assert!(response.errors.is_empty(), "{:?}", response.errors);
                    out.push(serde_json::to_value(&response.data).expect("data serializes"));
                }
                None => break,
            }
        }
        out
    });

    // A frame sent before the receiver exists is not delivered, which is what
    // this wait is for: the daemon's own broadcast behaves the same way.
    for _ in 0..500 {
        if tx.receiver_count() > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    for event in events {
        crate::commands::serve::events::send(&tx, event);
    }
    tokio::time::timeout(std::time::Duration::from_secs(10), collector)
        .await
        .expect("the frames arrive")
        .expect("the collector finishes")
}

/// The same, with a temp agents directory nobody in the test cares about.
async fn frames(query: &str, events: Vec<ServerEvent>, want: usize) -> Vec<serde_json::Value> {
    let dir = tempfile::tempdir().expect("an agents dir");
    frames_on(test_state(dir.path()), query, events, want).await
}

/// The field one frame carries, whichever root it came from.
fn field<'a>(frame: &'a serde_json::Value, root: &str, name: &str) -> &'a serde_json::Value {
    &frame[root][name]
}

/// Every subscription opens with the frame that says it is open.
///
/// The counterpart of the greeting `/ws` sends. A client that has it knows the
/// stream is live, knows which process is numbering the frames, and knows
/// whether the daemon behind it is reachable - without a second request.
#[tokio::test]
async fn every_subscription_opens_with_the_frame_that_says_so() {
    let out = frames(
        "subscription { runEvents { __typename
            ... on SubscriptionOpenedEvent { seq at serverInstance daemon { reachable version } }
            ... on LogLineWrittenEvent { runId line } } }",
        vec![log("run-a", "after the greeting")],
        2,
    )
    .await;
    assert_eq!(
        field(&out[0], "runEvents", "__typename"),
        "SubscriptionOpenedEvent"
    );
    assert!(field(&out[0], "runEvents", "at").as_i64().unwrap_or(0) > 0);
    let instance = field(&out[0], "runEvents", "serverInstance")
        .as_str()
        .expect("an instance id");
    assert_eq!(instance.len(), 16, "a stable-width instance id: {instance}");
    // No daemon is reachable in a test, and the frame says so rather than
    // being absent.
    assert_eq!(
        field(&out[0], "runEvents", "daemon")["reachable"],
        serde_json::Value::Bool(true)
    );
    assert_eq!(
        field(&out[1], "runEvents", "__typename"),
        "LogLineWrittenEvent"
    );

    // The machine and per-job streams open the same way.
    let machine = frames(
        "subscription { machineEvents { __typename } }",
        Vec::new(),
        1,
    )
    .await;
    assert_eq!(
        field(&machine[0], "machineEvents", "__typename"),
        "SubscriptionOpenedEvent"
    );
    let job = frames(
        r#"subscription { updateJobEvents(id: "job-1") { __typename } }"#,
        Vec::new(),
        1,
    )
    .await;
    assert_eq!(
        field(&job[0], "updateJobEvents", "__typename"),
        "SubscriptionOpenedEvent"
    );
}

/// The stamp rises on every frame, and the time is set.
///
/// Without it a client cannot tell "nothing happened" from "I was not handed
/// everything", which is the whole reason the frames are numbered.
#[tokio::test]
async fn the_stamp_rises_and_is_dated() {
    let out = frames(
        "subscription { runEvents { ... on Event { seq at } } }",
        vec![log("run-a", "1"), log("run-a", "2"), log("run-a", "3")],
        4,
    )
    .await;
    // One fragment, four frames, the greeting included: the transport frames
    // implement `Event` too, so a client reads the stamp off whatever arrives
    // rather than naming each member type to get at it.
    let seqs: Vec<i64> = out
        .iter()
        .map(|frame| {
            let at = field(frame, "runEvents", "at").as_i64().expect("a time");
            assert!(at > 1_700_000_000, "the frame is dated: {at}");
            field(frame, "runEvents", "seq").as_i64().expect("a number")
        })
        .collect();
    assert_eq!(seqs.len(), 4);
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "the greeting is numbered under the frames that follow it: {seqs:?}"
    );
}

/// Every frame arrives when nothing is filtered, and each one arrives as its
/// own type in the union.
#[tokio::test]
async fn an_unscoped_subscription_sees_every_run_frame() {
    let out = frames(
        "subscription { runEvents { __typename
            ... on RunStatusChangedEvent { runId agentId status iteration toolCalls acceptsMessages }
            ... on LogLineWrittenEvent { runId line } } }",
        vec![status("run-a"), log("run-b", "hello")],
        3,
    )
    .await;
    assert_eq!(
        field(&out[1], "runEvents", "__typename"),
        "RunStatusChangedEvent"
    );
    assert_eq!(field(&out[1], "runEvents", "runId"), "run-a");
    assert_eq!(field(&out[1], "runEvents", "agentId"), "agent-run-a");
    assert_eq!(field(&out[1], "runEvents", "status"), "RUNNING");
    assert_eq!(field(&out[1], "runEvents", "toolCalls"), 2);
    assert_eq!(
        field(&out[2], "runEvents", "__typename"),
        "LogLineWrittenEvent"
    );
    assert_eq!(field(&out[2], "runEvents", "line"), "hello");
}

/// A type filter drops what it did not ask for, before the frame is converted.
#[tokio::test]
async fn a_type_filter_keeps_only_what_it_asked_for() {
    let out = frames(
        "subscription { runEvents(types: [LOG_LINE_WRITTEN]) { __typename
             ... on LogLineWrittenEvent { line } } }",
        vec![status("run-a"), log("run-a", "only this"), status("run-b")],
        2,
    )
    .await;
    assert_eq!(out.len(), 2);
    assert_eq!(field(&out[1], "runEvents", "line"), "only this");
}

/// The link frame reaches a subscription whatever it asked for, because it is
/// what explains a silence.
#[tokio::test]
async fn the_link_frame_is_never_filtered_out() {
    let out = frames(
        "subscription { runEvents(types: [LOG_LINE_WRITTEN]) { __typename
             ... on DaemonLinkChangedEvent { connected restarted } } }",
        vec![
            status("run-a"),
            ServerEvent::DaemonLink {
                connected: false,
                daemon: None,
                restarted: false,
                restart_advised: None,
            },
        ],
        2,
    )
    .await;
    assert_eq!(
        field(&out[1], "runEvents", "__typename"),
        "DaemonLinkChangedEvent"
    );
    assert_eq!(field(&out[1], "runEvents", "connected"), false);
}

/// The machine's own frames are not a run subscription's business.
#[tokio::test]
async fn a_run_subscription_does_not_see_the_installs_frames() {
    let out = frames(
        "subscription { runEvents { __typename ... on LogLineWrittenEvent { line } } }",
        vec![
            ServerEvent::ConfigHealth {
                healthy: false,
                path: "/config.toml".to_string(),
                error: None,
                config_mtime: None,
            },
            ServerEvent::UpdateProgress {
                job_id: "job-1".to_string(),
                step: update_job::Step::Binary,
                status: update_job::StepStatus::Running,
                detail: String::new(),
            },
            log("run-a", "this one"),
        ],
        2,
    )
    .await;
    assert_eq!(out.len(), 2);
    assert_eq!(field(&out[1], "runEvents", "line"), "this one");
}

/// A run filter is resolved to a set of runs when the subscription starts.
#[tokio::test]
async fn a_run_filter_is_resolved_when_the_subscription_starts() {
    with_isolated_runs_dir_async("sub-filter-start", |_runs| async move {
        write_run("run-a", None, "coder", RunStatus::Running);
        write_run("run-b", None, "reviewer", RunStatus::Running);
        let dir = tempfile::tempdir().expect("an agents dir");
        let out = frames_on(
            test_state(dir.path()),
            r#"subscription { runEvents(filter: { blueprintName: { eq: "coder" } }) {
                 __typename ... on LogLineWrittenEvent { runId line } } }"#,
            vec![
                log("run-b", "not this"),
                log("run-a", "this one"),
                log("run-c", "nor this"),
            ],
            2,
        )
        .await;
        assert_eq!(out.len(), 2);
        assert_eq!(field(&out[1], "runEvents", "runId"), "run-a");
        assert_eq!(field(&out[1], "runEvents", "line"), "this one");
    })
    .await;
}

/// The greeting's number sits below every frame the subscription can carry,
/// even when the filter took a while to resolve.
///
/// The receiver is registered before the filter is walked, so a frame sent
/// during the walk is buffered and delivered. A greeting numbered after that
/// walk would claim a number at or above one of those frames, and a client
/// following the field's own promise - "every frame on this subscription has a
/// number above it" - would throw them away.
#[tokio::test]
async fn the_greeting_is_numbered_below_every_frame_the_stream_can_carry() {
    with_isolated_runs_dir_async("sub-greeting-seq", |_runs| async move {
        write_run("run-a", None, "coder", RunStatus::Running);
        let dir = tempfile::tempdir().expect("an agents dir");
        let state = test_state(dir.path());
        let tx = state.event_tx.clone();
        let schema = crate::commands::serve::graphql::build_schema(state, false);
        let mut stream = schema.execute_stream(Request::new(
            r#"subscription { runEvents(filter: { blueprintName: { eq: "coder" } }) {
                 __typename
                 ... on SubscriptionOpenedEvent { seq }
                 ... on LogLineWrittenEvent { seq } } }"#,
        ));
        let collector = tokio::spawn(async move {
            let mut out = Vec::new();
            while out.len() < 2 {
                match stream.next().await {
                    Some(response) => {
                        assert!(response.errors.is_empty(), "{:?}", response.errors);
                        out.push(serde_json::to_value(&response.data).expect("serializes"));
                    }
                    None => break,
                }
            }
            out
        });
        // The receiver exists from the moment the resolver starts, and the
        // filter's walk awaits after that, so this frame goes out while the
        // subscription is still working out which runs it is about.
        for _ in 0..500 {
            if tx.receiver_count() > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        crate::commands::serve::events::send(&tx, log("run-a", "sent during the walk"));

        let out = tokio::time::timeout(std::time::Duration::from_secs(10), collector)
            .await
            .expect("the frames arrive")
            .expect("the collector finishes");
        assert_eq!(
            field(&out[0], "runEvents", "__typename"),
            "SubscriptionOpenedEvent"
        );
        let greeting = field(&out[0], "runEvents", "seq")
            .as_i64()
            .expect("the greeting's number");
        let first = field(&out[1], "runEvents", "seq")
            .as_i64()
            .expect("the frame's number");
        assert!(
            greeting < first,
            "the greeting is numbered {greeting} and the first frame {first}"
        );
    })
    .await;
}

/// A run that starts matching joins the scope on its next status change.
///
/// The one thing a subscribe-time set cannot do on its own: a run that was not
/// interesting when the client subscribed, and became interesting since.
#[tokio::test]
async fn a_run_that_starts_matching_joins_on_a_status_change() {
    with_isolated_runs_dir_async("sub-filter-joins", |_runs| async move {
        write_run("run-a", None, "coder", RunStatus::Running);
        let dir = tempfile::tempdir().expect("an agents dir");
        let state = test_state(dir.path());
        let tx = state.event_tx.clone();
        let schema = crate::commands::serve::graphql::build_schema(state, false);
        let mut stream = schema.execute_stream(Request::new(
            r#"subscription { runEvents(filter: { status: { eq: ERROR } }) {
                 __typename ... on RunEvent { runId } } }"#,
        ));
        let collector = tokio::spawn(async move {
            let mut out = Vec::new();
            while out.len() < 2 {
                match stream.next().await {
                    Some(response) => {
                        assert!(response.errors.is_empty(), "{:?}", response.errors);
                        out.push(serde_json::to_value(&response.data).expect("serializes"));
                    }
                    None => break,
                }
            }
            out
        });
        for _ in 0..500 {
            if tx.receiver_count() > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        // Nothing matched at subscribe time, so this is dropped.
        crate::commands::serve::events::send(&tx, log("run-a", "still running"));
        // The run fails; the status frame is what makes the filter look again.
        write_run("run-a", None, "coder", RunStatus::Error);
        crate::commands::serve::events::send(&tx, status("run-a"));
        crate::commands::serve::events::send(&tx, log("run-a", "after the failure"));

        let out = tokio::time::timeout(std::time::Duration::from_secs(10), collector)
            .await
            .expect("the frames arrive")
            .expect("the collector finishes");
        assert_eq!(out.len(), 2);
        assert_eq!(
            field(&out[1], "runEvents", "__typename"),
            "RunStatusChangedEvent"
        );
        assert_eq!(field(&out[1], "runEvents", "runId"), "run-a");
    })
    .await;
}

/// A filter nothing about a run's status can flip is not asked again on every
/// status frame.
///
/// The re-check reads the whole run index and links it into a tree, so running
/// it per frame per subscriber is the store over again for every heartbeat a
/// fleet sends. A filter that names ids says nothing a status change can alter,
/// so there is nothing to look at.
#[tokio::test]
async fn a_filter_no_status_change_can_flip_is_not_re_read_per_frame() {
    with_isolated_runs_dir_async("sub-status-recheck", |_runs| async move {
        write_run("sub6-a", None, "coder", RunStatus::Running);
        write_run("sub6-b", None, "coder", RunStatus::Running);
        let dir = tempfile::tempdir().expect("an agents dir");
        let before = crate::commands::serve::testutil::trees_built_over("sub6-");
        let out = frames_on(
            test_state(dir.path()),
            r#"subscription { runEvents(filter: { id: { in: ["sub6-a"] } })
                 { ... on RunEvent { runId } } }"#,
            vec![
                status("sub6-b"),
                status("sub6-b"),
                status("sub6-b"),
                log("sub6-a", "mine"),
            ],
            2,
        )
        .await;
        let built = crate::commands::serve::testutil::trees_built_over("sub6-") - before;
        assert_eq!(
            built, 1,
            "the scope is resolved once, and not looked up again per frame"
        );
        assert_eq!(field(&out[1], "runEvents", "runId"), "sub6-a");
    })
    .await;
}

/// A filter a status change can flip is still asked again, which is what puts
/// a run that starts matching into scope.
#[tokio::test]
async fn a_filter_a_status_change_can_flip_is_still_re_read() {
    with_isolated_runs_dir_async("sub-status-recheck-on", |_runs| async move {
        write_run("sub6c-a", None, "coder", RunStatus::Error);
        write_run("sub6c-b", None, "coder", RunStatus::Running);
        let dir = tempfile::tempdir().expect("an agents dir");
        let before = crate::commands::serve::testutil::trees_built_over("sub6c-");
        let out = frames_on(
            test_state(dir.path()),
            r#"subscription { runEvents(filter: { status: { eq: ERROR } })
                 { ... on RunEvent { runId } } }"#,
            // The second run is outside the scope, so its status frame is the
            // moment the filter is asked whether it has joined.
            vec![status("sub6c-b"), log("sub6c-a", "mine")],
            2,
        )
        .await;
        let built = crate::commands::serve::testutil::trees_built_over("sub6c-") - before;
        assert!(built > 1, "the filter was asked again: {built} readings");
        assert_eq!(field(&out[1], "runEvents", "runId"), "sub6c-a");
    })
    .await;
}

/// A sub-agent spawned after the subscription started is included when the
/// client asked for descendants.
///
/// This is the difference the option buys: without it a fan-out means
/// re-querying the tree and re-subscribing as it grows, and frames in between
/// are lost.
#[tokio::test]
async fn descendants_spawned_later_are_included() {
    with_isolated_runs_dir_async("sub-descendants", |_runs| async move {
        write_run("root", None, "coder", RunStatus::Running);
        let dir = tempfile::tempdir().expect("an agents dir");
        let out = frames_on(
            test_state(dir.path()),
            r#"subscription { runEvents(filter: { id: { in: ["root"] } }, includeDescendants: true) {
                 __typename
                 ... on RunSpawnedEvent { runId parentId }
                 ... on LogLineWrittenEvent { runId line } } }"#,
            vec![
                spawned("worker-1", Some("root")),
                log("worker-1", "from the worker"),
                log("stranger", "from nobody's child"),
            ],
            3,
        )
        .await;
        assert_eq!(field(&out[1], "runEvents", "__typename"), "RunSpawnedEvent");
        assert_eq!(field(&out[1], "runEvents", "runId"), "worker-1");
        assert_eq!(field(&out[1], "runEvents", "parentId"), "root");
        // The worker's own frames now arrive, and an unrelated run's still do
        // not.
        assert_eq!(field(&out[2], "runEvents", "runId"), "worker-1");
        assert_eq!(field(&out[2], "runEvents", "line"), "from the worker");
    })
    .await;
}

/// Without the option, a sub-agent is in scope only if the filter itself picks
/// it up.
#[tokio::test]
async fn a_sub_agent_is_out_of_scope_unless_the_filter_takes_it() {
    with_isolated_runs_dir_async("sub-no-descendants", |_runs| async move {
        write_run("root", None, "coder", RunStatus::Running);
        let dir = tempfile::tempdir().expect("an agents dir");
        let out = frames_on(
            test_state(dir.path()),
            r#"subscription { runEvents(filter: { id: { in: ["root"] } }) {
                 ... on RunEvent { runId } } }"#,
            vec![
                spawned("worker-1", Some("root")),
                log("worker-1", "ignored"),
                log("root", "kept"),
            ],
            2,
        )
        .await;
        assert_eq!(out.len(), 2);
        assert_eq!(field(&out[1], "runEvents", "runId"), "root");
    })
    .await;
}

/// A spawn whose parent is not in scope does not widen it.
#[tokio::test]
async fn a_spawn_from_another_tree_does_not_widen_the_scope() {
    with_isolated_runs_dir_async("sub-other-tree", |_runs| async move {
        write_run("root", None, "coder", RunStatus::Running);
        let dir = tempfile::tempdir().expect("an agents dir");
        let out = frames_on(
            test_state(dir.path()),
            r#"subscription { runEvents(filter: { id: { in: ["root"] } }, includeDescendants: true) {
                 ... on RunEvent { runId } } }"#,
            vec![
                spawned("worker-1", Some("elsewhere")),
                log("worker-1", "not ours"),
                log("root", "ours"),
            ],
            2,
        )
        .await;
        assert_eq!(out.len(), 2);
        assert_eq!(field(&out[1], "runEvents", "runId"), "root");
    })
    .await;
}

/// An empty filter object names no condition, so it is every run - the same
/// answer as no filter at all.
#[tokio::test]
async fn an_empty_filter_is_every_run() {
    let out = frames(
        "subscription { runEvents(filter: {}) { ... on RunEvent { runId } } }",
        vec![log("run-a", "1"), log("run-b", "2")],
        3,
    )
    .await;
    assert_eq!(field(&out[1], "runEvents", "runId"), "run-a");
    assert_eq!(field(&out[2], "runEvents", "runId"), "run-b");
}

/// A filter the server refuses fails the subscription outright, rather than
/// opening a stream that will never carry anything.
#[tokio::test]
async fn a_refused_filter_fails_the_subscription() {
    let dir = tempfile::tempdir().expect("an agents dir");
    let state = test_state(dir.path());
    let schema = crate::commands::serve::graphql::build_schema(state, false);
    // A filter naming more runs than a listing will read at once. The
    // subscription resolves its filter exactly as a page does, so what one
    // refuses the other refuses.
    let named: Vec<String> = (0..MAX_IDS + 1).map(|at| format!("\"run-{at}\"")).collect();
    let mut stream = schema.execute_stream(Request::new(format!(
        "subscription {{ runEvents(filter: {{ id: {{ in: [{}] }} }}) {{ __typename }} }}",
        named.join(",")
    )));
    let first = stream.next().await.expect("a response");
    let message = first
        .errors
        .first()
        .map(|error| error.message.clone())
        .unwrap_or_default();
    assert!(
        message.contains("at most 200 may be named at once"),
        "{message}"
    );
}

/// The machine stream carries the frames that are about no run, and nothing
/// else.
#[tokio::test]
async fn the_machine_stream_carries_the_machines_own_frames() {
    let out = frames(
        "subscription { machineEvents { __typename
             ... on ConfigHealthChangedEvent { healthy path error { kind message } }
             ... on UpdateStepChangedEvent { jobId step status detail } } }",
        vec![
            log("run-a", "not the machine's business"),
            ServerEvent::ConfigHealth {
                healthy: false,
                path: "/config.toml".to_string(),
                error: Some(crate::commands::serve::config_types::ConfigErrorInfo {
                    kind: "parse".to_string(),
                    path: "/config.toml".to_string(),
                    message: "expected a table".to_string(),
                    line: None,
                    column: None,
                    key: None,
                    since: 1_788_000_000,
                    note: "it did not parse".to_string(),
                }),
                config_mtime: None,
            },
            ServerEvent::UpdateProgress {
                job_id: "job-1".to_string(),
                step: update_job::Step::Binary,
                status: update_job::StepStatus::Running,
                detail: "downloading".to_string(),
            },
        ],
        3,
    )
    .await;
    assert_eq!(
        field(&out[1], "machineEvents", "__typename"),
        "ConfigHealthChangedEvent"
    );
    assert_eq!(field(&out[1], "machineEvents", "path"), "/config.toml");
    assert_eq!(
        field(&out[1], "machineEvents", "error")["message"],
        "expected a table"
    );
    assert_eq!(
        field(&out[2], "machineEvents", "__typename"),
        "UpdateStepChangedEvent"
    );
    assert_eq!(field(&out[2], "machineEvents", "step"), "BINARY");
    assert_eq!(field(&out[2], "machineEvents", "status"), "RUNNING");
}

/// The machine stream takes a type filter of its own.
#[tokio::test]
async fn the_machine_stream_filters_by_type() {
    let out = frames(
        "subscription { machineEvents(types: [DAEMON_LINK_CHANGED]) { __typename } }",
        vec![
            ServerEvent::ConfigHealth {
                healthy: true,
                path: "/config.toml".to_string(),
                error: None,
                config_mtime: None,
            },
            ServerEvent::DaemonLink {
                connected: true,
                daemon: None,
                restarted: false,
                restart_advised: None,
            },
        ],
        2,
    )
    .await;
    assert_eq!(out.len(), 2);
    assert_eq!(
        field(&out[1], "machineEvents", "__typename"),
        "DaemonLinkChangedEvent"
    );
}

/// A subscription on one job sees that job and no other.
#[tokio::test]
async fn one_jobs_subscription_narrows_to_that_job() {
    let job = crate::commands::serve::update_job::UpdateJob {
        id: "job-1".to_string(),
        status: update_job::JobStatus::Complete,
        steps: Vec::new(),
        restart_required: false,
        restart_hint: None,
        started_at: 1,
        finished_at: Some(2),
    };
    let out = frames(
        r#"subscription { updateJobEvents(id: "job-1") { __typename
             ... on UpdateStepChangedEvent { jobId detail }
             ... on UpdateFinishedEvent { jobId status job { id status } } } }"#,
        vec![
            ServerEvent::UpdateProgress {
                job_id: "job-2".to_string(),
                step: update_job::Step::Binary,
                status: update_job::StepStatus::Running,
                detail: "another job".to_string(),
            },
            ServerEvent::UpdateProgress {
                job_id: "job-1".to_string(),
                step: update_job::Step::Binary,
                status: update_job::StepStatus::Running,
                detail: "this job".to_string(),
            },
            ServerEvent::UpdateFinished {
                job_id: "job-1".to_string(),
                status: update_job::JobStatus::Complete,
                restart_required: false,
                job,
            },
        ],
        3,
    )
    .await;
    assert_eq!(
        field(&out[1], "updateJobEvents", "__typename"),
        "UpdateStepChangedEvent"
    );
    assert_eq!(field(&out[1], "updateJobEvents", "detail"), "this job");
    assert_eq!(
        field(&out[2], "updateJobEvents", "__typename"),
        "UpdateFinishedEvent"
    );
    assert_eq!(field(&out[2], "updateJobEvents", "status"), "COMPLETE");
    assert_eq!(field(&out[2], "updateJobEvents", "job")["id"], "job-1");
}

/// A run frame can be read back to the run itself, and a frame about a run
/// that is not there answers null rather than failing.
#[tokio::test]
async fn a_frame_reads_back_to_its_run() {
    with_isolated_runs_dir_async("sub-run-of", |_runs| async move {
        write_run("run-a", None, "coder", RunStatus::Running);
        let dir = tempfile::tempdir().expect("an agents dir");
        let out = frames_on(
            test_state(dir.path()),
            "subscription { runEvents { ... on RunEvent { runId run { id blueprintName } } } }",
            vec![log("run-a", "1"), log("gone", "2")],
            3,
        )
        .await;
        assert_eq!(field(&out[1], "runEvents", "run")["id"], "run-a");
        assert_eq!(field(&out[1], "runEvents", "run")["blueprintName"], "coder");
        assert!(field(&out[2], "runEvents", "run").is_null());
    })
    .await;
}

/// A subscription that falls behind is told how many frames it missed, and
/// stays open.
///
/// Silently skipped, before this: a client could not tell a quiet run from a
/// missed one, and so could not know to re-read what it renders.
#[tokio::test]
async fn a_subscription_that_falls_behind_is_told_and_stays_open() {
    let dir = tempfile::tempdir().expect("an agents dir");
    let state = test_state(dir.path());
    let tx = state.event_tx.clone();
    let schema = crate::commands::serve::graphql::build_schema(state, false);
    let mut stream = schema.execute_stream(Request::new(
        "subscription { runEvents { __typename
             ... on EventsDroppedEvent { count seq at }
             ... on LogLineWrittenEvent { line } } }",
    ));

    // Registered first, then overrun: the channel a test server uses holds 64,
    // so filling it past that is what a slow client does to itself.
    let first = tokio::spawn(async move {
        let mut seen = Vec::new();
        while seen.len() < 4 {
            match stream.next().await {
                Some(response) => seen.push(response),
                None => break,
            }
        }
        seen
    });
    for _ in 0..500 {
        if tx.receiver_count() > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    for i in 0..600 {
        crate::commands::serve::events::send(&tx, log("run-a", &format!("line {i}")));
    }

    let seen = tokio::time::timeout(std::time::Duration::from_secs(10), first)
        .await
        .expect("frames arrive")
        .expect("the collector finishes");
    let frames: Vec<&Value> = seen
        .iter()
        .filter_map(|response| match &response.data {
            Value::Object(map) => map.get("runEvents"),
            _ => None,
        })
        .collect();
    let named = |frame: &Value, name: &str| match frame {
        Value::Object(map) => map.get(name).cloned(),
        _ => None,
    };
    let at = frames
        .iter()
        .position(|frame| {
            named(frame, "__typename") == Some(Value::String("EventsDroppedEvent".into()))
        })
        .expect("a drop was reported");
    let missed = match named(frames[at], "count") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or_default(),
        other => panic!("count is a number: {other:?}"),
    };
    assert!(missed > 0, "it says how many were missed: {missed}");
    assert!(
        matches!(named(frames[at], "seq"), Some(Value::Number(_))),
        "the gap names the last frame that did arrive"
    );
    // The stream is still carrying frames after the gap, which is the whole
    // point of reporting one rather than ending.
    assert!(
        frames[at + 1..]
            .iter()
            .any(|frame| named(frame, "__typename")
                == Some(Value::String("LogLineWrittenEvent".into()))),
        "the subscription stays open"
    );
}

/// The subscription endpoint over a real socket: handshake, subscribe, frame.
///
/// The resolvers are exercised above without HTTP. What this covers is the
/// endpoint: the `graphql-transport-ws` handshake, and a frame arriving over
/// the wire after a broadcast.
#[tokio::test]
async fn a_subscription_runs_over_a_real_websocket() {
    use axum::Router;
    use axum::routing::get;

    let dir = tempfile::tempdir().expect("an agents dir");
    let state = test_state(dir.path());
    let tx = state.event_tx.clone();
    let schema = crate::commands::serve::graphql::build_schema(state, false);
    let app = Router::new()
        .route("/ws/graphql", get(crate::commands::serve::graphql::ws))
        .layer(axum::extract::Extension(schema));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let addr = listener.local_addr().expect("the bound address");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let mut client = crate::commands::serve::testutil::WsTestClient::connect_with_protocol(
        addr,
        "/ws/graphql",
        Some("graphql-transport-ws"),
    )
    .await;

    client.send_text("{\"type\":\"connection_init\"}").await;
    let (opcode, payload) = client.recv_frame().await;
    assert_eq!(opcode, 0x1, "a text frame");
    let ack: serde_json::Value =
        serde_json::from_slice(&payload).expect("the acknowledgement is JSON");
    assert_eq!(ack["type"], "connection_ack");

    client
        .send_text(
            &serde_json::json!({
                "id": "1",
                "type": "subscribe",
                "payload": {
                    "query": "subscription { runEvents { __typename \
                              ... on LogLineWrittenEvent { runId line } } }"
                },
            })
            .to_string(),
        )
        .await;

    // The opening frame arrives without anything being sent, which is how a
    // client knows the subscription took.
    let (_, payload) = tokio::time::timeout(std::time::Duration::from_secs(5), client.recv_frame())
        .await
        .expect("the opening frame arrives");
    let greeting: serde_json::Value = serde_json::from_slice(&payload).expect("JSON");
    assert_eq!(greeting["type"], "next");
    assert_eq!(
        greeting["payload"]["data"]["runEvents"]["__typename"],
        "SubscriptionOpenedEvent"
    );

    crate::commands::serve::events::send(&tx, log("run-a", "over the wire"));

    let (opcode, payload) =
        tokio::time::timeout(std::time::Duration::from_secs(5), client.recv_frame())
            .await
            .expect("the frame arrives");
    assert_eq!(opcode, 0x1);
    let frame: serde_json::Value = serde_json::from_slice(&payload).expect("the frame is JSON");
    assert_eq!(frame["type"], "next");
    assert_eq!(frame["payload"]["data"]["runEvents"]["runId"], "run-a");
    assert_eq!(
        frame["payload"]["data"]["runEvents"]["line"],
        "over the wire"
    );

    client.send_close().await;
    server.abort();
}

/// The stamp a subscription opens on is the one the bus last handed out.
#[test]
fn the_opening_stamp_follows_the_bus() {
    use crate::commands::serve::events::latest_seq;

    let dir = tempfile::tempdir().expect("an agents dir");
    let state = test_state(dir.path());
    let before = super::super::events::opened(&state, latest_seq());
    crate::commands::serve::events::send(&state.event_tx, log("run-a", "one frame goes past"));
    let after = super::super::events::opened(&state, latest_seq());
    assert!(
        after.seq.0 > before.seq.0,
        "{} then {}",
        before.seq.0,
        after.seq.0
    );
    assert_eq!(
        after.server_instance.as_str(),
        before.server_instance.as_str(),
        "one process, one instance"
    );
}

/// One frame stamped by the bus carries a rising number and the second it went
/// out.
#[test]
fn the_bus_stamps_every_frame() {
    let (tx, mut rx) = tokio::sync::broadcast::channel::<Stamped>(8);
    crate::commands::serve::events::send(&tx, log("run-a", "1"));
    crate::commands::serve::events::send(&tx, log("run-a", "2"));
    let first = rx.try_recv().expect("the first frame");
    let second = rx.try_recv().expect("the second frame");
    assert!(second.seq > first.seq, "{} then {}", first.seq, second.seq);
    assert!(first.at > 1_700_000_000, "the frame is dated: {}", first.at);
    // Nobody listening is not an error: the daemon keeps working whether or
    // not a console is open.
    let (lonely, dropped) = tokio::sync::broadcast::channel::<Stamped>(8);
    drop(dropped);
    crate::commands::serve::events::send(&lonely, log("run-a", "3"));
}

/// A stream that falls behind is told how many frames it missed and stays
/// open, on every stream rather than only on the run one.
///
/// Three streams, three unfold loops, three places a gap could be dropped on
/// the floor. `query` selects the frame, `flood` fills the bus past what it
/// holds.
async fn a_gap_is_reported_on(query: &str, flood: impl Fn(usize) -> ServerEvent) {
    let dir = tempfile::tempdir().expect("an agents dir");
    let state = test_state(dir.path());
    let tx = state.event_tx.clone();
    let schema = crate::commands::serve::graphql::build_schema(state, false);
    let mut stream = schema.execute_stream(Request::new(query.to_string()));

    // Registered first, then overrun: the channel a test server uses holds 64,
    // so filling it past that is what a slow client does to itself.
    let collector = tokio::spawn(async move {
        let mut seen = Vec::new();
        while seen.len() < 3 {
            match stream.next().await {
                Some(response) => seen.push(response),
                None => break,
            }
        }
        seen
    });
    for _ in 0..500 {
        if tx.receiver_count() > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    for i in 0..600 {
        crate::commands::serve::events::send(&tx, flood(i));
    }

    let seen = tokio::time::timeout(std::time::Duration::from_secs(10), collector)
        .await
        .expect("frames arrive")
        .expect("the collector finishes");
    let names: Vec<String> = seen
        .iter()
        .filter_map(|response| match &response.data {
            Value::Object(map) => map.values().next().cloned(),
            _ => None,
        })
        .filter_map(|frame| match frame {
            Value::Object(map) => map.get("__typename").map(ToString::to_string),
            _ => None,
        })
        .collect();
    assert!(
        names.iter().any(|name| name.contains("EventsDroppedEvent")),
        "a gap is reported rather than silently skipped: {names:?}"
    );
}

/// The machine stream reports a gap of its own.
#[tokio::test]
async fn a_machine_subscription_that_falls_behind_is_told() {
    a_gap_is_reported_on(
        "subscription { machineEvents { __typename
             ... on EventsDroppedEvent { count seq } } }",
        |i| ServerEvent::UpdateProgress {
            job_id: "job-1".to_string(),
            step: update_job::Step::Binary,
            status: update_job::StepStatus::Running,
            detail: format!("step {i}"),
        },
    )
    .await;
}

/// And so does one job's stream.
#[tokio::test]
async fn a_job_subscription_that_falls_behind_is_told() {
    a_gap_is_reported_on(
        r#"subscription { updateJobEvents(id: "job-1") { __typename
             ... on EventsDroppedEvent { count seq } } }"#,
        |i| ServerEvent::UpdateProgress {
            job_id: "job-1".to_string(),
            step: update_job::Step::Binary,
            status: update_job::StepStatus::Running,
            detail: format!("step {i}"),
        },
    )
    .await;
}

/// A subscription scoped to some runs still gets the link frame, and none of
/// the machine's other frames.
///
/// A run's frames stopping because the daemon went away looks exactly like a
/// quiet run without it, so the link frame crosses the scope; the config's
/// health does not, because that belongs to `machineEvents`.
#[tokio::test]
async fn a_scoped_subscription_keeps_the_link_frame_and_nothing_else_machine_wide() {
    with_isolated_runs_dir_async("sub-scoped-link", |_runs| async move {
        write_run("run-a", None, "coder", RunStatus::Running);
        let dir = tempfile::tempdir().expect("an agents dir");
        let out = frames_on(
            test_state(dir.path()),
            r#"subscription { runEvents(filter: { id: { in: ["run-a"] } })
                 { __typename } }"#,
            vec![
                ServerEvent::ConfigHealth {
                    healthy: true,
                    path: "/config.toml".to_string(),
                    error: None,
                    config_mtime: None,
                },
                ServerEvent::DaemonLink {
                    connected: false,
                    daemon: None,
                    restarted: false,
                    restart_advised: None,
                },
            ],
            2,
        )
        .await;
        assert_eq!(out.len(), 2, "the opening frame, then the link one");
        assert_eq!(
            field(&out[1], "runEvents", "__typename"),
            "DaemonLinkChangedEvent",
            "the config's health stayed on the machine stream"
        );
    })
    .await;
}

/// A stream whose bus has gone ends rather than hanging.
///
/// The daemon's broadcast outlives every subscription in a server, so this is
/// the shutdown path and nothing else: the last sender drops, the receiver runs
/// dry, and each of the three streams finishes instead of waiting for a frame
/// nobody will send.
#[tokio::test]
async fn a_stream_whose_bus_has_gone_ends() {
    use std::collections::HashSet;
    use tokio_stream::wrappers::BroadcastStream;

    use super::{
        JobLive, MachineLive, RunLive, Scope, Types, job_frames, machine_frames, run_frames,
    };

    /// A receiver whose only sender is already gone.
    fn drained() -> BroadcastStream<Stamped> {
        let (tx, rx) = tokio::sync::broadcast::channel::<Stamped>(4);
        drop(tx);
        BroadcastStream::new(rx)
    }

    let dir = tempfile::tempdir().expect("an agents dir");
    let mut runs = Box::pin(run_frames(RunLive {
        frames: drained(),
        types: Types(HashSet::new()),
        scope: Scope::Everything,
        state: test_state(dir.path()),
        last_seq: 0,
    }));
    assert!(runs.next().await.is_none(), "the run stream ends");

    let mut machine = Box::pin(machine_frames(MachineLive {
        frames: drained(),
        types: Types(HashSet::new()),
        last_seq: 0,
    }));
    assert!(machine.next().await.is_none(), "the machine stream ends");

    let mut job = Box::pin(job_frames(JobLive {
        frames: drained(),
        job: "job-1".to_string(),
        last_seq: 0,
    }));
    assert!(job.next().await.is_none(), "the job stream ends");
}

/// A filter too deep to walk is refused when the subscription starts, rather
/// than once per frame.
#[tokio::test]
async fn a_subscription_filter_too_deep_to_walk_is_refused() {
    let mut written = "{ blueprintName: { eq: \"coder\" } }".to_string();
    for _ in 0..crate::commands::serve::graphql::paging::digest::MAX_FILTER_DEPTH {
        written = format!("{{ and: [{written}] }}");
    }
    let dir = tempfile::tempdir().expect("an agents dir");
    let schema = crate::commands::serve::graphql::build_schema(test_state(dir.path()), false);
    let mut stream = schema.execute_stream(Request::new(format!(
        "subscription {{ runEvents(filter: {written}) {{ __typename }} }}"
    )));
    let first = stream.next().await.expect("a response");
    assert!(
        first
            .errors
            .first()
            .is_some_and(|error| error.message.contains("levels deep")),
        "{:?}",
        first.errors
    );
}
