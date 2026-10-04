use super::*;
use leviath_core::run_meta::{RunMeta, RunStatus};

fn meta(run_id: &str, started: i64, ended: i64) -> RunMeta {
    let mut m = RunMeta::new(
        run_id.to_string(),
        "deep-researcher".to_string(),
        "/agents/deep-researcher".to_string(),
        "task".to_string(),
        Some("openrouter/x-ai/grok-4.6".to_string()),
        "/tmp".to_string(),
        3,
    );
    m.started_at = started;
    m.updated_at = ended;
    m.status = RunStatus::Complete;
    m
}

fn usage(
    kind: CallKind,
    stage: &str,
    iteration: usize,
    model: &str,
    out: usize,
    at: i64,
) -> Moment {
    Moment::Call {
        kind,
        stage: stage.to_string(),
        iteration,
        model: model.to_string(),
        prompt_tokens: 1_000,
        completion_tokens: out,
        cached_tokens: 100,
        at,
    }
}

fn status(status: RunStatus, at: i64) -> Moment {
    Moment::Status { status, at }
}

fn tool_done(at: i64) -> Moment {
    Moment::ToolDone { at }
}

/// The steps of a run that searched, spawned children, waited, then wrote a
/// report five times at the same size: every span kind the reducer knows.
fn journal() -> Vec<Moment> {
    vec![
        usage(
            CallKind::Title,
            "",
            0,
            "anthropic/claude-sonnet-5",
            30,
            1_003,
        ),
        usage(CallKind::Stage, "gather", 1, "x-ai/grok-4.6", 600, 1_010),
        tool_done(1_012),
        usage(CallKind::Stage, "gather", 2, "x-ai/grok-4.6", 500, 1_030),
        usage(CallKind::Routing, "gather", 2, "x-ai/grok-4.6", 3, 1_040),
        // A status change that is not "waiting" while not waiting: nothing to close.
        status(RunStatus::Running, 1_040),
        status(RunStatus::WaitingInput, 1_045),
        // A child's title call journaled while parked is not this run's time.
        usage(
            CallKind::Title,
            "",
            0,
            "anthropic/claude-sonnet-5",
            30,
            1_050,
        ),
        status(RunStatus::Running, 1_345),
        usage(
            CallKind::Stage,
            "polish",
            3,
            "google/gemini-3.1-pro-preview",
            23_050,
            1_545,
        ),
        usage(
            CallKind::Stage,
            "polish",
            4,
            "google/gemini-3.1-pro-preview",
            23_046,
            1_745,
        ),
        usage(
            CallKind::Stage,
            "polish",
            5,
            "google/gemini-3.1-pro-preview",
            23_996,
            1_945,
        ),
        // Large but a different stage: the run of repeats ends here.
        usage(
            CallKind::Stage,
            "summary",
            6,
            "anthropic/claude-sonnet-5",
            20_000,
            2_045,
        ),
        // Small and consecutive: ordinary, never a warning.
        usage(
            CallKind::Stage,
            "summary",
            7,
            "anthropic/claude-sonnet-5",
            200,
            2_050,
        ),
        usage(
            CallKind::Stage,
            "summary",
            8,
            "anthropic/claude-sonnet-5",
            210,
            2_055,
        ),
    ]
}

#[test]
fn the_split_adds_up_and_waiting_is_not_model_time() {
    let t = analyze(&meta("r", 1_000, 2_060), &journal());
    assert_eq!(t.totals.wall, 1_060);
    assert_eq!(t.totals.waiting, 300);
    assert_eq!(t.totals.tools, 2);
    // Title 3 + gather 7 + 18 + routing 10 + polish 200+200+200 + summary 100+5+5.
    assert_eq!(t.totals.inference, 748);
    assert_eq!(t.totals.other, 1_060 - 748 - 2 - 300);
    // The parked title call was skipped: 11 usage records, 10 spans.
    assert_eq!(t.calls.len(), 10);
    assert_eq!(t.status, "complete");
}

#[test]
fn stages_roll_up_in_first_seen_order_with_routing_folded_in() {
    let t = analyze(&meta("r", 1_000, 2_060), &journal());
    let names: Vec<&str> = t.stages.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["(title)", "gather", "polish", "summary"]);
    let gather = &t.stages[1];
    assert_eq!(
        (gather.calls, gather.output_tokens, gather.largest_reply),
        (3, 1_103, 600)
    );
    assert_eq!(gather.secs, 7 + 18 + 10);
}

