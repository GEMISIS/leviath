//! Tests for [`super`].
//!
//! A sibling file rather than an inline `mod tests`, and deliberately:
//! the helpers below poll a background lane, and whether a poll loop
//! iterates at all depends on how fast that lane happens to be. Inline,
//! the gate measures that scaffolding and fails intermittently on a
//! sleep that legitimately did not need to run. llvm-cov excludes this
//! layout by default, which is the sanctioned answer for a test module
//! whose own branches cannot be exercised deterministically
//! (see CONTRIBUTING, "Where a test module lives").

use super::*;

fn req(id: &str) -> InteractionRequest {
    InteractionRequest::free_text(id, "prompt?", "stage", true)
}

/// Let a just-spawned `submit` task reach its await point. `submit` inserts
/// into the registry synchronously before awaiting, so a few yields on the
/// current-thread test runtime are enough for it to have registered.
async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

#[test]
fn a_poisoned_registry_still_serves_every_other_agent() {
    // `pending` holds *every* agent's open prompt, so a panic while holding
    // it must not poison it: a poisoned registry makes
    // `pending()`/`answer()`/`cancel()` panic for all agents and the
    // dashboard.
    let hub = InteractionHub::new();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {})); // silence the deliberate panic
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = hub.pending.lock().expect("fresh lock");
        panic!("a panic while holding the interaction registry");
    }));
    std::panic::set_hook(prev);
    assert!(poisoned.is_err());
    assert!(hub.pending.is_poisoned(), "the lock really is poisoned");

    assert!(hub.pending().is_empty());
    assert!(!hub.cancel("nope"));
    assert!(!hub.answer(InteractionResponse::text("nope", "x")));
}

#[tokio::test]
async fn ask_is_answered_through_the_hub() {
    let hub = InteractionHub::new();
    let backend = hub.backend_for("agent-a");
    let asking = tokio::spawn(async move { backend.ask(req("q1")).await });

    settle().await;
    let pending = hub.pending();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, "agent-a");
    assert_eq!(pending[0].1.id, "q1");

    assert!(hub.answer(InteractionResponse::text("q1", "hello")));
    let response = asking.await.unwrap();
    assert_eq!(response.value.as_deref(), Some("hello"));
    // No longer pending.
    assert!(hub.pending().is_empty());
}

/// An unanswered prompt must not hold tool-lane capacity.
///
/// The answer can arrive from another agent's tool call, and on a lane with
/// no room left that call is queued behind the batch that is waiting for it.
/// That is the shape that freezes a whole daemon: everything looks `waiting`,
/// nothing is failed, and nothing moves again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_waiting_on_a_prompt_does_not_hold_the_tool_lane() {
    use crate::tool_bridge::{ToolJob, ToolLane, ToolLaneStats};
    use bevy_ecs::entity::Entity;

    let hub = InteractionHub::new();
    let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel();
    let (result_tx, mut results) = tokio::sync::mpsc::unbounded_channel();
    let stats = Arc::new(ToolLaneStats::new(1));
    let lane = ToolLane::new(
        tokio::runtime::Handle::current(),
        result_tx,
        Arc::new(Notify::new()),
        1,
        stats.clone(),
    );
    let serving = lane.serve(job_rx);
    let submit = |entity: u32, exec: crate::tool_bridge::BoxedToolExec| {
        stats.enqueued();
        job_tx
            .send(ToolJob {
                entity: Entity::from_raw_u32(entity).expect("a small index is a valid id"),
                exec,
                cancel: crate::cancel::CancelToken::new(),
            })
            .expect("the lane is serving");
    };

    // The whole lane, spent on waiting for an answer.
    let asking = hub.backend_for("agent-a");
    submit(
        1,
        Box::new(move || {
            Box::pin(async move {
                let response = asking.ask(req("q1")).await;
                vec![("q1".to_string(), response.value.unwrap_or_default().into())]
            })
        }),
    );
    wait_for_prompt(&hub).await;
    assert_eq!(stats.parked(), 1, "the asker stepped off the lane");

    // The answer, as another batch - only reachable if the lane is free.
    let answering = hub.clone();
    submit(
        2,
        Box::new(move || {
            Box::pin(async move {
                answering.answer(InteractionResponse::text("q1", "hello"));
                vec![("answered".to_string(), "ok".into())]
            })
        }),
    );

    let mut answers = Vec::new();
    for _ in 0..2 {
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), results.recv())
            .await
            .expect("both batches finished")
            .expect("an outcome arrived");
        answers.extend(
            outcome
                .results
                .into_iter()
                .map(|(id, r)| (id, r.into_string())),
        );
    }
    answers.sort();
    assert_eq!(
        answers,
        vec![
            ("answered".to_string(), "ok".to_string()),
            ("q1".to_string(), "hello".to_string()),
        ],
        "the asker got its answer from the batch behind it"
    );

    drop(job_tx);
    tokio::time::timeout(std::time::Duration::from_secs(30), serving)
        .await
        .expect("the lane drained")
        .expect("the lane task ended");
}

