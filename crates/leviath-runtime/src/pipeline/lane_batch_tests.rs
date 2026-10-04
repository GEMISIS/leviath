//! Each call of a batch decided in the world, approvals held there, and the
//! batch sent with what was decided.

use super::*;
use crate::components::{InferenceResult, ToolCall};
use crate::tool_bridge::BoxedToolExec;
use leviath_core::interaction::{ApprovalScope, InteractionResponse};
use std::sync::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// A service whose verdict is the call's name: `ok` runs (charging 5), `no`
/// is refused, `q` is a question to a person, and `ask` needs approval unless
/// the key `k` was granted (charging 7). Records what it is handed to run.
#[derive(Default)]
struct Judge {
    ran: Mutex<Vec<(String, Decision, u64)>>,
    seen_written: Mutex<Vec<u64>>,
}

impl ToolService for Judge {
    fn exec_for(
        &self,
        _entity: Entity,
        _calls: Vec<leviath_providers::ToolCall>,
        _progress: ToolProgress,
    ) -> BoxedToolExec {
        Box::new(|| Box::pin(async { Vec::new() }))
    }

    fn decide(
        &self,
        _entity: Entity,
        call: &leviath_providers::ToolCall,
        ctx: &DecideCtx<'_>,
    ) -> ToolVerdict {
        self.seen_written.lock().unwrap().push(ctx.written);
        match call.name.as_str() {
            "ok" => ToolVerdict::Run { charge: 5 },
            "no" => ToolVerdict::Refuse("nope".to_string()),
            "q" => ToolVerdict::Interact { attended: true },
            _ if ctx.grants.granted("k") => ToolVerdict::Run { charge: 0 },
            _ => ToolVerdict::Ask {
                keys: vec!["k".to_string()],
                charge: 7,
            },
        }
    }

    fn exec_decided(
        &self,
        _entity: Entity,
        calls: Vec<DecidedCall>,
        written: u64,
        _progress: ToolProgress,
    ) -> BoxedToolExec {
        let mut ran = self.ran.lock().unwrap();
        for c in calls {
            ran.push((c.call.name, c.decision, written));
        }
        Box::new(|| Box::pin(async { Vec::new() }))
    }

    fn written(&self, _entity: Entity) -> Option<u64> {
        Some(42)
    }
}

/// A world with the lane systems and `judge` installed, and the receiver the
/// lane's jobs land on.
fn world_with(judge: Arc<Judge>) -> (World, UnboundedReceiver<ToolJob>) {
    let mut world = World::new();
    let (jobs, rx) = unbounded_channel();
    world.insert_resource(ToolServiceRes(judge));
    world.insert_resource(ToolStage::detached(jobs));
    (world, rx)
}

/// Give `world` a hub and an approval lane; returns the hub.
fn with_prompts(world: &mut World) -> InteractionHub {
    let hub = InteractionHub::new();
    world.insert_resource(hub.clone());
    crate::approval_prompt::install(
        world,
        tokio::runtime::Handle::current(),
        Arc::new(tokio::sync::Notify::new()),
    );
    hub
}

/// An agent holding a batch of calls named `names`, with grants and a ledger.
fn agent(world: &mut World, names: &[&str]) -> Entity {
    let calls: Vec<leviath_providers::ToolCall> = names
        .iter()
        .enumerate()
        .map(|(i, name)| leviath_providers::ToolCall {
            id: format!("c{i}"),
            name: name.to_string(),
            arguments: serde_json::json!({}),
            thought_signature: None,
        })
        .collect();
    world
        .spawn((
            AgentState {
                agent_id: "run-a".to_string(),
                current_visit: String::new(),
                current_stage: "build".to_string(),
                iteration: 1,
                status: AgentStatus::Active,
                spawned_children_ids: vec![],
                pending_wait: None,
                accepts_messages: true,
            },
            InferenceResult {
                attempt_id: String::new(),
                response: String::new(),
                tool_calls: calls
                    .iter()
                    .map(|c| ToolCall {
                        tool_id: c.id.clone(),
                        name: c.name.clone(),
                        arguments: c.arguments.clone(),
                        thought_signature: None,
                    })
                    .collect(),
                tokens_used: 0,
                cut_off_at: None,
                reasoning: None,
                parts: Vec::new(),
            },
            ContextWindow::new(1000),
            PendingBatch::new(calls, Vec::new(), HashMap::new(), Vec::new(), Vec::new()),
            ToolGrants::default(),
            WriteLedger { written: 100 },
        ))
        .id()
}

