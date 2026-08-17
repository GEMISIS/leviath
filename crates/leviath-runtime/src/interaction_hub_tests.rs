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
    // dashboard (issue #109).
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
/// That is the shape that froze whole factories in issue #191: everything
/// looked `waiting`, nothing was failed, and nothing ever moved again.
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
                vec![("q1".to_string(), response.value.unwrap_or_default())]
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
                vec![("answered".to_string(), "ok".to_string())]
            })
        }),
    );

    let mut answers = Vec::new();
    for _ in 0..2 {
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), results.recv())
            .await
            .expect("both batches finished")
            .expect("an outcome arrived");
        answers.extend(outcome.results);
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
    // The zombie in issue #204: six runs sat in `WaitingInput` for hours
    // because nothing ever aged the request out. Now the hub resolves it
    // itself and the agent goes back to work.
    let hub = InteractionHub::new();
    hub.set_timeout_secs(60);
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
    hub.set_timeout_secs(3600);

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

#[tokio::test(start_paused = true)]
async fn a_zero_deadline_waits_for_a_person_however_long_it_takes() {
    // `0` is the explicit "I will be here" setting, and it has to keep the
    // old behaviour exactly: the prompt stays open until answered.
    let hub = InteractionHub::new();
    hub.set_timeout_secs(0);
    let backend = hub.backend_for("agent-a");
    let asking = tokio::spawn(async move { backend.ask(req("q1")).await });

    settle().await;
    tokio::time::advance(Duration::from_secs(86_400)).await;
    assert_eq!(hub.pending().len(), 1, "a day later, still waiting");

    assert!(hub.answer(InteractionResponse::text("q1", "here I am")));
    assert_eq!(asking.await.unwrap().value.as_deref(), Some("here I am"));
}

#[tokio::test(start_paused = true)]
async fn the_deadline_denies_rather_than_approves() {
    // A timeout must never be read as consent: an approval prompt and a
    // taint gate both go through `response_approved`, which reads the
    // neutral response as "no".
    let hub = InteractionHub::new();
    hub.set_timeout_secs(30);
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