/// Block until a prompt is registered. `submit` inserts synchronously before
/// awaiting, but on a multi-threaded runtime the batch task may not have been
/// polled yet, so yielding a fixed number of times is not enough.
async fn wait_for_prompt(hub: &InteractionHub) {
    leviath_testkit::wait_until("the prompt was raised", || !hub.pending().is_empty()).await;
}

#[tokio::test]
async fn answer_unknown_request_is_false() {
    let hub = InteractionHub::new();
    assert!(!hub.answer(InteractionResponse::text("nope", "x")));
}

#[tokio::test]
async fn submit_and_answer_nudge_the_attached_wake() {
    let hub = InteractionHub::new();
    let wake = Arc::new(Notify::new());
    hub.attach_wake(wake.clone());
    // A second attach is ignored - the handle is set once at startup.
    hub.attach_wake(Arc::new(Notify::new()));

    let backend = hub.backend_for("agent-a");
    let asking = tokio::spawn(async move { backend.ask(req("q1")).await });
    settle().await;

    // submit() nudged the original wake.
    wake.notified().await;

    // answer() nudges it again (consume the submit permit first).
    assert!(hub.answer(InteractionResponse::text("q1", "hi")));
    wake.notified().await;
    assert_eq!(asking.await.unwrap().value.as_deref(), Some("hi"));
}

#[tokio::test]
async fn cancel_nudges_the_attached_wake() {
    let hub = InteractionHub::new();
    let wake = Arc::new(Notify::new());
    hub.attach_wake(wake.clone());

    let backend = hub.backend_for("agent-a");
    let asking = tokio::spawn(async move { backend.ask(req("q2")).await });
    settle().await;
    wake.notified().await; // drain the submit nudge

    assert!(hub.cancel("q2"));
    wake.notified().await; // cancel nudged the wake
    let _ = asking.await.unwrap();
}

#[tokio::test]
async fn cancel_wakes_submit_with_neutral_response() {
    let hub = InteractionHub::new();
    let backend = hub.backend_for("agent-a");
    let asking = tokio::spawn(async move { backend.ask(req("q2")).await });

    settle().await;
    assert!(hub.cancel("q2"));
    let response = asking.await.unwrap();
    assert_eq!(response.request_id, "q2");
    assert_eq!(response.value.as_deref(), Some("")); // neutral

    // Cancelling again ⇒ nothing to cancel.
    assert!(!hub.cancel("q2"));
}

// ─── the deadline on an unanswered prompt (issue #204) ───────────────────