fn run(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(
        (
            crate::approval_prompt::collect_approvals,
            dispatch_lane_batches,
        )
            .chain(),
    );
    s.run(world);
}

/// What the judge was handed to run, by name and decision.
fn ran(judge: &Judge) -> Vec<(String, Decision, u64)> {
    judge.ran.lock().unwrap().clone()
}

/// Every call is decided in order and the batch goes out with each decision;
/// the run is charged as it goes, and the next call is judged against the
/// total the calls before it left.
#[test]
fn a_batch_is_decided_in_order_and_sent_with_its_decisions() {
    let judge = Arc::new(Judge::default());
    let (mut world, mut jobs) = world_with(judge.clone());
    let e = agent(&mut world, &["ok", "no", "q", "ok"]);
    run(&mut world);
    assert!(jobs.try_recv().is_ok(), "the batch went to the lane");
    assert_eq!(
        ran(&judge),
        vec![
            ("ok".to_string(), Decision::Run, 110),
            ("no".to_string(), Decision::Refuse("nope".to_string()), 110),
            ("q".to_string(), Decision::Interact { attended: true }, 110),
            ("ok".to_string(), Decision::Run, 110),
        ]
    );
    assert_eq!(
        *judge.seen_written.lock().unwrap(),
        vec![100, 105, 105, 105]
    );
    assert_eq!(world.get::<WriteLedger>(e).unwrap().written, 110);
    assert!(world.get::<AwaitingTools>(e).is_some());
    assert!(world.get::<PendingBatch>(e).is_none());
    assert!(world.get::<crate::components::BatchExecutions>(e).is_some());
}

/// With no one to ask, a call that needs approval is refused as a prompt
/// nobody answered, and the rest of the batch goes on.
#[test]
fn with_no_one_to_ask_an_approval_is_refused_unanswered() {
    let judge = Arc::new(Judge::default());
    let (mut world, _jobs) = world_with(judge.clone());
    agent(&mut world, &["ask", "ok"]);
    run(&mut world);
    let ran = ran(&judge);
    assert_eq!(ran.len(), 2);
    assert_eq!(
        ran[0].1,
        Decision::Refuse(crate::approval_prompt::unanswered_approval_result(
            "ask", None
        ))
    );
    assert_eq!(ran[1].1, Decision::Run);
}

