use super::*;
use crate::spec::graph::{EdgeCondition, InteractiveDef, StageMode};
use crate::test_graph as g;
use leviath_core::{Region, RegionKind};

/// A window with the conversation and the region an answer is mirrored into.
fn window() -> ContextWindow {
    let mut w = ContextWindow::new(10_000);
    for name in ["conversation", crate::output_tool::FINAL_OUTPUT_REGION] {
        w.add_region(Region::new(name.to_string(), RegionKind::Pinned, 2_000));
    }
    w
}

fn interactive() -> StageMode {
    StageMode::Interactive(InteractiveDef {
        idle_timeout_secs: None,
    })
}

fn submit(id: &str, content: &str) -> crate::components::ToolCall {
    crate::components::ToolCall {
        tool_id: id.to_string(),
        name: leviath_core::stage_tools::SUBMIT_OUTPUT_TOOL.to_string(),
        arguments: serde_json::json!({ "content": content }),
        thought_signature: None,
    }
}

/// What a submission is judged against, for a stage `s` with no shape asked
/// of its answer.
fn ctx() -> OutputContext<'static> {
    OutputContext {
        write_fence: None,
        spec: None,
        validators: None,
        stage: "s",
        stage_names: &[],
        workdir: None,
        sink: None,
        overwrite_artifacts: false,
    }
}

fn state() -> AgentState {
    AgentState {
        agent_id: "a".to_string(),
        current_visit: "v1".to_string(),
        current_stage: "s".to_string(),
        iteration: 0,
        status: AgentStatus::Active,
        spawned_children_ids: vec![],
        pending_wait: None,
        accepts_messages: true,
    }
}

/// A stage ends on its answer when no normal edge leaves it, or when it is the
/// stage a fan-out worker runs; never when it is interactive.
#[test]
fn only_a_stage_with_nowhere_to_go_ends_on_its_answer() {
    let layout = g::layout(vec![], 1_000);
    let stages = vec![
        g::stage("draft"),
        g::stage("final"),
        g::stage("recover"),
        g::stage("pick"),
        StageDef {
            mode: interactive(),
            ..g::stage("chat")
        },
    ];
    let mut graph = g::graph(stages, layout);
    graph.edges = vec![
        g::edge("draft", "final"),
        g::edge_when("final", "recover", EdgeCondition::Error),
        g::edge_when("recover", "draft", EdgeCondition::MaxIterations),
        g::edge_when("pick", "draft", EdgeCondition::LlmChoice),
    ];
    let mut spec = g::spec_named("t", graph);
    let ends: Vec<bool> = (0..5).map(|i| ends_on_submission(&spec, i)).collect();
    assert_eq!(ends, [false, true, true, false, false]);

    spec.placement.worker_stage = Some(crate::spec::names::StageName::new("draft").unwrap());
    assert!(ends_on_submission(&spec, 0), "a worker's own stage ends");
    spec.placement.worker_stage = Some(crate::spec::names::StageName::new("chat").unwrap());
    assert!(
        !ends_on_submission(&spec, 4),
        "an interactive worker stage talks on"
    );
}

/// The answer an agent holds is this visit's only when it was recorded in this
/// visit, and a batch run again after a gate prompt is judged as it was the
/// first time.
#[test]
fn the_held_answer_is_the_visits_own() {
    let recorded = crate::persistence::FinalOutput(FinalOutput::new("a", None, "s".into(), 1));
    let kept = VisitSubmissions {
        visit: "v1".to_string(),
        repeats: 1,
    };
    let stage = g::stage("s");
    let this_visit = BatchSubmissions::new(Some(&stage), "v1", Some(&recorded), Some(&kept), false);
    assert_eq!(this_visit.held.map(|o| o.content.as_str()), Some("a"));
    assert_eq!((this_visit.repeats, this_visit.guarded), (1, true));
    let earlier = BatchSubmissions::new(Some(&stage), "v2", Some(&recorded), Some(&kept), false);
    assert!(earlier.held.is_none());
    assert_eq!(earlier.repeats, 0);
    let rerun = BatchSubmissions::new(Some(&stage), "v1", Some(&recorded), Some(&kept), true);
    assert!(rerun.held.is_none());
    let bare = BatchSubmissions::new(None, "v1", None, None, false);
    assert!(!bare.guarded, "with no graph to read, nothing is guarded");
}