#[tokio::test(start_paused = true)]
async fn a_prompt_nobody_answers_is_released_when_the_deadline_passes() {
    // With no deadline, a prompt nobody answers keeps its run in `WaitingInput`
    // for ever. With one, the hub resolves the request itself and the agent
    // goes back to work.
    let hub = InteractionHub::new();
    hub.set_timeout_secs(Some(60));
    let backend = hub.backend_for("agent-a");
    let asking = tokio::spawn(async move { backend.ask(req("q1")).await });

    settle().await;
    assert_eq!(hub.pending().len(), 1, "the prompt is open while it waits");

    // The paused clock jumps to the deadline once nothing else can run.
    let response = asking.await.unwrap();
    assert_eq!(response.request_id, "q1");
    // The same neutral answer a cancel produces: not approved, no text.
    assert_eq!(response.value.as_deref(), Some(""));
    assert_eq!(response.approved, None);
    assert!(
        hub.pending().is_empty(),
        "the expired request is off the open list, so the agent leaves Waiting"
    );
}

#[tokio::test(start_paused = true)]
async fn a_deadline_changes_nothing_for_a_prompt_that_is_answered() {
    // Setting a deadline must not alter the ordinary paths. Both of them run
    // here: one prompt answered by a person, one cancelled under it.
    let hub = InteractionHub::new();
    hub.set_timeout_secs(Some(3600));

    let answered_backend = hub.backend_for("agent-a");
    let answered = tokio::spawn(async move { answered_backend.ask(req("q1")).await });
    let cancelled_backend = hub.backend_for("agent-b");
    let cancelled = tokio::spawn(async move { cancelled_backend.ask(req("q2")).await });
    settle().await;

    assert!(hub.answer(InteractionResponse::text("q1", "yes, go on")));
    assert_eq!(answered.await.unwrap().value.as_deref(), Some("yes, go on"));

    assert!(hub.cancel("q2"));
    assert_eq!(cancelled.await.unwrap().value.as_deref(), Some(""));
}

/// With no deadline set, a prompt waits for a person however long that takes.
/// The clock is paused and advanced a full day, and the prompt is still open;
/// the answer that then arrives is the one the caller gets.
#[tokio::test(start_paused = true)]
async fn with_no_deadline_a_prompt_waits_for_a_person_however_long_it_takes() {
    let hub = InteractionHub::new();
    assert_eq!(hub.timeout_secs(), None, "nothing set: no deadline");
    let backend = hub.backend_for("agent-a");
    let asking = tokio::spawn(async move { backend.ask(req("q1")).await });

    settle().await;
    tokio::time::advance(Duration::from_secs(86_400)).await;
    settle().await;
    assert_eq!(hub.pending().len(), 1, "a day later, still waiting");

    assert!(hub.answer(InteractionResponse::text("q1", "here I am")));
    let response = asking.await.unwrap();
    assert_eq!(response.value.as_deref(), Some("here I am"));
    assert!(hub.pending().is_empty());
}

/// `Some(0)` is read as "no deadline", not "expire at once": a config that
/// spelled the wait as `interaction_timeout_secs = 0` keeps waiting.
#[tokio::test(start_paused = true)]
async fn a_zero_deadline_waits_for_a_person_however_long_it_takes() {
    let hub = InteractionHub::new();
    hub.set_timeout_secs(Some(0));
    assert_eq!(hub.timeout_secs(), None);
    let backend = hub.backend_for("agent-a");
    let asking = tokio::spawn(async move { backend.ask(req("q1")).await });

    settle().await;
    tokio::time::advance(Duration::from_secs(86_400)).await;
    settle().await;
    assert_eq!(hub.pending().len(), 1, "a day later, still waiting");

    assert!(hub.answer(InteractionResponse::text("q1", "here I am")));
    assert_eq!(asking.await.unwrap().value.as_deref(), Some("here I am"));
}