#[test]
fn five_replies_at_the_cap_are_named_as_one_warning() {
    let t = analyze(&meta("r", 1_000, 2_060), &journal());
    assert_eq!(t.warnings.len(), 1, "{:?}", t.warnings);
    assert!(t.warnings[0].contains("stage `polish`: 3 back-to-back replies of about 23996"));
    assert!(t.warnings[0].contains("(600s)"));
}

/// A tool that finishes while the run is parked was in flight when the wait
/// began: an approval it asked for, children it started. Its time runs up to
/// the wait and the wait is waiting, never both, so the split adds up to the
/// wall clock. Seen on a real run cancelled at its approval prompt: tools 82
/// and waiting 74 of a wall of 82.
#[test]
fn a_tool_that_ends_while_parked_is_not_counted_twice() {
    let moments = [
        tool_done(1_001),
        tool_done(1_005),
        status(RunStatus::WaitingInput, 1_008),
        tool_done(1_082),
        status(RunStatus::Cancelled, 1_082),
    ];
    let t = analyze(&meta("r", 1_000, 1_082), &moments).totals;
    assert_eq!((t.tools, t.waiting, t.other), (8, 74, 0));
    assert_eq!(t.inference + t.tools + t.waiting + t.other, t.wall);
}

/// A run said to be waiting twice over is waiting from the first time: the
/// second says nothing new, and starting the wait again would drop the time
/// between them from every total.
#[test]
fn waiting_said_twice_keeps_the_first_start() {
    let moments = [
        status(RunStatus::WaitingInput, 1_010),
        status(RunStatus::WaitingInput, 1_050),
        status(RunStatus::Running, 1_100),
    ];
    let t = analyze(&meta("r", 1_000, 1_100), &moments).totals;
    assert_eq!((t.waiting, t.other), (90, 10));
}

#[test]
fn a_run_with_no_records_is_all_other_time() {
    let t = analyze(&meta("r", 1_000, 1_100), &[]);
    assert_eq!(
        t.totals,
        Totals {
            wall: 100,
            other: 100,
            ..Totals::default()
        }
    );
    assert!(t.stages.is_empty() && t.warnings.is_empty());
}

#[test]
fn a_clock_that_went_backwards_never_makes_a_negative_total() {
    let records = [
        usage(CallKind::Stage, "gather", 1, "m", 10, 900),
        tool_done(890),
    ];
    let t = analyze(&meta("r", 1_000, 950), &records);
    assert_eq!(t.totals.wall, 0);
    assert_eq!(t.calls[0].secs(), 0);
    assert_eq!(t.totals.tools, 0);
}

#[test]
fn peak_in_flight_counts_overlap_per_model_and_a_touch_is_not_an_overlap() {
    let call = |model: &str, s: i64, e: i64| CallSpan {
        stage: "analyze".to_string(),
        iteration: 1,
        kind: "stage".to_string(),
        model: model.to_string(),
        started_at: s,
        ended_at: e,
        prompt_tokens: 0,
        cached_tokens: 0,
        completion_tokens: 0,
    };
    let run = |calls: Vec<CallSpan>| RunTimeline {
        run_id: "r".to_string(),
        agent_name: "a".to_string(),
        status: "complete".to_string(),
        depth: 0,
        children: vec![],
        totals: Totals::default(),
        stages: vec![],
        calls,
        warnings: vec![],
    };
    let runs = [
        run(vec![
            call("opus", 0, 10),
            call("opus", 5, 15),
            call("sonnet", 0, 3),
        ]),
        run(vec![call("opus", 8, 20), call("sonnet", 3, 6)]),
    ];
    assert_eq!(
        peak_in_flight(&runs),
        vec![("opus".to_string(), 3), ("sonnet".to_string(), 1)]
    );
}

#[test]
fn durations_read_as_clock_time() {
    assert_eq!(hms(0), "0:00");
    assert_eq!(hms(65), "1:05");
    assert_eq!(hms(3_725), "1:02:05");
    assert_eq!(hms(-5), "0:00");
    assert_eq!(leviath_core::truncate_chars("short", 20), "short");
    assert_eq!(
        leviath_core::truncate_chars(&"é".repeat(30), 5)
            .chars()
            .count(),
        5
    );
}