/// Wait for the hub to have an open request, and return it.
async fn open_request(hub: &InteractionHub) -> leviath_core::interaction::InteractionRequest {
    for _ in 0..200 {
        if let Some((_, req)) = hub.pending().into_iter().next() {
            return req;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    panic!("no approval prompt was asked");
}

/// Pass the systems until the agent's approval is applied.
async fn until_answered(world: &mut World, e: Entity) {
    for _ in 0..200 {
        run(world);
        if world.get::<AwaitingApproval>(e).is_none() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    panic!("the answer never landed");
}

/// A call that needs approval holds the batch in the world, asked on the
/// hub. Approved for the run, the grant is remembered on the run and covers
/// the same call later in the batch, and the approval's charge is spent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approval_holds_the_batch_and_its_grant_covers_what_follows() {
    let judge = Arc::new(Judge::default());
    let (mut world, mut jobs) = world_with(judge.clone());
    let hub = with_prompts(&mut world);
    let e = agent(&mut world, &["ok", "ask", "ask"]);
    run(&mut world);
    assert!(jobs.try_recv().is_err(), "held, not sent");
    let held = world.get::<AwaitingApproval>(e).expect("held").clone();
    assert_eq!(held.call_id, "c1");
    assert_eq!(
        crate::state::inspect::inspect(&world, e).map(|s| s.phase),
        Some(crate::state::PipelinePhase::AwaitingTools),
        "a held batch is a batch in hand"
    );
    let req = open_request(&hub).await;
    assert_eq!(req.stage_name, "build");
    assert_eq!(req.tool_name.as_deref(), Some("ask"));
    hub.answer(InteractionResponse::approval(
        &req.id,
        true,
        ApprovalScope::Run,
    ));
    until_answered(&mut world, e).await;
    assert!(world.get::<ToolGrants>(e).unwrap().granted("k"));
    assert!(jobs.try_recv().is_ok(), "sent once decided");
    let ran = ran(&judge);
    assert_eq!(
        ran.iter().map(|r| r.1.clone()).collect::<Vec<_>>(),
        vec![Decision::Run, Decision::Run, Decision::Run],
        "the second `ask` was covered by the grant, not asked again"
    );
    assert_eq!(world.get::<WriteLedger>(e).unwrap().written, 112);
    assert!(hub.pending().is_empty());
}

/// A declined approval becomes the call's result, with the person's words.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declined_approval_is_the_calls_result() {
    let judge = Arc::new(Judge::default());
    let (mut world, _jobs) = world_with(judge.clone());
    let hub = with_prompts(&mut world);
    let e = agent(&mut world, &["ask"]);
    run(&mut world);
    let req = open_request(&hub).await;
    hub.answer(InteractionResponse::deny_with_feedback(
        &req.id,
        "read it first",
    ));
    until_answered(&mut world, e).await;
    assert_eq!(
        ran(&judge)[0].1,
        Decision::Refuse(
            "[denied] User declined tool call 'ask'. Feedback: read it first".to_string()
        )
    );
    assert!(!world.get::<ToolGrants>(e).unwrap().granted("k"));
    assert_eq!(
        world.get::<WriteLedger>(e).unwrap().written,
        100,
        "nothing charged"
    );
}

/// A prompt closed without an answer refuses the call without blaming a
/// person, and an approval for an agent with no grants or ledger still lands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unanswered_approval_refuses_and_a_bare_agent_still_settles() {
    let judge = Arc::new(Judge::default());
    let (mut world, _jobs) = world_with(judge.clone());
    let hub = with_prompts(&mut world);
    let e = agent(&mut world, &["ask"]);
    run(&mut world);
    let req = open_request(&hub).await;
    hub.cancel(&req.id);
    until_answered(&mut world, e).await;
    assert_eq!(
        ran(&judge)[0].1,
        Decision::Refuse(crate::approval_prompt::unanswered_approval_result(
            "ask", None
        ))
    );

    let bare = agent(&mut world, &["ask"]);
    world
        .entity_mut(bare)
        .remove::<ToolGrants>()
        .remove::<WriteLedger>();
    run(&mut world);
    let req = open_request(&hub).await;
    hub.answer(InteractionResponse::approval(
        &req.id,
        true,
        ApprovalScope::Stage,
    ));
    until_answered(&mut world, bare).await;
    assert_eq!(ran(&judge).last().unwrap().1, Decision::Run);
}

/// An answer for an agent that is no longer waiting on it is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_for_a_gone_agent_is_dropped() {
    let judge = Arc::new(Judge::default());
    let (mut world, _jobs) = world_with(judge.clone());
    let hub = with_prompts(&mut world);
    let e = agent(&mut world, &["ask"]);
    run(&mut world);
    let req = open_request(&hub).await;
    world.despawn(e);
    hub.answer(InteractionResponse::approval(
        &req.id,
        true,
        ApprovalScope::Run,
    ));
    for _ in 0..20 {
        run(&mut world);
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(ran(&judge).is_empty());
}

/// A service that keeps no write total of its own.
struct Plain;

impl ToolService for Plain {
    fn exec_for(
        &self,
        _entity: Entity,
        _calls: Vec<leviath_providers::ToolCall>,
        _progress: ToolProgress,
    ) -> BoxedToolExec {
        Box::new(|| Box::pin(async { Vec::new() }))
    }
}

/// With a service that keeps no write total, a landed batch leaves the
/// ledger at what the world charged.
#[tokio::test]
async fn a_service_with_no_write_total_leaves_the_ledger_alone() {
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(Plain)));
    let e = world.spawn((WriteLedger { written: 9 }, WritesOut)).id();
    let mut s = Schedule::default();
    s.add_systems(settle_write_ledgers);
    s.run(&mut world);
    assert_eq!(world.get::<WriteLedger>(e).unwrap().written, 9);
    let exec = Plain.exec_for(Entity::PLACEHOLDER, Vec::new(), noop_progress());
    assert!(exec().await.is_empty());
}