/// A configured deadline fires when it says: two seconds means two seconds,
/// not longer and not for ever.
#[tokio::test(start_paused = true)]
async fn a_two_second_deadline_fires_at_two_seconds() {
    let hub = InteractionHub::new();
    hub.set_timeout_secs(Some(2));
    let backend = hub.backend_for("agent-a");
    let asking = tokio::spawn(async move { backend.ask(req("q1")).await });

    settle().await;
    tokio::time::advance(Duration::from_millis(1_900)).await;
    settle().await;
    assert_eq!(
        hub.pending().len(),
        1,
        "still open just short of the deadline"
    );

    tokio::time::advance(Duration::from_millis(200)).await;
    settle().await;
    assert!(
        hub.pending().is_empty(),
        "released once two seconds have passed"
    );
    let response = asking.await.unwrap();
    assert_eq!(response.approved, None);
    assert_eq!(response.value.as_deref(), Some(""));
}

#[tokio::test(start_paused = true)]
async fn the_deadline_denies_rather_than_approves() {
    // A timeout must never be read as consent: an approval prompt and a
    // taint gate both go through `response_approved`, which reads the
    // neutral response as "no".
    let hub = InteractionHub::new();
    hub.set_timeout_secs(Some(30));
    let backend = hub.backend_for("agent-a");
    let asking = tokio::spawn(async move {
        backend
            .ask(InteractionRequest::tool_approval(
                "t1",
                "shell",
                serde_json::json!({"command": "rm -rf /"}),
                "implement",
                &[],
            ))
            .await
    });

    let response = asking.await.unwrap();
    assert!(!leviath_core::interaction::response_approved(&response));
}

#[tokio::test]
async fn an_answer_that_lands_as_the_deadline_passes_still_wins() {
    // The race: `answer` took the entry out of the registry and sent its
    // response an instant before the timer fired. Handing back the neutral
    // response here would throw away what a person actually said.
    let hub = InteractionHub::new();
    let (responder, mut rx) = oneshot::channel();
    responder
        .send(InteractionResponse::text("q1", "approved by hand"))
        .expect("the receiver is still alive");

    let response = hub.expire("agent-a", "q1", &mut rx);
    assert_eq!(response.value.as_deref(), Some("approved by hand"));
}

/// The deadline reads back through the hub and through a per-agent backend,
/// which is how a tool result can name it when a prompt expires.
#[test]
fn the_timeout_reads_back_through_the_hub_and_its_backends() {
    let hub = InteractionHub::new();
    assert_eq!(hub.timeout_secs(), None, "a fresh hub waits indefinitely");
    hub.set_timeout_secs(Some(7));
    assert_eq!(hub.timeout_secs(), Some(7));
    let backend = hub.backend_for("agent-a");
    assert_eq!(backend.timeout_secs(), Some(7));
    // A caller minting a request id asks the backend whose run it is: the id
    // has to carry it, and the backend is what the caller holds.
    assert_eq!(backend.agent_id(), "agent-a");
    hub.set_timeout_secs(None);
    assert_eq!(hub.timeout_secs(), None, "cleared again");
}

/// A deny that carries feedback comes out of `ask` exactly as it went into
/// `answer`: the hub is a channel, and the text is part of the answer.
#[tokio::test]
async fn a_deny_with_feedback_round_trips_through_the_hub() {
    let hub = InteractionHub::new();
    let backend = hub.backend_for("agent-a");
    let asking = tokio::spawn(async move {
        backend
            .ask(InteractionRequest::tool_approval(
                "q1",
                "bash",
                serde_json::json!({"command": "rm -rf build"}),
                "stage",
                &[],
            ))
            .await
    });
    settle().await;
    let sent = InteractionResponse::deny_with_feedback("q1", "keep build/, clean only dist/");
    assert!(hub.answer(sent.clone()));
    let got = asking.await.unwrap();
    assert_eq!(got, sent);
    assert_eq!(got.deny_feedback(), Some("keep build/, clean only dist/"));
    assert!(hub.pending().is_empty());
}