#[test]
fn the_report_prints_in_every_shape() {
    let t = analyze(&meta("r", 1_000, 2_060), &journal());
    print_run(&t, false);
    print_run(&t, true);
    print_tree(&[t]);
}

/// A run tree on disk in an isolated runs dir: a root with its steps, one
/// child with its steps, and one child whose run file will not read.
async fn with_tree<R, Fut>(unique: &str, f: impl FnOnce(String) -> Fut) -> R
where
    Fut: std::future::Future<Output = R>,
{
    crate::runstate::with_isolated_runs_dir_async(unique, |_base| async move {
        let mut root = meta("root-1", 1_000, 2_060);
        root.children = vec!["child-1".to_string(), "child-torn".to_string()];
        crate::runstate::create_run(&root).expect("root");
        write_journal("root-1", &journal());

        let mut child = meta("child-1", 1_045, 1_345);
        child.depth = 1;
        child.parent_run_id = Some("root-1".to_string());
        crate::runstate::create_run(&child).expect("child");
        write_journal(
            "child-1",
            &[usage(
                CallKind::Stage,
                "gather",
                1,
                "anthropic/claude-sonnet-5",
                500,
                1_100,
            )],
        );

        let mut torn = meta("child-torn", 1_045, 1_345);
        torn.depth = 1;
        crate::runstate::create_run(&torn).expect("torn child");
        let path = crate::runstate::run_file::path_in(&crate::runstate::run_dir("child-torn"));
        let mut bytes = std::fs::read(&path).expect("the run file");
        bytes.extend(
            leviath_runtime::runfile::codec::encode(
                leviath_runtime::runfile::codec::FrameKind::Delta,
                &9u64,
            )
            .expect("a frame"),
        );
        std::fs::write(&path, bytes).expect("torn");

        f("root-1".to_string()).await
    })
    .await
}

/// Record `moments` as steps of `run_id`'s file, one step each.
fn write_journal(run_id: &str, moments: &[Moment]) {
    use leviath_runtime::spec::names::{ModelId, ModelRef, ProviderName};
    use leviath_runtime::state::{RunEvent, RunStatus as State, Spend, ToolResultState};
    let dir = crate::runstate::run_dir(run_id);
    for (i, moment) in moments.iter().enumerate() {
        let (events, status) = match moment {
            Moment::Status { status, .. } => (
                Vec::new(),
                Some(match status {
                    RunStatus::WaitingInput => State::Waiting,
                    RunStatus::Complete => State::Complete,
                    _ => State::Active,
                }),
            ),
            Moment::Call {
                kind,
                stage,
                iteration,
                model,
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                ..
            } => (
                vec![RunEvent::Inference {
                    attempt: String::new(),
                    model: ModelRef {
                        provider: ProviderName::new("openrouter").ok(),
                        model: ModelId::new(model).expect("a model"),
                    },
                    spend: Spend {
                        prompt_tokens: *prompt_tokens as u64,
                        completion_tokens: *completion_tokens as u64,
                        cached_tokens: *cached_tokens as u64,
                        ..Spend::default()
                    },
                    finish_reason: None,
                    kind: *kind,
                    stage: leviath_runtime::spec::names::StageName::new(stage).ok(),
                    iteration: *iteration as u32,
                }],
                None,
            ),
            Moment::ToolDone { .. } => (
                vec![RunEvent::ToolFinished {
                    call_id: "c1".to_string(),
                    result: ToolResultState {
                        text: "ok".to_string(),
                        is_error: false,
                    },
                    millis: 0,
                }],
                None,
            ),
        };
        crate::runstate::run_file::tests::step_with(&dir, 1_000 + i as i64, events, |s| {
            if let Some(status) = status {
                s.status = status;
            }
        });
    }
}

#[tokio::test]
async fn the_table_reads_a_real_journal() {
    with_tree("timeline-table", |run_id| async move {
        execute(TimelineArgs {
            run_id,
            json: false,
            calls: true,
            tree: false,
        })
        .await
        .expect("a journal on disk is readable");
    })
    .await;
}

#[tokio::test]
async fn the_tree_includes_children_and_skips_one_with_no_journal() {
    with_tree("timeline-tree", |run_id| async move {
        execute(TimelineArgs {
            run_id: run_id.clone(),
            json: false,
            calls: false,
            tree: true,
        })
        .await
        .expect("tree");
        // The JSON form is the same walk, so it is the assertable one.
        let root = load(&run_id).expect("root loads");
        assert_eq!(root.children.len(), 2);
        assert!(load("child-1").is_ok());
        let err = load("child-torn").expect_err("a run file that will not read");
        assert!(err.to_string().contains("no readable"), "{err}");
        execute(TimelineArgs {
            run_id,
            json: true,
            calls: false,
            tree: true,
        })
        .await
        .expect("json tree");
    })
    .await;
}