/// Once the batch lands, its executors' write total is the run's.
#[test]
fn a_landed_batch_settles_the_runs_write_ledger() {
    let judge = Arc::new(Judge::default());
    let (mut world, _jobs) = world_with(judge);
    let e = world.spawn((WriteLedger { written: 1 }, WritesOut)).id();
    let bare = world.spawn(WritesOut).id();
    let out = world
        .spawn((WriteLedger { written: 1 }, WritesOut, AwaitingTools))
        .id();
    let mut s = Schedule::default();
    s.add_systems(settle_write_ledgers);
    s.run(&mut world);
    assert_eq!(world.get::<WriteLedger>(e).unwrap().written, 42);
    assert!(world.get::<WritesOut>(bare).is_none());
    assert_eq!(
        world.get::<WriteLedger>(out).unwrap().written,
        1,
        "a batch still out is not read yet"
    );
}

/// A stage grant ends when the run enters a different stage, and survives
/// re-entering the same one; a run grant survives both. `Once` remembers
/// nothing.
#[test]
fn a_stage_grant_ends_with_its_stage() {
    let mut grants = ToolGrants::default();
    grants.enter_stage(0);
    grants.grant(Some(ApprovalScope::Stage), &["s".to_string()]);
    grants.grant(Some(ApprovalScope::Run), &["r".to_string()]);
    grants.grant(Some(ApprovalScope::Once), &["o".to_string()]);
    grants.grant(None, &["n".to_string()]);
    grants.enter_stage(0);
    assert!(grants.granted("s") && grants.granted("r"));
    assert!(!grants.granted("o") && !grants.granted("n"));
    grants.enter_stage(1);
    assert!(!grants.granted("s"));
    assert!(grants.granted("r"));
}

/// The stage sync system moves a run's grants into the stage it entered.
#[test]
fn entering_a_stage_moves_the_grants_with_it() {
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(Judge::default())));
    let mut grants = ToolGrants::default();
    grants.enter_stage(0);
    grants.grant(Some(ApprovalScope::Stage), &["s".to_string()]);
    let e = world
        .spawn((
            grants,
            StageJustEntered {
                index: 1,
                name: "next".to_string(),
            },
        ))
        .id();
    let bare = world
        .spawn(StageJustEntered {
            index: 1,
            name: "next".to_string(),
        })
        .id();
    let mut s = Schedule::default();
    s.add_systems(sync_tool_stages);
    s.run(&mut world);
    assert!(!world.get::<ToolGrants>(e).unwrap().granted("s"));
    assert!(world.get::<StageJustEntered>(bare).is_none());
}

/// A service whose executor blows up.
struct Explodes;

impl ToolService for Explodes {
    fn exec_for(
        &self,
        _entity: Entity,
        _calls: Vec<leviath_providers::ToolCall>,
        _progress: ToolProgress,
    ) -> BoxedToolExec {
        Box::new(|| Box::pin(async { panic!("the executor blew up") }))
    }
}

/// A batch whose executor panics still reports, each of its calls failed,
/// so its run is not left waiting on the lane for good.
#[tokio::test]
async fn a_batch_whose_executor_panics_reports_each_call_failed() {
    let mut world = World::new();
    let (jobs, mut rx) = unbounded_channel();
    world.insert_resource(ToolServiceRes(Arc::new(Explodes)));
    world.insert_resource(ToolStage::detached(jobs));
    agent(&mut world, &["ok", "ok"]);
    run(&mut world);
    let job = rx.try_recv().expect("the batch went to the lane");
    let silent = crate::test_support::SilentPanics::install();
    let results = tokio::spawn((job.exec)()).await;
    drop(silent);
    let results = results.expect("the batch reported instead of panicking");
    assert_eq!(results.len(), 2);
    assert_eq!(results[1].0, "c1");
    assert!(results[0].1.as_str().starts_with("[error]"));
    assert!(results[0].1.as_str().contains("the executor blew up"));
}

/// The judge's exec_for is only there because the trait asks for it.
#[tokio::test]
async fn the_judges_plain_exec_runs_nothing() {
    let judge = Judge::default();
    let exec = judge.exec_for(Entity::PLACEHOLDER, Vec::new(), noop_progress());
    assert!(exec().await.is_empty());
    let exec = judge.exec_decided(Entity::PLACEHOLDER, Vec::new(), 0, noop_progress());
    assert!(exec().await.is_empty());
}
