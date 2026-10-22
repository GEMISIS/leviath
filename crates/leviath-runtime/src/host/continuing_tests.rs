//! The host's half of a continuation: one at a time per run, a finished run
//! still in the world continued once it is unloaded, the answer sent once
//! the step that reopened the run has landed, and every way it can be
//! refused.

use std::path::PathBuf;

use super::*;
use crate::restore::Resumable;
use crate::spec::issues::IssueCode;
use crate::state::RunStatus;
use tokio::runtime::Handle;

const RUN: &str = "t-1";

fn host() -> WorldHost {
    let world = PipelineWorld::new(
        crate::providers::ProviderRegistry::new(),
        Arc::new(super::super::tests::NoTools),
        crate::inference_pool::InferencePoolConfig::new(),
        1,
        None,
        Handle::current(),
    );
    WorldHost::new(world)
}

fn spec() -> Arc<crate::spec::run_spec::RunSpec> {
    let graph = crate::test_graph::graph_of(
        r#"
title = "t"
[[stages]]
name = "s"
[layout]
total_budget_tokens = 0
regions = [{ name = "notes", kind = "pinned", budget = 1000 }]
"#,
    );
    Arc::new(crate::test_graph::spec_named("t", graph))
}

/// The run as its file holds it, in `status`.
fn resumable(status: RunStatus) -> Resumable {
    let spec = spec();
    let mut state = crate::insert::initial_state(&spec);
    state.status = status;
    Resumable {
        spec,
        state,
        code: Default::default(),
        answer: None,
        asked: 0,
        audit: Vec::new(),
        segment: 1,
    }
}

/// A continuer that reopens a finished copy of the run, read from `dir`,
/// and places it under the id it was asked for, or a fork's id.
fn continuer(dir: PathBuf) -> Continuer {
    Box::new(move |job: ContinueJob| -> ContinueTask {
        let dir = dir.clone();
        Box::pin(async move {
            job.written.await;
            let forked_from = job.request.fork.then(|| spec().run_id.clone());
            let prepared = crate::continuation::prepare(
                &dir,
                resumable(RunStatus::Complete),
                job.request.message,
                forked_from,
            )?;
            let run_id = match job.request.fork {
                true => format!("{}-fork", job.run_id),
                false => job.run_id,
            };
            let place: PlaceContinued = Box::new(move |world: &mut PipelineWorld| {
                let entity = crate::continuation::place(
                    world.world_mut(),
                    prepared,
                    crate::spec::env::Bindings::new(),
                );
                Ok((run_id, world.own_agent(entity)))
            });
            Ok(place)
        })
    })
}

/// A continuer whose every job comes back as `answer`.
fn answering(
    answer: impl Fn() -> Result<PlaceContinued, SpawnIssues> + Send + Sync + 'static,
) -> Continuer {
    let answer = Arc::new(answer);
    Box::new(move |_job| -> ContinueTask {
        let answer = answer.clone();
        Box::pin(async move { answer() })
    })
}

fn request(text: &str, fork: bool) -> ContinueRequest {
    ContinueRequest {
        message: ContinueMessage {
            text: text.to_string(),
            ..Default::default()
        },
        fork,
    }
}

/// Ask `host` to continue `run_id`, returning where the answer will come.
fn ask(
    host: &mut WorldHost,
    run_id: &str,
    request: ContinueRequest,
) -> oneshot::Receiver<Result<Continued, SpawnIssues>> {
    let (reply, rx) = oneshot::channel();
    host.handle(ControlOp::Continue {
        run_id: run_id.to_string(),
        request,
        reply,
    });
    rx
}

/// Place the next continuation that lands, and drive the world until the
/// step that reopened the run is recorded.
async fn land(host: &mut WorldHost) {
    let landed = host
        .continued_rx
        .recv()
        .await
        .expect("a continuation lands");
    host.landed_continuation(landed);
    host.world.run_to_fixed_point();
}

/// The code of the first issue a refusal names.
fn code(refused: Result<Continued, SpawnIssues>) -> IssueCode {
    refused.expect_err("a refusal").0[0].code
}

#[tokio::test]
async fn a_finished_run_is_continued_and_says_so_once_its_step_lands() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = host();
    host.set_continuer(continuer(dir.path().to_path_buf()));
    let mut events = host.subscribe();
    let reply = ask(&mut host, RUN, request("again", false));
    land(&mut host).await;
    let continued = reply.await.unwrap().unwrap();
    assert_eq!(
        continued,
        Continued {
            run_id: RUN.into(),
            segment: 2,
            forked_from: None,
        }
    );
    assert!(host.live_entity(RUN).is_some());
    let said = std::iter::from_fn(|| events.try_recv().ok())
        .find(|e| matches!(e, WorldEvent::Continued { .. }))
        .expect("a continued frame");
    assert_eq!(said.run_id(), RUN);
}