/// The same answer over the control socket's wire shape and through the
/// run journal: what `lev respond` or the API sends is what the daemon reads,
/// and the tool result it becomes survives the archive a resumed run replays.
#[test]
fn a_deny_with_feedback_survives_the_wire_and_the_journal() {
    use leviath_core::run_archive::{
        RUN_ARCHIVE_VERSION, RunRecord, read_archive, write_archive_start, write_record,
    };
    let wire = r#"{"request_id":"q1","value":null,"choice_index":null,"approved":false,"scope":"once","feedback":"use the API"}"#;
    let parsed: InteractionResponse = serde_json::from_str(wire).unwrap();
    assert_eq!(
        parsed,
        InteractionResponse::deny_with_feedback("q1", "use the API")
    );
    let mut buf = Vec::new();
    write_archive_start(&mut buf, RUN_ARCHIVE_VERSION).unwrap();
    write_record(
        &mut buf,
        &RunRecord::ToolCallDone {
            execution_id: String::new(),
            outcome: None,
            iteration: 1,
            call_id: "c1".to_string(),
            result: "[denied] User declined tool call 'bash'. Feedback: use the API"
                .to_string()
                .into(),
            at: 0,
        },
    )
    .unwrap();
    let (_, records) = read_archive(&mut buf.as_slice()).unwrap();
    assert!(matches!(
        &records[0],
        RunRecord::ToolCallDone { result, .. } if result.ends_with("Feedback: use the API")
    ));
}

// ─── The record of what a person answered ─────────────────────────────────────

/// Each way a request settles is recorded, and the three are told apart.
///
/// They have to be: an answer, a request nobody answered in time, and one
/// withdrawn when the run was cancelled all hand the waiting caller the same
/// neutral response, so the record is the only thing that distinguishes them.
#[tokio::test]
async fn each_way_a_request_ends_is_recorded_as_itself() {
    use leviath_core::interaction::{ApprovalScope, Settlement};

    let hub = InteractionHub::new();

    // Answered: an approval, with the scope the person chose.
    let asked = hub.clone();
    let answered = tokio::spawn(async move {
        asked
            .submit(
                "run-1",
                InteractionRequest::tool_approval(
                    "approve-1",
                    "shell",
                    serde_json::json!({"command": "rm -rf build"}),
                    "implement",
                    &[],
                ),
            )
            .await
    });
    settle().await;
    hub.answer(InteractionResponse {
        request_id: "approve-1".to_string(),
        value: None,
        choice_index: Some(1),
        approved: Some(true),
        scope: Some(ApprovalScope::Run),
        feedback: None,
        parts: Vec::new(),
    });
    answered.await.expect("the ask completes");

    // Cancelled: the run went away with the question still open.
    let asked = hub.clone();
    let cancelled = tokio::spawn(async move { asked.submit("run-1", req("ask-2")).await });
    settle().await;
    assert!(hub.cancel("ask-2"));
    cancelled.await.expect("the ask completes");

    let settled = hub.take_settled();
    assert_eq!(settled.len(), 2, "one record per question: {settled:?}");

    let (run, first) = &settled[0];
    assert_eq!(run, "run-1", "the record names the run that asked");
    assert_eq!(first.request_id, "approve-1");
    assert_eq!(first.tool.as_deref(), Some("shell"));
    assert_eq!(first.stage, "implement");
    assert!(
        first.prompt.contains("rm -rf build"),
        "the prompt is what the person saw: {}",
        first.prompt
    );
    let Settlement::Answered {
        approved,
        scope,
        choice,
        ..
    } = &first.settlement
    else {
        panic!("an answered approval is answered: {:?}", first.settlement);
    };
    assert_eq!(*approved, Some(true));
    assert_eq!(*scope, Some(ApprovalScope::Run), "and at which scope");
    assert_eq!(*choice, Some(1));
    assert!(first.asked_at <= first.at, "asked before it settled");

    assert_eq!(settled[1].1.settlement, Settlement::Cancelled);

    // Drained, so the next tick does not write them again.
    assert!(hub.take_settled().is_empty());
}

