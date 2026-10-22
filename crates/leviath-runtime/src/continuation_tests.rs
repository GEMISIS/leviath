//! Continuing a finished run: what reopening it decides off the world, what
//! placing it does to its regions, its stage and its inbox, and what a fork
//! writes.

use std::collections::BTreeMap;
use std::path::Path;

use bevy_ecs::system::RunSystemOnce;

use super::*;
use crate::components::{ContextWindow, MessageInbox};
use crate::runfile::reader_tests::entry;
use crate::spec::graph::OnContinue;
use crate::spec::issues::IssueCode;
use crate::state::{EntryKind, FinalOutputState, RunStatus};

/// A graph that continues at `revise`, summarizes on the stage's own model,
/// and declares one region of each policy.
const GRAPH: &str = r#"
title = "t"
continue_at = "revise"
[compaction]
[[stages]]
name = "plan"
system_prompt = "Plan it."
[[stages]]
name = "revise"
system_prompt = "Revise it."
[[edges]]
name = "next"
from = "plan"
to = "revise"
[layout]
total_budget_tokens = 0
regions = [
  { name = "refs", kind = "pinned", budget = 1000, seed = { literal = "sheet" }, on_continue = "reseed" },
  { name = "notes", kind = "pinned", budget = 1000, on_continue = "compact" },
  { name = "scratch", kind = "clearable", budget = 1000 },
  { name = "decisions", kind = "pinned", budget = 1000 },
]
"#;

fn graph() -> RunGraph {
    crate::test_graph::graph_of(GRAPH)
}

fn spec() -> RunSpec {
    crate::test_graph::spec_named("t", graph())
}

/// What the run held when it finished: a line in every region, and `status`.
fn finished(status: RunStatus) -> RunState {
    let mut state = crate::insert::initial_state(&spec());
    for region in &mut state.context.regions {
        if region.name.as_str() != crate::spec::graph::STAGE_INSTRUCTIONS_REGION {
            region.entries.push(entry(
                &format!("{} held this", region.name),
                EntryKind::Text,
            ));
        }
    }
    state.status = status;
    state.phase = crate::state::PipelinePhase::Done;
    state
}

fn resumable(state: RunState) -> Resumable {
    Resumable {
        spec: std::sync::Arc::new(spec()),
        state,
        code: Default::default(),
        answer: Some("the old answer".into()),
        asked: 0,
        audit: Vec::new(),
        segment: 1,
    }
}

fn message(text: &str) -> ContinueMessage {
    ContinueMessage {
        text: text.to_string(),
        ..Default::default()
    }
}

/// A run that handed back `answer`, written beside it in `dir`.
fn answered(dir: &Path, answer: &str) -> RunState {
    let mut state = finished(RunStatus::Complete);
    let name = leviath_core::FINAL_OUTPUT_FILE;
    std::fs::write(dir.join(name), answer).unwrap();
    state.files.final_output = Some(FileRef::whole(name, answer.as_bytes()));
    state.final_output = Some(FinalOutputState {
        bytes: answer.len() as u64,
        format: None,
        stage: crate::spec::names::StageName::new("revise").unwrap(),
        submitted_at: 5,
        truncated: false,
        artifacts: Vec::new(),
    });
    state.totals.spend.priced_usd = 1.5;
    state
}

