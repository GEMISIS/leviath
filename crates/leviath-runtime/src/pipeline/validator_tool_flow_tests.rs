//! A validator tool end to end through the systems that implement it:
//! dispatch holds the batch and sends the call, `collect_tools` files the
//! verdict, and dispatch applies the batch with it.

use super::*;
use crate::pipeline::validator_tool::{PendingValidation, ValidatorVerdicts};

const CHECKER: &str = "pw__check_panel";

/// A stage whose answers `pw__check_panel` judges, under `policy`.
fn judged(
    calls: Vec<crate::components::ToolCall>,
    policy: Option<leviath_core::output::OnValidatorError>,
    format: Option<&str>,
) -> (StageInference, crate::components::InferenceResult) {
    let (mut offers, mut result) = infer_with(calls);
    offers.output = Some(leviath_core::output::OutputSpec {
        format: format.map(str::to_string),
        on_validator_error: policy,
        validator_tool: Some(leviath_core::output::ValidatorTool {
            tool: CHECKER.to_string(),
            content_arg: "panel".to_string(),
            args: Default::default(),
        }),
        ..Default::default()
    });
    result.attempt_id = "attempt-1".to_string();
    (offers, result)
}

/// A world with a lane and a results channel, the tests' end of each.
type Wired = (
    World,
    mpsc::UnboundedReceiver<crate::pipeline::ToolJob>,
    mpsc::UnboundedSender<ToolOutcome>,
);

fn wired() -> Wired {
    let (mut world, jobs) = world_with_lane();
    let (tx, rx) = mpsc::unbounded_channel();
    world.insert_resource(ToolResults(rx));
    (world, jobs, tx)
}

/// An agent in a stage with nowhere else to go, whose turn made `calls`.
fn agent(
    world: &mut World,
    calls: Vec<crate::components::ToolCall>,
    policy: Option<leviath_core::output::OnValidatorError>,
) -> Entity {
    let alone = tg::graph(vec![tg::stage("s")], tg::layout(vec![], 10_000));
    world
        .spawn((
            agent_state(),
            judged(calls, policy, Some("json")),
            output_window(),
            ReadyForTools,
            run_metadata(),
            spec_of(alone),
            StageCursor { index: 0 },
            crate::components::OutputValidators::new(Default::default()),
        ))
        .id()
}

/// The validator's answer for `call_id` lands.
fn lands(
    world: &mut World,
    tx: &mpsc::UnboundedSender<ToolOutcome>,
    e: Entity,
    results: &[(&str, &str)],
) {
    tx.send(ToolOutcome {
        elapsed: std::time::Duration::ZERO,
        entity: e,
        results: results
            .iter()
            .map(|(id, text)| ((*id).to_string(), (*text).into()))
            .collect(),
    })
    .unwrap();
    run_collect_tools(world);
}

/// The next turn of the same agent made `calls`.
fn next_turn(world: &mut World, e: Entity, calls: Vec<crate::components::ToolCall>, attempt: &str) {
    let (_, mut result) = infer_with(calls);
    result.attempt_id = attempt.to_string();
    world
        .entity_mut(e)
        .remove::<ReadyToInfer>()
        .insert((result, ReadyForTools));
}

fn broken(world: &World, e: Entity) -> Vec<String> {
    world
        .get::<crate::components::OutputValidators>(e)
        .unwrap()
        .broken_names()
}

