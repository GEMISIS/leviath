use super::*;
use leviath_core::run_archive::{RUN_ARCHIVE_VERSION, write_archive_start, write_record};
use leviath_core::run_meta::{ContextSnapshot, RunMeta, RunStatus};

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
    kind: InferenceKind,
    stage: &str,
    iteration: usize,
    model: &str,
    out: usize,
    at: i64,
) -> RunRecord {
    RunRecord::InferenceUsage {
        kind,
        stage: stage.to_string(),
        iteration,
        provider: "openrouter".to_string(),
        model: model.to_string(),
        prompt_tokens: 1_000,
        completion_tokens: out,
        cached_tokens: 100,
        cache_write_tokens: 0,
        cost_usd: None,
        cost_reported_by_provider: None,
        at,
    }
}

fn status(status: RunStatus, at: i64) -> RunRecord {
    RunRecord::StatusChanged { status, at }
}

fn tool_done(at: i64) -> RunRecord {
    RunRecord::ToolCallDone {
        iteration: 1,
        call_id: "c1".to_string(),
        result: "ok".to_string(),
        at,
    }
}

/// The journal of a run that searched, spawned children, waited, then wrote a
/// report five times at the same size: every span kind the reducer knows.
fn journal() -> Vec<RunRecord> {
    vec![
        // Ignored kinds cover the catch-all arm.
        RunRecord::ContextCheckpoint {
            snapshot: ContextSnapshot {
                stage_name: "gather".to_string(),
                total_tokens: 0,
                max_tokens: 1_000,
                regions: vec![],
            },
            at: 1_000,
        },
        usage(
            InferenceKind::Title,
            "",
            0,
            "anthropic/claude-sonnet-5",
            30,
            1_003,
        ),
        usage(
            InferenceKind::Stage,
            "gather",
            1,
            "x-ai/grok-4.6",
            600,
            1_010,
        ),
        RunRecord::ToolBatch {
            calls: vec![],
            at: 1_010,
            stage_index: 0,
            iteration: 1,
            response: String::new(),
        },
        tool_done(1_012),
        usage(
            InferenceKind::Stage,
            "gather",
            2,
            "x-ai/grok-4.6",
            500,
            1_030,
        ),
        usage(
            InferenceKind::Routing,
            "gather",
            2,
            "x-ai/grok-4.6",
            3,
            1_040,
        ),
        // A status change that is not "waiting" while not waiting: nothing to close.
        status(RunStatus::Running, 1_040),
        status(RunStatus::WaitingInput, 1_045),
        // A child's title call journaled while parked is not this run's time.
        usage(
            InferenceKind::Title,
            "",
            0,
            "anthropic/claude-sonnet-5",
            30,
            1_050,
        ),
        status(RunStatus::Running, 1_345),
        usage(
            InferenceKind::Stage,
            "polish",
            3,
            "google/gemini-3.1-pro-preview",
            23_050,
            1_545,
        ),
        usage(
            InferenceKind::Stage,
            "polish",
            4,
            "google/gemini-3.1-pro-preview",
            23_046,
            1_745,
        ),
        usage(
            InferenceKind::Stage,
            "polish",
            5,
            "google/gemini-3.1-pro-preview",
            23_996,
            1_945,
        ),
        // Large but a different stage: the run of repeats ends here.
        usage(
            InferenceKind::Stage,
            "summary",
            6,
            "anthropic/claude-sonnet-5",
            20_000,
            2_045,
        ),
        // Small and consecutive: ordinary, never a warning.
        usage(
            InferenceKind::Stage,
            "summary",
            7,
            "anthropic/claude-sonnet-5",
            200,
            2_050,
        ),
        usage(
            InferenceKind::Stage,
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
        usage(InferenceKind::Stage, "gather", 1, "m", 10, 900),
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
    assert_eq!(truncate("short", 20), "short");
    assert_eq!(truncate(&"é".repeat(30), 5).chars().count(), 5);
}

#[test]
fn the_report_prints_in_every_shape() {
    let t = analyze(&meta("r", 1_000, 2_060), &journal());
    print_run(&t, false);
    print_run(&t, true);
    print_tree(&[t]);
}

/// A run tree on disk in an isolated runs dir: a root with a journal, one
/// child with a journal, one child with a meta but no journal.
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
                InferenceKind::Stage,
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

        f("root-1".to_string()).await
    })
    .await
}

fn write_journal(run_id: &str, records: &[RunRecord]) {
    let mut bytes = Vec::new();
    write_archive_start(&mut bytes, RUN_ARCHIVE_VERSION).expect("preamble");
    for r in records {
        write_record(&mut bytes, r).expect("record");
    }
    std::fs::write(crate::runstate::run_dir(run_id).join("run.lvr"), bytes).expect("journal");
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
        let err = load("child-torn").expect_err("no journal");
        assert!(err.to_string().contains("no readable journal"), "{err}");
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
    assert!(err.to_string().contains("no readable meta.json"), "{err}");
}