fn texts(window: &ContextWindow, region: &str) -> Vec<String> {
    window
        .get_region(region)
        .map(|r| {
            r.content
                .iter()
                .map(|e| e.content.as_str().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_finished_run_can_be_continued_and_nothing_else_can() {
    let spec = spec();
    for status in [
        RunStatus::Complete,
        RunStatus::Error("boom".into()),
        RunStatus::Cancelled,
    ] {
        assert!(check(&spec, &finished(status)).is_ok());
    }
    for (status, word) in [
        (RunStatus::Idle, "starting"),
        (RunStatus::Active, "running"),
        (RunStatus::Waiting, "waiting"),
        (RunStatus::Paused, "paused"),
    ] {
        let issues = check(&spec, &finished(status)).unwrap_err();
        assert_eq!(issues.0[0].code, IssueCode::Conflict);
        assert!(issues.to_string().contains(word), "{issues}");
    }
}

#[test]
fn a_sub_agent_and_a_run_that_never_resumes_are_refused() {
    let mut child = spec();
    child.placement.parent = Some(crate::spec::names::RunId::new("p-1").unwrap());
    let issues = check(&child, &finished(RunStatus::Complete)).unwrap_err();
    assert_eq!(issues.0[0].code, IssueCode::NotAllowed);
    assert!(issues.to_string().contains("sub-agent of run 'p-1'"));

    let mut recorded = spec();
    recorded.origin = crate::spec::run_spec::SpecOrigin::Recorded {
        name: crate::spec::names::BlueprintName::new("old").unwrap(),
        manifest: String::new(),
        why: "its blueprint is gone".into(),
    };
    let issues = check(&recorded, &finished(RunStatus::Complete)).unwrap_err();
    assert!(
        issues.to_string().contains("its blueprint is gone"),
        "{issues}"
    );
}

#[test]
fn reopening_keeps_the_answer_of_the_segment_that_ended() {
    let dir = tempfile::tempdir().unwrap();
    let state = answered(dir.path(), "the old answer");
    let prepared = prepare(
        dir.path(),
        resumable(state.clone()),
        message("make it red"),
        None,
    )
    .unwrap();
    assert_eq!(prepared.segment(), 2);
    let previous = prepared.continued.previous.clone().unwrap();
    assert_eq!(previous.status, RunStatus::Complete);
    assert_eq!(previous.spend.priced_usd, 1.5);
    let kept = previous.file.unwrap();
    assert_eq!(kept.path, "segments/1/final_output");
    assert_eq!(kept.read(dir.path()).unwrap(), b"the old answer");
    // Reopened: going again, owing nothing, its answer gone with the
    // segment, its spend and context carried.
    let run = &prepared.run;
    assert_eq!(run.state.status, RunStatus::Active);
    assert_eq!(run.state.phase, crate::state::PipelinePhase::ReadyToInfer);
    assert!(run.state.final_output.is_none());
    assert!(run.state.files.final_output.is_none());
    assert!(run.state.visits.is_empty());
    assert_eq!(run.state.totals, state.totals);
    assert_eq!(run.state.context, state.context);
    assert_eq!(run.answer, None);
    assert_eq!(run.segment, 2);
}

#[test]
fn a_run_with_no_answer_keeps_none() {
    let dir = tempfile::tempdir().unwrap();
    let prepared = prepare(
        dir.path(),
        resumable(finished(RunStatus::Cancelled)),
        message("again"),
        None,
    )
    .unwrap();
    let previous = prepared.continued.previous.unwrap();
    assert_eq!(previous.status, RunStatus::Cancelled);
    assert!(previous.file.is_none());
}

#[test]
fn an_answer_that_cannot_be_kept_refuses_the_continuation() {
    // The file the run names is gone.
    let dir = tempfile::tempdir().unwrap();
    let state = answered(dir.path(), "x");
    std::fs::remove_file(dir.path().join(leviath_core::FINAL_OUTPUT_FILE)).unwrap();
    let issues = prepare(dir.path(), resumable(state), message("m"), None).unwrap_err();
    assert_eq!(issues.0[0].code, IssueCode::Unavailable);
    // Where it would be kept is taken by a file.
    let dir = tempfile::tempdir().unwrap();
    let state = answered(dir.path(), "x");
    std::fs::write(dir.path().join("segments"), b"in the way").unwrap();
    let issues = prepare(dir.path(), resumable(state), message("m"), None).unwrap_err();
    assert!(issues.to_string().contains("cannot be kept"), "{issues}");
}

#[test]
fn a_fork_starts_its_first_segment_with_none_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let from = crate::spec::names::RunId::new("src-1").unwrap();
    let prepared = prepare(
        dir.path(),
        resumable(finished(RunStatus::Complete)),
        message("facing left"),
        Some(from.clone()),
    )
    .unwrap();
    assert_eq!(prepared.segment(), 1);
    assert_eq!(prepared.continued.forked_from, Some(from));
    assert!(prepared.continued.previous.is_none());
}

#[test]
fn a_message_that_says_nothing_or_names_no_region_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let run = || resumable(finished(RunStatus::Complete));
    let issues = prepare(dir.path(), run(), message("  "), None).unwrap_err();
    assert_eq!(issues.0[0].code, IssueCode::Missing);
    assert_eq!(issues.0[0].path.to_string(), "text");

    let mut nowhere = message("hi");
    nowhere.region = Some("nowhere".into());
    let issues = prepare(dir.path(), run(), nowhere, None).unwrap_err();
    assert_eq!(issues.0[0].code, IssueCode::Unknown);
    assert!(issues.0[0].known.contains(&"decisions".to_string()));
    assert!(issues.0[0].known.contains(&"conversation".to_string()));

    // A region the window holds, and a file with no words, both go.
    let mut aimed = message("");
    aimed.region = Some("decisions".into());
    aimed.parts = vec![leviath_core::mime::InboundPart::from_bytes(
        "a.png",
        vec![1],
    )];
    assert!(prepare(dir.path(), run(), aimed, None).is_ok());
}

#[test]
fn the_regions_that_reseed_are_named_once_with_their_seeds() {
    let mut g = graph();
    // A stage that declares `refs` too does not name it twice.
    let mut layout = g.layout.clone();
    layout.regions.truncate(1);
    g.stages[1].layout = Some(layout);
    let reseeds = reseeding(&g);
    assert_eq!(reseeds.len(), 1);
    assert_eq!(reseeds[0].0.as_str(), "refs");
    assert!(matches!(&reseeds[0].1, Seed::Literal(text) if text == "sheet"));
}

#[tokio::test]
async fn reseeding_runs_each_seed_and_skips_a_command_the_run_may_not_run() {
    use crate::test_graph::spawning::{StageAnswers, TestEnv, limits};
    let env = TestEnv::new(
        graph(),
        "r",
        StageAnswers::Given(BTreeMap::new()),
        limits(Default::default()),
    );
    let run = resumable(finished(RunStatus::Complete));
    let seeded = reseed(&run, &env).await;
    assert!(seeded["refs"].text.contains("sheet"));

    // A blueprint whose file is known seeds against its directory.
    let mut placed = spec();
    placed.origin = crate::spec::run_spec::SpecOrigin::Blueprint {
        blueprint: crate::spec::names::BlueprintRef::parse("t").unwrap(),
        version: "1".into(),
        manifest: "/agents/t/agent.toml".into(),
    };
    let mut run = resumable(finished(RunStatus::Complete));
    run.spec = std::sync::Arc::new(placed);
    assert!(reseed(&run, &env).await.contains_key("refs"));

    let mut commanded = graph();
    commanded.layout.regions[0].seed = Some(Seed::Command("date".into()));
    let mut spec = crate::test_graph::spec_named("t", commanded);
    spec.launch.seed_commands = false;
    let mut run = resumable(finished(RunStatus::Complete));
    run.spec = std::sync::Arc::new(spec);
    assert!(reseed(&run, &env).await.is_empty());
}

/// Placing a continuation carries each region over by its policy, enters
/// the stage the graph continues at, leaves the message waiting and asks
/// for the summaries; applying it a second time does nothing.
#[tokio::test]
async fn a_placed_continuation_carries_the_regions_and_enters_its_stage() {
    let dir = tempfile::tempdir().unwrap();
    let mut prepared = prepare(
        dir.path(),
        resumable(finished(RunStatus::Complete)),
        message("make the cape red"),
        None,
    )
    .unwrap();
    prepared.reseeded.insert(
        "refs".into(),
        SeededContent {
            text: "the sheet, again".into(),
            parts: Vec::new(),
        },
    );
    let mut world = World::new();
    let entity = place(&mut world, prepared, crate::spec::env::Bindings::new());
    let applied = apply(&mut world, entity).expect("a continuation was placed");
    assert_eq!(applied.segment, 2);
    assert_eq!(applied.stage, "revise");
    assert_eq!(applied.forked_from, None);
    assert_eq!(applied.landed.await.unwrap(), Appended::NoJournal);

    let window = world.get::<ContextWindow>(entity).unwrap();
    assert_eq!(texts(window, "decisions"), vec!["decisions held this"]);
    assert_eq!(
        texts(window, "conversation"),
        vec!["conversation held this"]
    );
    assert!(texts(window, "scratch").is_empty());
    assert!(texts(window, "tool_results").is_empty());
    assert_eq!(texts(window, "refs"), vec!["the sheet, again"]);
    let instructions = texts(window, crate::spec::graph::STAGE_INSTRUCTIONS_REGION);
    assert!(instructions[0].contains("Revise it."), "{instructions:?}");
    let state = world.get::<crate::components::AgentState>(entity).unwrap();
    assert_eq!(state.current_stage, "revise");
    let inbox = world.get::<MessageInbox>(entity).unwrap();
    assert_eq!(inbox.messages[0].from, FROM_CONTINUATION);
    assert_eq!(inbox.messages[0].content, "make the cape red");
    let pending = world.get::<PendingEdgeCompact>(entity).unwrap();
    assert_eq!(pending.0, vec!["notes".to_string()]);
    assert!(apply(&mut world, entity).is_none());
}

/// Nothing to summarize asks for no summary.
#[test]
fn a_continuation_with_nothing_to_summarize_asks_for_none() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = finished(RunStatus::Complete);
    for region in &mut state.context.regions {
        if region.name.as_str() == "notes" {
            region.entries.clear();
        }
    }
    let prepared = prepare(dir.path(), resumable(state), message("m"), None).unwrap();
    let mut world = World::new();
    let entity = place(&mut world, prepared, crate::spec::env::Bindings::new());
    apply(&mut world, entity).unwrap();
    assert!(world.get::<PendingEdgeCompact>(entity).is_none());
}

/// A run placed with its continuation's message still waiting (a restart
/// came between) asks again for the summaries the continuation wanted.
#[test]
fn a_run_back_with_its_message_waiting_asks_for_its_summaries_again() {
    let spec = std::sync::Arc::new(spec());
    let waiting = |from: &str, notes: bool| {
        let mut state = finished(RunStatus::Active);
        state.inbox.push(crate::state::MessageState {
            from: from.to_string(),
            text: "m".into(),
            region: None,
        });
        if !notes {
            for region in &mut state.context.regions {
                region.entries.clear();
            }
        }
        let mut world = World::new();
        let e = crate::insert::insert(
            &mut world,
            spec.clone(),
            crate::spec::env::Bindings::new(),
            &state,
        );
        world.get::<PendingEdgeCompact>(e).map(|p| p.0.clone())
    };
    assert_eq!(waiting(FROM_CONTINUATION, true), Some(vec!["notes".into()]));
    assert_eq!(waiting(crate::components::FROM_PERSON, true), None);
    assert_eq!(waiting(FROM_CONTINUATION, false), None);
}

/// The message is delivered once no summary is pending, whatever the stage
/// says about messages, and only then.
#[test]
fn the_waiting_message_lands_once_the_summaries_are_in() {
    let dir = tempfile::tempdir().unwrap();
    let prepared = prepare(
        dir.path(),
        resumable(finished(RunStatus::Complete)),
        message("now this"),
        None,
    )
    .unwrap();
    let mut world = World::new();
    let entity = place(&mut world, prepared, crate::spec::env::Bindings::new());
    apply(&mut world, entity).unwrap();
    world
        .run_system_once(crate::pipeline::deliver_continuation)
        .unwrap();
    assert!(
        !texts(world.get::<ContextWindow>(entity).unwrap(), "conversation")
            .contains(&"now this".to_string())
    );
    world.entity_mut(entity).remove::<PendingEdgeCompact>();
    world
        .run_system_once(crate::pipeline::deliver_continuation)
        .unwrap();
    let window = world.get::<ContextWindow>(entity).unwrap();
    assert!(texts(window, "conversation").contains(&"now this".to_string()));
    assert!(
        world
            .get::<MessageInbox>(entity)
            .unwrap()
            .messages
            .is_empty()
    );
    // Nothing waiting: nothing happens.
    world
        .run_system_once(crate::pipeline::deliver_continuation)
        .unwrap();
}

/// A message from a person waits for a stage that takes messages; the
/// continuation's goes on its own.
#[test]
fn an_inbox_hands_over_a_continuation_apart_from_the_rest() {
    let mut inbox = MessageInbox::new();
    for from in [crate::components::FROM_PERSON, FROM_CONTINUATION] {
        inbox.push(crate::components::AgentMessage {
            agent_id: "r".into(),
            from: from.into(),
            content: from.into(),
            target_region: None,
            parts: Vec::new(),
        });
    }
    let ready = inbox.drain_all();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].from, crate::components::FROM_PERSON);
    let continuing = inbox.take_continuation();
    assert_eq!(continuing[0].from, FROM_CONTINUATION);
    assert!(inbox.messages.is_empty());
}