/// A submission of the held answer is answered as a repeat and recorded
/// nothing; a different one is recorded; a refused one leaves the answer.
/// Every call once the visit has an answer counts as a resubmission.
#[test]
fn the_same_answer_again_is_not_recorded_again() {
    let mut w = window();
    let stage = g::stage("s");
    let mut batch = BatchSubmissions::new(Some(&stage), "v1", None, None, false);

    let first = batch.apply(&submit("c1", "the answer"), None, &ctx(), &mut w);
    assert!(
        first.starts_with("Recorded as this run's final output"),
        "{first}"
    );
    assert_eq!((batch.resubmissions, batch.repeats), (0, 0));

    let again = batch.apply(&submit("c2", "the answer"), None, &ctx(), &mut w);
    assert_eq!(again, REPEATED_SUBMISSION);
    assert_eq!((batch.resubmissions, batch.repeats), (1, 1));

    let changed = batch.apply(&submit("c3", "a better answer"), None, &ctx(), &mut w);
    assert!(changed.starts_with("Recorded"), "{changed}");
    assert_eq!((batch.resubmissions, batch.repeats), (2, 0));

    let refused = batch.apply(&submit("c4", "  "), None, &ctx(), &mut w);
    assert!(refused.starts_with("[error]"), "{refused}");
    assert_eq!(batch.resubmissions, 3);
    let kept = batch.submitted.as_ref().map(|o| o.content.as_str());
    assert_eq!(kept, Some("a better answer"));
    assert!(batch.produced.is_empty());
}

/// An interactive stage records every submission, the same one included: its
/// conversation decides when it is over.
#[test]
fn an_interactive_stage_records_every_submission() {
    let mut w = window();
    let stage = StageDef {
        mode: interactive(),
        ..g::stage("s")
    };
    let mut batch = BatchSubmissions::new(Some(&stage), "v1", None, None, false);
    for id in ["c1", "c2"] {
        let text = batch.apply(&submit(id, "the answer"), None, &ctx(), &mut w);
        assert!(text.starts_with("Recorded"), "{text}");
    }
    assert_eq!((batch.resubmissions, batch.repeats), (1, 0));
}

/// Settling keeps the visit's answer and its repeats for the next batch,
/// counts the resubmissions on the stage's record, and ends the stage on an
/// answer in a stage with nowhere to go, or on the last repeat allowed.
#[test]
fn settling_keeps_the_visit_and_says_when_the_stage_ends() {
    let mut world = World::new();
    let agent = world.spawn_empty().id();
    let mut ledger = StageLedger(vec![
        leviath_core::run_meta::StageRecord::new("other".into(), 0),
        leviath_core::run_meta::StageRecord::new("s".into(), 1),
    ]);
    let recorded = crate::persistence::FinalOutput(FinalOutput::new("a", None, "s".into(), 1));
    let kept = VisitSubmissions {
        visit: "v1".to_string(),
        repeats: 1,
    };
    let stage = g::stage("s");
    let mut w = window();
    let mut queue = bevy_ecs::world::CommandQueue::default();
    let mut commands = Commands::new(&mut queue, &world);

    // Nothing submitted and nothing held: nothing kept, nothing ends.
    let idle = BatchSubmissions::new(Some(&stage), "v1", None, None, false);
    assert!(!idle.settle(&mut commands.entity(agent), &state(), true, None));

    // The held answer again, once more than allowed so far: the stage ends.
    let mut repeat = BatchSubmissions::new(Some(&stage), "v1", Some(&recorded), Some(&kept), false);
    repeat.apply(&submit("c1", "a"), None, &ctx(), &mut w);
    assert!(repeat.settle(
        &mut commands.entity(agent),
        &state(),
        false,
        Some(&mut ledger)
    ));
    queue.apply(&mut world);
    assert_eq!(
        world.get::<VisitSubmissions>(agent),
        Some(&VisitSubmissions {
            visit: "v1".to_string(),
            repeats: MAX_REPEATED_SUBMISSIONS,
        })
    );
    assert_eq!(
        ledger.0.iter().map(|r| r.resubmissions).collect::<Vec<_>>(),
        [0, 1]
    );

    // A new answer ends a stage with nowhere to go, and only such a stage.
    let mut queue = bevy_ecs::world::CommandQueue::default();
    let mut commands = Commands::new(&mut queue, &world);
    let mut fresh = BatchSubmissions::new(Some(&stage), "v2", None, None, false);
    fresh.apply(&submit("c2", "b"), None, &ctx(), &mut w);
    assert!(fresh.settle(&mut commands.entity(agent), &state(), true, None));
    assert!(!fresh.settle(&mut commands.entity(agent), &state(), false, None));
    queue.apply(&mut world);
    assert_eq!(world.get::<VisitSubmissions>(agent).unwrap().repeats, 0);
}