/// A request nobody answers in time is recorded as the timeout it was.
///
/// On a paused clock, because `Some(0)` means "no deadline" here: a deadline
/// that fires has to be a real one, advanced past.
#[tokio::test(start_paused = true)]
async fn an_unanswered_request_is_recorded_as_a_timeout() {
    use leviath_core::interaction::Settlement;

    let hub = InteractionHub::new();
    hub.set_timeout_secs(Some(2));
    let asked = hub.clone();
    let expired = tokio::spawn(async move { asked.submit("run-2", req("ask-late")).await });
    settle().await;
    tokio::time::advance(Duration::from_secs(3)).await;
    settle().await;
    expired.await.expect("the ask completes");

    let settled = hub.take_settled();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].1.settlement, Settlement::TimedOut);
    assert_eq!(settled[0].0, "run-2");
    assert!(
        settled[0].1.asked_at <= settled[0].1.at,
        "asked before it gave up"
    );
}

/// Cancelling a run records every question it still had open, so a cancelled
/// run does not look like one nobody ever asked anything.
#[tokio::test]
async fn cancelling_a_run_records_each_open_question() {
    use leviath_core::interaction::Settlement;

    let hub = InteractionHub::new();
    for id in ["ask-a", "ask-b"] {
        let asked = hub.clone();
        tokio::spawn(async move { asked.submit("run-3", req(id)).await });
    }
    // Somebody else's question, which a cancel of run-3 must not touch.
    let other = hub.clone();
    tokio::spawn(async move { other.submit("run-4", req("ask-c")).await });
    settle().await;

    assert_eq!(hub.cancel_for_agent("run-3"), 2);

    let settled = hub.take_settled();
    assert_eq!(settled.len(), 2);
    assert!(
        settled
            .iter()
            .all(|(run, r)| run == "run-3" && r.settlement == Settlement::Cancelled),
        "{settled:?}"
    );
    let mut ids: Vec<&str> = settled.iter().map(|(_, r)| r.request_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["ask-a", "ask-b"]);
}

/// Two runs, one provider that names both their tool calls `call_1`, and both
/// asking at once. Each gets its own answer.
///
/// This is the shape that used to lose one: the hub holds every run's open
/// requests under one key space, so with ids that carried only the tool-call id
/// the second ask replaced the first, the first run was handed the neutral
/// answer nobody gave it, and the answer a person then gave to "that" id went
/// to the second run.
#[tokio::test]
async fn two_runs_whose_provider_repeats_a_tool_call_id_each_keep_their_own_prompt() {
    use leviath_core::interaction::ApprovalScope;

    let hub = InteractionHub::new();
    // What the two runs' providers minted. Identical, which is what a
    // per-conversation counter produces and what the mock provider does.
    let tool_call_id = "call_1";

    let mut asks = Vec::new();
    for run in ["run-a", "run-b"] {
        let asked = hub.clone();
        let id = leviath_core::interaction::request_id(run, "approve", tool_call_id);
        asks.push((
            run,
            id.clone(),
            tokio::spawn(async move {
                asked
                    .submit(
                        run,
                        InteractionRequest::tool_approval(
                            id,
                            "shell",
                            serde_json::json!({"command": "echo hello"}),
                            "main",
                            &[],
                        ),
                    )
                    .await
            }),
        ));
    }
    settle().await;

    // Both are open, and they are two different requests.
    let open = hub.pending();
    assert_eq!(open.len(), 2, "both runs are waiting: {open:?}");
    assert_eq!(
        asks[0].1, "run-a-approve-call_1",
        "the id names the run that asked"
    );
    assert_ne!(asks[0].1, asks[1].1, "two runs, two ids");

    // Answering one names one. `run-a` is denied, `run-b` approved, and neither
    // gets the other's answer.
    for (run, id, _) in &asks {
        let approved = *run == "run-b";
        assert!(
            hub.answer(InteractionResponse {
                request_id: id.clone(),
                value: None,
                choice_index: None,
                approved: Some(approved),
                scope: Some(ApprovalScope::Once),
                feedback: None,
                parts: Vec::new(),
            }),
            "{id} is open"
        );
    }
    for (run, _, task) in asks {
        let answer = task.await.expect("the ask completes");
        assert_eq!(
            answer.approved,
            Some(run == "run-b"),
            "{run} got its own answer"
        );
    }
    assert!(hub.pending().is_empty(), "nothing is left open");
}