/// The acceptance: a submission the tool refuses gets the reason as its
/// `submit_output` result and records nothing; the next one it passes is
/// recorded, and a stage with nowhere else to go ends on it, after the
/// verdict rather than before.
#[tokio::test]
async fn a_refused_answer_is_sent_back_and_the_next_one_it_passes_ends_the_stage() {
    let (mut world, mut jobs, tx) = wired();
    let e = agent(&mut world, vec![submit_call("o1", "{\"id\": 1}")], None);

    dispatch_once(&mut world);
    // Held: nothing applied, the call is out on the lane.
    assert!(world.get::<ReadyForTools>(e).is_none());
    assert!(world.get::<crate::persistence::FinalOutput>(e).is_none());
    let pending = world.get::<PendingValidation>(e).expect("held").clone();
    assert_eq!(pending.attempt, "attempt-1");
    assert_eq!(pending.sites.len(), 1);
    assert_eq!(pending.sites[0].call_id, "o1");
    assert_eq!(pending.sites[0].tool, CHECKER);
    assert_eq!(jobs.try_recv().expect("a job").entity, e);
    // Not a turn the model waits on, and nothing a person is asked about.
    assert!(world.get::<AwaitingTools>(e).is_none());
    assert!(
        world
            .get::<crate::pipeline::lane_batch::PendingBatch>(e)
            .is_none()
    );

    lands(
        &mut world,
        &tx,
        e,
        &[(
            "validator-o1",
            r#"{"ok": false, "reason": "the panel has no title"}"#,
        )],
    );
    assert!(world.get::<PendingValidation>(e).is_none());
    assert!(world.get::<ReadyForTools>(e).is_some(), "released");
    assert!(world.get::<ValidatorVerdicts>(e).is_some());

    dispatch_once(&mut world);
    assert!(world.get::<crate::persistence::FinalOutput>(e).is_none());
    assert!(world.get::<ValidatorVerdicts>(e).is_none(), "spent");
    assert!(
        world.get::<ReadyToInfer>(e).is_some(),
        "the stage carries on"
    );
    let text = conversation_text(&world, e);
    assert!(
        text.contains("[error] the final output was rejected: the panel has no title"),
        "{text}"
    );
    assert!(jobs.try_recv().is_err(), "nothing else went out");

    next_turn(
        &mut world,
        e,
        vec![submit_call("o2", "{\"id\": 2}")],
        "attempt-2",
    );
    dispatch_once(&mut world);
    assert_eq!(
        world.get::<PendingValidation>(e).unwrap().sites[0].call_id,
        "o2"
    );
    lands(&mut world, &tx, e, &[("validator-o2", r#"{"ok": true}"#)]);
    dispatch_once(&mut world);
    let recorded = world
        .get::<crate::persistence::FinalOutput>(e)
        .expect("recorded");
    assert_eq!(recorded.0.content, "{\"id\": 2}");
    assert!(
        world.get::<ResolveTransition>(e).is_some(),
        "the stage ends"
    );
    assert!(broken(&world, e).is_empty());
}

/// A fan-out worker ends on the first submission the tool passes, though its
/// stage has somewhere else to go: the verdict comes first, then the end.
#[tokio::test]
async fn a_fan_out_worker_ends_on_the_first_answer_the_tool_passes() {
    let (mut world, _jobs, tx) = wired();
    let onward = tg::graph(
        vec![tg::stage("s"), tg::stage("t")],
        tg::layout(vec![], 10_000),
    );
    let mut spec = (*spec_of(onward).0).clone();
    spec.placement.worker_stage = Some(crate::spec::names::StageName::new("s").unwrap());
    let e = agent(&mut world, vec![submit_call("o1", "{}")], None);
    world
        .entity_mut(e)
        .insert(crate::insert::RunSpecC(std::sync::Arc::new(spec)));
    dispatch_once(&mut world);
    assert!(
        world.get::<ResolveTransition>(e).is_none(),
        "not before the verdict"
    );
    lands(&mut world, &tx, e, &[("validator-o1", r#"{"ok": true}"#)]);
    dispatch_once(&mut world);
    assert!(world.get::<crate::persistence::FinalOutput>(e).is_some());
    assert!(world.get::<ResolveTransition>(e).is_some());
}

/// A tool that cannot answer (its server is down) refuses under `reject`
/// with the error, and records unchecked under `accept`. Either way the run
/// names the tool among the checks that did not work.
#[tokio::test]
async fn a_tool_that_cannot_answer_follows_on_validator_error() {
    use leviath_core::output::OnValidatorError;
    let down = "[error] MCP server 'pw' is not connected";
    let (mut world, _jobs, tx) = wired();
    let reject = agent(&mut world, vec![submit_call("o1", "{}")], None);
    let accept = agent(
        &mut world,
        vec![submit_call("o1", "{}")],
        Some(OnValidatorError::Accept),
    );
    dispatch_once(&mut world);
    for e in [reject, accept] {
        lands(&mut world, &tx, e, &[("validator-o1", down)]);
    }
    dispatch_once(&mut world);

    assert!(
        world
            .get::<crate::persistence::FinalOutput>(reject)
            .is_none()
    );
    let text = conversation_text(&world, reject);
    assert!(
        text.contains("[error] the final output was rejected"),
        "{text}"
    );
    assert!(text.contains("not connected"), "{text}");
    assert!(world.get::<ReadyToInfer>(reject).is_some());

    assert!(
        world
            .get::<crate::persistence::FinalOutput>(accept)
            .is_some()
    );
    assert!(world.get::<ResolveTransition>(accept).is_some());
    for e in [reject, accept] {
        assert_eq!(broken(&world, e), [CHECKER]);
    }
}

/// A call the lane never answers, a refusal by policy and an answer that is
/// no verdict are all a tool that could not run.
#[tokio::test]
async fn no_answer_is_a_tool_that_could_not_run() {
    let (mut world, _jobs, tx) = wired();
    let e = agent(&mut world, vec![submit_call("o1", "{}")], None);
    dispatch_once(&mut world);
    lands(&mut world, &tx, e, &[]);
    dispatch_once(&mut world);
    let text = conversation_text(&world, e);
    assert!(text.contains("never answered"), "{text}");
}

/// The cheap checks come first: an answer that is not even the format asked
/// for is refused without a tool call, as is the answer the visit already
/// holds, handed back again.
#[tokio::test]
async fn an_answer_the_cheaper_checks_refuse_never_reaches_the_tool() {
    let (mut world, mut jobs, _tx) = wired();
    let e = agent(&mut world, vec![submit_call("o1", "not json")], None);
    dispatch_once(&mut world);
    assert!(jobs.try_recv().is_err(), "no call went out");
    assert!(world.get::<PendingValidation>(e).is_none());
    let text = conversation_text(&world, e);
    assert!(text.contains("is not valid json"), "{text}");

    // The held answer again: a repeat, answered without asking.
    let onward = tg::graph(
        vec![tg::stage("s"), tg::stage("t")],
        tg::layout(vec![], 10_000),
    );
    let answer = leviath_core::output::FinalOutput::new("{}", None, "s".into(), 0);
    let repeat = world
        .spawn((
            agent_state(),
            judged(vec![submit_call("o9", "{}")], None, None),
            output_window(),
            ReadyForTools,
            run_metadata(),
            spec_of(onward),
            StageCursor { index: 0 },
            crate::persistence::FinalOutput(answer),
            crate::pipeline::VisitSubmissions {
                visit: agent_state().current_visit,
                repeats: 0,
            },
        ))
        .id();
    dispatch_once(&mut world);
    assert!(jobs.try_recv().is_err(), "a repeat is not judged again");
    assert!(world.get::<PendingValidation>(repeat).is_none());
    assert!(
        conversation_text(&world, repeat).contains("Already recorded"),
        "{}",
        conversation_text(&world, repeat)
    );
}

/// A submission the batch already finished before a restart is not judged
/// again: its result is what the run's file recorded.
#[tokio::test]
async fn a_submission_finished_before_a_restart_is_not_judged_again() {
    let (mut world, mut jobs, _tx) = wired();
    let e = agent(
        &mut world,
        vec![submit_call("o1", "{}"), tc("c_read", "n")],
        None,
    );
    world
        .entity_mut(e)
        .insert(crate::pipeline::RecoveredResults(vec![(
            "c_read".to_string(),
            "read it".into(),
        )]));
    dispatch_once(&mut world);
    assert!(
        world.get::<PendingValidation>(e).is_some(),
        "o1 is still to judge"
    );
    let _ = jobs.try_recv();

    let done = agent(&mut world, vec![submit_call("o1", "{}")], None);
    world
        .entity_mut(done)
        .insert(crate::pipeline::RecoveredResults(vec![(
            "o1".to_string(),
            "Recorded as this run's final output.".into(),
        )]));
    dispatch_once(&mut world);
    assert!(world.get::<PendingValidation>(done).is_none());
    assert!(jobs.try_recv().is_err());
}

/// Verdicts belong to the model call whose batch they judged: a later turn
/// asks again rather than borrowing them.
#[tokio::test]
async fn verdicts_for_another_turn_are_not_this_ones() {
    let (mut world, mut jobs, _tx) = wired();
    let e = agent(&mut world, vec![submit_call("o1", "{}")], None);
    let mut stale = std::collections::HashMap::new();
    stale.insert(
        "o1".to_string(),
        leviath_scripting::output_validator::Verdict::Valid,
    );
    world.entity_mut(e).insert(ValidatorVerdicts {
        attempt: "an-earlier-turn".to_string(),
        verdicts: stale,
    });
    dispatch_once(&mut world);
    assert!(world.get::<PendingValidation>(e).is_some(), "asked again");
    assert!(jobs.try_recv().is_ok());
}

/// A daemon that stops while the tool is judging comes back with the turn's
/// batch still to dispatch, and dispatching it asks the tool again, which
/// reaches the same verdict.
#[tokio::test]
async fn a_restart_during_validation_asks_again() {
    let (mut world, mut jobs, tx) = wired();
    let e = agent(&mut world, vec![submit_call("o1", "{}")], None);
    dispatch_once(&mut world);
    assert!(world.get::<PendingValidation>(e).is_some());
    let _ = jobs.try_recv();

    // What the run's state records while it is held: the batch, undispatched.
    let held = crate::state::inspect::pending_of(&world, e).expect("the batch is in flight");
    assert_eq!(held.calls.len(), 1);
    assert_eq!(held.calls[0].id, "o1");
    assert!(held.done.is_empty(), "nothing in it has run");
    // Recorded as held, so a daemon that dies here interrupts nothing in it.
    assert!(held.held.is_some());
    let mut crashed = crate::runfile::reader_tests::initial();
    crashed.pending = Some(held.clone());
    crate::restore::interrupt_in_flight(&mut crashed);
    assert!(crashed.pending.unwrap().done.is_empty());

    // A fresh world, as a restart builds it.
    let (mut after, mut jobs, tx_after) = wired();
    let alone = tg::graph(vec![tg::stage("s")], tg::layout(vec![], 10_000));
    let (offers, _) = judged(vec![submit_call("o1", "{}")], None, Some("json"));
    let resumed = after
        .spawn((
            agent_state(),
            offers,
            output_window(),
            run_metadata(),
            spec_of(alone),
            StageCursor { index: 0 },
            crate::components::OutputValidators::new(Default::default()),
        ))
        .id();
    crate::insert::place::pending_batch(&mut after.entity_mut(resumed), &held);
    dispatch_once(&mut after);
    let pending = after
        .get::<PendingValidation>(resumed)
        .expect("asked again");
    assert_eq!(pending.sites[0].call_id, "o1");
    assert!(jobs.try_recv().is_ok());
    lands(
        &mut after,
        &tx_after,
        resumed,
        &[("validator-o1", r#"{"ok": true}"#)],
    );
    dispatch_once(&mut after);
    assert!(
        after
            .get::<crate::persistence::FinalOutput>(resumed)
            .is_some()
    );
    drop(tx);
}

/// The taint gate looks at a validator call as it would at the model's own:
/// a block is the verdict and nobody is asked, `--yolo` waives it and says
/// so in the audit, and a run with no gate is not held to one.
#[test]
fn the_taint_gate_judges_a_validator_call() {
    use crate::pipeline::validator_tool::{GateLook, gate_word};
    let window = tainted_conv_window();
    let policy = leviath_core::PolicyConfig::default();
    let look = |waived| GateLook {
        agent_id: "a",
        stage: 0,
        window: &window,
        policy: &policy,
        rules: None,
        waived,
    };
    let mut gate = enabled_gate();
    let blocked = gate_word(Some(&mut gate), &look(false), "shell").expect("blocked");
    assert!(blocked.starts_with("[blocked]"), "{blocked}");
    assert_eq!(gate_word(Some(&mut gate), &look(true), "shell"), None);
    assert_eq!(gate_word(Some(&mut gate), &look(false), "read_file"), None);
    assert_eq!(gate_word(None, &look(false), "shell"), None);
}

/// A tool the gate blocks is never sent: every submission it would judge
/// gets the block as a verdict, and the batch goes ahead with it.
#[tokio::test]
async fn a_blocked_validator_is_not_sent() {
    // An MCP tool is outbound until the operator says otherwise, so over
    // Internal data the gate blocks it.
    let (mut world, mut jobs, _tx) = wired();
    let e = agent(&mut world, vec![submit_call("o1", "{}")], None);
    world
        .entity_mut(e)
        .insert((enabled_gate(), tainted_output_window()));
    dispatch_once(&mut world);
    assert!(world.get::<PendingValidation>(e).is_none());
    assert!(jobs.try_recv().is_err());
    assert!(world.get::<crate::persistence::FinalOutput>(e).is_none());
}