#[tokio::test]
async fn a_fork_is_placed_under_its_own_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = host();
    host.set_continuer(continuer(dir.path().to_path_buf()));
    let reply = ask(&mut host, RUN, request("left", true));
    assert!(
        !host.continuing.contains_key(RUN),
        "a fork holds nothing back"
    );
    land(&mut host).await;
    let continued = reply.await.unwrap().unwrap();
    assert_eq!(continued.run_id, "t-1-fork");
    assert_eq!(continued.segment, 1);
    assert_eq!(continued.forked_from.as_deref(), Some(RUN));
}

/// A second continuation while the first is out is a conflict, and an op
/// naming the run waits for the first to land rather than paging in a copy.
#[tokio::test]
async fn one_continuation_of_a_run_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = host();
    host.set_continuer(continuer(dir.path().to_path_buf()));
    let first = ask(&mut host, RUN, request("one", false));
    let second = ask(&mut host, RUN, request("two", false));
    assert_eq!(code(second.await.unwrap()), IssueCode::Conflict);
    let (pause, paused) = oneshot::channel();
    host.handle(ControlOp::Pause {
        run_id: RUN.into(),
        reply: pause,
    });
    land(&mut host).await;
    assert!(first.await.unwrap().is_ok());
    assert!(
        paused.await.unwrap(),
        "the held pause reached the continued run"
    );
}

#[tokio::test]
async fn a_host_that_cannot_continue_refuses() {
    let mut host = host();
    let reply = ask(&mut host, RUN, request("x", false));
    assert_eq!(code(reply.await.unwrap()), IssueCode::Unavailable);
}

/// A run in the world that has not finished is a conflict; one that has
/// finished and was started by another run is not this caller's to continue.
#[tokio::test]
async fn a_live_run_that_is_going_or_is_a_sub_agent_is_refused() {
    let mut host = host();
    let entity = crate::restore::resume(
        host.world_mut().world_mut(),
        resumable(RunStatus::Active),
        crate::spec::env::Bindings::new(),
    );
    let agent = host.world.own_agent(entity);
    host.register(RUN, agent);
    let reply = ask(&mut host, RUN, request("x", false));
    assert_eq!(code(reply.await.unwrap()), IssueCode::Conflict);

    host.world_mut().set_status(agent, AgentStatus::Complete);
    host.world_mut()
        .world_mut()
        .entity_mut(entity)
        .insert(crate::components::ParentRef {
            parent_entity: entity,
            parent_agent_id: "p-1".into(),
            depth: 1,
        });
    let reply = ask(&mut host, RUN, request("x", false));
    let refused = reply.await.unwrap().unwrap_err();
    assert_eq!(refused.0[0].code, IssueCode::NotAllowed);
    assert!(refused.to_string().contains("p-1"), "{refused}");
}

/// A run that finished and is still in the world is continued once it has
/// been unloaded, when its last step is on its way to disk.
#[tokio::test]
async fn a_finished_run_still_loaded_is_continued_once_unloaded() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = host();
    host.set_continuer(continuer(dir.path().to_path_buf()));
    let entity = crate::restore::resume(
        host.world_mut().world_mut(),
        resumable(RunStatus::Active),
        crate::spec::env::Bindings::new(),
    );
    let agent = host.world.own_agent(entity);
    host.register(RUN, agent);
    host.emit_events();
    host.world_mut().set_status(agent, AgentStatus::Complete);
    let reply = ask(&mut host, RUN, request("after", false));
    assert!(host.after_unload.contains_key(RUN));
    // Seen finished, then unloaded on the next pass.
    host.emit_events();
    host.emit_events();
    assert!(!host.after_unload.contains_key(RUN));
    land(&mut host).await;
    assert_eq!(reply.await.unwrap().unwrap().segment, 2);
}