/// An id that is somehow already open refuses the arriving request instead of
/// replacing the one open.
///
/// Unreachable through the ids this daemon mints, which is why it is an error
/// in the log and a settlement of its own in the journal rather than a quiet
/// replacement: the request already open may be on somebody's screen.
#[tokio::test]
async fn a_second_request_under_an_open_id_is_refused_not_swapped_in() {
    use leviath_core::interaction::Settlement;

    let hub = InteractionHub::new();
    let asked = hub.clone();
    let first = tokio::spawn(async move { asked.submit("run-a", req("same-id")).await });
    settle().await;

    // The arriving one is answered immediately, with the neutral response an
    // approval reads as not-approved.
    let arriving = hub.submit("run-b", req("same-id")).await;
    assert_eq!(arriving.request_id, "same-id");
    assert_eq!(arriving.value.as_deref(), Some(""), "the neutral answer");
    assert_eq!(arriving.approved, None, "nothing was approved");

    // The one already open is untouched, and still belongs to the run that
    // opened it.
    let open = hub.pending();
    assert_eq!(open.len(), 1, "the open request stayed: {open:?}");
    assert_eq!(open[0].0, "run-a", "and it is still run-a's");
    assert!(!first.is_finished(), "run-a is still waiting");

    // The refusal is in the journal, against the run that was refused, and it
    // is not a denial and not a cancellation.
    let settled = hub.take_settled();
    assert_eq!(settled.len(), 1, "one record: {settled:?}");
    assert_eq!(settled[0].0, "run-b", "recorded against the run refused");
    assert_eq!(settled[0].1.request_id, "same-id");
    assert_eq!(settled[0].1.settlement, Settlement::Refused);

    // And the run that kept its prompt can still be answered.
    assert!(hub.answer(InteractionResponse::text("same-id", "go on")));
    let answer = first.await.expect("the ask completes");
    assert_eq!(answer.value.as_deref(), Some("go on"));
}

/// Two prompts from one run never share an id, however the provider numbered
/// the calls behind them.
///
/// A provider numbers its tool calls within one message, so a run's second
/// turn asks under `call_1` again. The hub draws the tail itself, so the
/// second prompt is `-2` whatever the call was called, and an answer to one
/// cannot land on the other.
#[test]
fn a_run_draws_a_fresh_id_for_every_prompt() {
    let hub = InteractionHub::new();
    let backend = hub.backend_for("run-a");
    assert_eq!(backend.request_id("ask"), "run-a-ask-1");
    assert_eq!(backend.request_id("ask"), "run-a-ask-2");
    // The kind is in the id but not in the count: one series per run.
    assert_eq!(backend.request_id("approve"), "run-a-approve-3");
}

/// Two runs count on their own, so the daemon-wide hub cannot let one run's
/// prompts crowd another's numbering.
#[test]
fn two_runs_count_their_prompts_apart() {
    let hub = InteractionHub::new();
    let a = hub.backend_for("run-a");
    let b = hub.backend_for("run-b");
    assert_eq!(a.request_id("ask"), "run-a-ask-1");
    assert_eq!(b.request_id("ask"), "run-b-ask-1");
    assert_eq!(a.request_id("ask"), "run-a-ask-2");
}

/// A reaped run's count is let go, and a run that comes back under the same
/// id after that starts again: nothing of the old run's is open to collide
/// with.
#[test]
fn a_forgotten_run_starts_its_count_again() {
    let hub = InteractionHub::new();
    let backend = hub.backend_for("run-a");
    assert_eq!(backend.request_id("gate"), "run-a-gate-1");
    hub.forget_run("run-a");
    assert_eq!(backend.request_id("gate"), "run-a-gate-1");
    // Forgetting a run the hub never counted for is nothing.
    hub.forget_run("run-z");
}