#[tokio::test]
async fn the_lane_barrier_is_answered_behind_what_the_lane_holds() {
    assert_eq!(
        lane_barrier(&World::new(), "r").await.unwrap(),
        Appended::NoJournal
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(crate::pipeline::PersistenceStage(tx));
    let landed = lane_barrier(&world, "r");
    let Some(crate::persistence_bridge::PersistMsg::Step(step)) = rx.recv().await else {
        panic!("a barrier is a step");
    };
    assert_eq!(step.run_id, "r");
    assert!(step.events.is_empty() && step.now.is_none());
    for ack in step.acks {
        let _ = ack.send(Appended::Landed { position: 3 });
    }
    assert_eq!(landed.await.unwrap(), Appended::Landed { position: 3 });
}

/// A fork writes a new run whose first state is the finished run's own,
/// with nothing it spent or owned carried over, and its stored parts copied;
/// the finished run's files are only read.
#[test]
fn a_fork_writes_a_new_run_and_leaves_its_source_alone() {
    let runs = tempfile::tempdir().unwrap();
    let source_dir = runs.path().join("src-1");
    std::fs::create_dir_all(&source_dir).unwrap();
    let mut state = answered(&source_dir, "a");
    let blob = crate::spec::names::Digest::of(b"img");
    let blob_path = crate::runfile::blob_path(&source_dir, &blob);
    std::fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
    std::fs::write(&blob_path, b"img").unwrap();
    state.blobs.push(crate::state::BlobFile {
        digest: blob.clone(),
        mime_type: "image/png".into(),
        size: 3,
        name: None,
        region: None,
        tool: None,
    });
    state
        .children
        .push(crate::spec::names::RunId::new("c-1").unwrap());
    let mut source = resumable(state);
    source.code = crate::runfile::reader_tests::code();
    let forked = fork(
        &source,
        &source_dir,
        crate::spec::names::RunId::new("new-1").unwrap(),
        runs.path(),
    )
    .unwrap();
    assert_eq!(forked.spec.run_id.as_str(), "new-1");
    assert_eq!(
        forked.spec.delivery.metadata[FORKED_FROM_LABEL],
        source.spec.run_id.to_string()
    );
    assert_eq!(forked.state.totals, Default::default());
    assert!(forked.state.children.is_empty());
    assert!(forked.state.final_output.is_none());
    assert_eq!(forked.state.context, source.state.context);
    let new_dir = runs.path().join("new-1");
    assert!(crate::runfile::blob_path(&new_dir, &blob).is_file());
    let read = crate::restore::read_for_resume(&new_dir).unwrap().unwrap();
    assert_eq!(read.state.status, RunStatus::Complete);
    assert_eq!(read.spec.run_id.as_str(), "new-1");
    // The source is as it was.
    assert_eq!(
        std::fs::read(source_dir.join(leviath_core::FINAL_OUTPUT_FILE)).unwrap(),
        b"a"
    );
    assert!(!source_dir.join(leviath_core::files::RUN_FILE).exists());
}

#[test]
fn a_fork_that_cannot_be_written_or_has_no_finished_source_is_refused() {
    let runs = tempfile::tempdir().unwrap();
    let id = |s: &str| crate::spec::names::RunId::new(s).unwrap();
    // A source still going.
    let going = resumable(finished(RunStatus::Active));
    let issues = fork(&going, runs.path(), id("n-1"), runs.path()).unwrap_err();
    assert_eq!(issues.0[0].code, IssueCode::Conflict);
    // A stored part the source names and does not have.
    let mut state = finished(RunStatus::Complete);
    state.blobs.push(crate::state::BlobFile {
        digest: crate::spec::names::Digest::of(b"gone"),
        mime_type: "image/png".into(),
        size: 4,
        name: None,
        region: None,
        tool: None,
    });
    let issues = fork(&resumable(state), runs.path(), id("n-2"), runs.path()).unwrap_err();
    assert!(issues.to_string().contains("stored part"), "{issues}");
    // A runs directory that is a file.
    let file = runs.path().join("a-file");
    std::fs::write(&file, b"").unwrap();
    let source = resumable(finished(RunStatus::Complete));
    assert!(fork(&source, runs.path(), id("n-3"), &file).is_err());
    // A run file whose place is taken by a directory.
    std::fs::create_dir_all(runs.path().join("n-4").join(leviath_core::files::RUN_FILE)).unwrap();
    let issues = fork(&source, runs.path(), id("n-4"), runs.path()).unwrap_err();
    assert!(issues.to_string().contains("cannot be written"), "{issues}");
}

#[test]
fn every_region_kind_follows_its_default_and_a_declared_one_wins() {
    use leviath_core::RegionKind as K;
    assert_eq!(OnContinue::for_kind(&K::Temporary), OnContinue::Clear);
    assert_eq!(OnContinue::for_kind(&K::Clearable), OnContinue::Clear);
    assert_eq!(OnContinue::for_kind(&K::Pinned), OnContinue::Keep);
    let g = graph();
    assert_eq!(g.continue_policy("refs"), Some(OnContinue::Reseed));
    assert_eq!(g.continue_policy("scratch"), Some(OnContinue::Clear));
    assert_eq!(g.continue_policy("decisions"), Some(OnContinue::Keep));
    assert_eq!(g.continue_policy("tool_results"), None);
    assert_eq!(g.continue_stage().unwrap().name.as_str(), "revise");
    let labels: Vec<&str> = [
        OnContinue::Keep,
        OnContinue::Clear,
        OnContinue::Compact,
        OnContinue::Reseed,
    ]
    .into_iter()
    .map(OnContinue::label)
    .collect();
    assert_eq!(labels, ["keep", "clear", "compact", "reseed"]);
    // With no `continue_at`, the entry stage.
    let mut plain = graph();
    plain.continue_at = None;
    assert_eq!(plain.continue_stage().unwrap().name.as_str(), "plan");
}