/// A run's file is read the same way: each model call in the stage it was
/// made in, each tool result, and the time spent waiting.
#[tokio::test]
async fn a_run_file_reads_as_a_timeline() {
    use crate::runstate::run_file::tests::{recorded, step_with};
    use leviath_runtime::spec::names::ModelRef;
    use leviath_runtime::state::{RunEvent, RunStatus as State, Spend};
    crate::runstate::with_isolated_runs_dir_async("timeline-run-file", |_d| async {
        let dir = recorded(&crate::runstate::runs_dir());
        let run_id = dir.file_name().unwrap().to_string_lossy().into_owned();
        // Each call says what kind it was and where it was made, so a title
        // call is a row of its own and a call is placed by its own record,
        // not by the step that happened to carry it.
        let call = |completion_tokens, kind, stage: Option<&str>, iteration| RunEvent::Inference {
            attempt: "a".to_string(),
            model: ModelRef::parse("anthropic/claude-sonnet-5").unwrap(),
            spend: Spend {
                prompt_tokens: 100,
                completion_tokens,
                ..Spend::default()
            },
            finish_reason: None,
            kind,
            stage: stage.map(|s| leviath_runtime::spec::names::StageName::new(s).unwrap()),
            iteration,
        };
        step_with(
            &dir,
            100,
            vec![call(10, CallKind::Stage, Some("analyze"), 1)],
            |s| s.status = State::Active,
        );
        step_with(&dir, 105, vec![call(3, CallKind::Title, None, 0)], |_| {});
        step_with(
            &dir,
            110,
            vec![call(20, CallKind::Stage, Some("analyze"), 2)],
            |s| s.cursor.iteration = 1,
        );
        let done = RunEvent::ToolFinished {
            call_id: "c1".to_string(),
            result: leviath_runtime::state::ToolResultState {
                text: "ok".to_string(),
                is_error: false,
            },
            millis: 5,
        };
        step_with(&dir, 115, vec![done], |_| {});
        step_with(&dir, 120, vec![RunEvent::Log("parked".into())], |s| {
            s.status = State::Waiting
        });
        step_with(&dir, 150, Vec::new(), |s| s.status = State::Active);
        let timeline = load(&run_id).expect("the run file reads");
        assert_eq!(timeline.calls.len(), 3);
        assert_eq!(timeline.calls[0].stage, "analyze");
        assert_eq!(timeline.calls[0].iteration, 1);
        assert_eq!(timeline.calls[1].kind, "title");
        assert_eq!(timeline.calls[1].stage, "");
        assert_eq!(timeline.calls[2].iteration, 2);
        assert_eq!(timeline.calls[2].completion_tokens, 20);
        assert_eq!(timeline.calls[0].model, "claude-sonnet-5");
        let rows: Vec<(&str, usize)> = timeline
            .stages
            .iter()
            .map(|s| (s.name.as_str(), s.calls))
            .collect();
        assert_eq!(rows, vec![("analyze", 2), ("(title)", 1)]);
        assert_eq!(timeline.totals.waiting, 30);
        assert_eq!(timeline.totals.tools, 5);

        // A step that will not decode leaves nothing to show.
        let mut bytes = std::fs::read(dir.join(leviath_core::files::RUN_FILE)).unwrap();
        bytes.extend(
            leviath_runtime::runfile::codec::encode(
                leviath_runtime::runfile::codec::FrameKind::Delta,
                &9u64,
            )
            .unwrap(),
        );
        std::fs::write(dir.join(leviath_core::files::RUN_FILE), bytes).unwrap();
        assert!(load(&run_id).is_err());
    })
    .await;
}

#[tokio::test]
async fn a_run_with_no_meta_is_an_error_rather_than_an_empty_table() {
    let err = execute(TimelineArgs {
        run_id: "no-such-run".to_string(),
        json: false,
        calls: false,
        tree: false,
    })
    .await
    .expect_err("a missing run is worth saying");
    assert!(err.to_string().contains("no readable record"), "{err}");
}