/// A message to a finished run continues it, and is answered the way a
/// message is.
#[tokio::test]
async fn a_message_to_a_finished_run_continues_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = host();
    host.set_continuer(continuer(dir.path().to_path_buf()));
    host.set_reloader(Box::new(|_run_id, _purpose| {
        Box::pin(async { Err(NotPlaced::Stopped(AgentStatus::Complete)) })
    }));
    let (reply, delivered) = oneshot::channel();
    host.handle(ControlOp::Message {
        agent_id: RUN.into(),
        content: "go on".into(),
        target_region: None,
        parts: Vec::new(),
        reply,
    });
    land(&mut host).await;
    assert_eq!(delivered.await.unwrap(), Ok(true));
    // A message that cannot continue the run says why, as text.
    host.set_continuer(answering(|| {
        Err(crate::continuation::refused(
            IssueCode::Conflict,
            crate::spec::issues::SpecPath::root(),
            "no",
        ))
    }));
    host.by_run_id.clear();
    let (reply, refused) = oneshot::channel();
    host.handle(ControlOp::Message {
        agent_id: "other".into(),
        content: "go on".into(),
        target_region: None,
        parts: Vec::new(),
        reply,
    });
    land(&mut host).await;
    assert!(refused.await.unwrap().unwrap_err().contains("no"));
}

/// A continuer that refuses, panics, or places something it cannot
/// continue: the caller hears each, and the run is free again.
#[tokio::test]
async fn every_way_a_continuation_fails_is_answered() {
    let mut host = host();
    host.set_continuer(answering(|| {
        Err(crate::continuation::refused(
            IssueCode::Missing,
            crate::spec::issues::SpecPath::root(),
            "no run",
        ))
    }));
    let reply = ask(&mut host, RUN, request("x", false));
    land(&mut host).await;
    assert_eq!(code(reply.await.unwrap()), IssueCode::Missing);
    assert!(!host.continuing.contains_key(RUN));

    host.set_continuer(Box::new(|_job| -> ContinueTask {
        Box::pin(async { panic!("the continuer fell over") })
    }));
    let reply = ask(&mut host, RUN, request("x", false));
    land(&mut host).await;
    let refused = reply.await.unwrap().unwrap_err();
    assert!(refused.to_string().contains("panicked"), "{refused}");

    host.set_continuer(answering(|| {
        let place: PlaceContinued = Box::new(|_world| panic!("placing fell over"));
        Ok(place)
    }));
    let reply = ask(&mut host, RUN, request("x", false));
    land(&mut host).await;
    let refused = reply.await.unwrap().unwrap_err();
    assert!(refused.to_string().contains("placing"), "{refused}");

    host.set_continuer(answering(|| {
        let place: PlaceContinued = Box::new(|world: &mut PipelineWorld| {
            let entity = crate::restore::resume(
                world.world_mut(),
                resumable(RunStatus::Active),
                crate::spec::env::Bindings::new(),
            );
            Ok((RUN.to_string(), world.own_agent(entity)))
        });
        Ok(place)
    }));
    let reply = ask(&mut host, "t-2", request("x", false));
    land(&mut host).await;
    let refused = reply.await.unwrap().unwrap_err();
    assert!(
        refused.to_string().contains("without its continuation"),
        "{refused}"
    );
}

/// A host serving its control channel places a continuation that lands off
/// the loop, answers it, and only stops once it has.
#[tokio::test]
async fn the_serve_loop_lands_a_continuation_before_it_stops() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = host();
    host.set_continuer(continuer(dir.path().to_path_buf()));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let (reply, answer) = oneshot::channel();
    tx.send(ControlOp::Continue {
        run_id: RUN.to_string(),
        request: request("again", false),
        reply,
    })
    .unwrap();
    drop(tx);
    host.serve(rx).await;
    assert_eq!(answer.await.unwrap().unwrap().segment, 2);
}

/// Reopening a run that has not finished is refused before anything is
/// written.
#[test]
fn reopening_a_run_still_going_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let refused = crate::continuation::prepare(
        dir.path(),
        resumable(RunStatus::Active),
        ContinueMessage {
            text: "x".into(),
            ..Default::default()
        },
        None,
    )
    .unwrap_err();
    assert_eq!(refused.0[0].code, IssueCode::Conflict);
}

/// The answer waits for the step that reopened the run: a step that could
/// not be written, or a daemon that stopped first, is a refusal.
#[tokio::test]
async fn a_continuation_whose_step_does_not_land_is_refused() {
    let host = host();
    for landed in [Some(Appended::Failed), None] {
        let (tx, rx) = oneshot::channel();
        match landed {
            Some(landed) => tx.send(landed).unwrap(),
            None => drop(tx),
        }
        let applied = crate::continuation::Applied {
            segment: 2,
            stage: "s".into(),
            forked_from: None,
            landed: rx,
        };
        let (reply, answer) = oneshot::channel();
        host.answer_when_written(RUN.into(), applied, ContinueReply::Continue(reply));
        assert_eq!(code(answer.await.unwrap()), IssueCode::Unavailable);
    }
}
