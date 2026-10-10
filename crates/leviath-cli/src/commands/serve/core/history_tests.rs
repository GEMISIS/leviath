//! A run's history read off its run file: the points, a page of them, and
//! one found by its revision.

use leviath_core::run_meta::revision::context_revision;
use leviath_runtime::state::{EntryKind, EntryMeta, EntryState, RunEvent};

use super::super::run_file::tests::{recorded, stateless, step};
use super::*;

/// Record a step that adds `text` to the run's first region.
fn said(run_id: &str, at: i64, text: &str) {
    step(run_id, at, vec![RunEvent::Log(text.into())], |s| {
        s.context.regions[0].entries.push(EntryState {
            text: text.to_string(),
            parts: Vec::new(),
            tokens: 2,
            timestamp: at,
            kind: EntryKind::Text,
            meta: EntryMeta::None,
            key: None,
            reasoning: None,
        });
    });
}

/// A run with a start and three steps that changed its window, and one that
/// did not.
fn history_run() -> String {
    let run_id = recorded();
    said(&run_id, 10, "one");
    step(&run_id, 15, Vec::new(), |s| s.cursor.iteration += 1);
    said(&run_id, 20, "two");
    said(&run_id, 30, "three");
    run_id
}

#[tokio::test]
async fn the_points_are_the_start_and_every_step_that_moved_the_window() {
    crate::runstate::with_isolated_runs_dir_async("history-points", |_d| async move {
        // The process-wide recorder the GraphQL paging tests count with, so
        // whichever test installs it first, every one of them is counted.
        let run_id = history_run();
        let _ = crate::commands::serve::testutil::windows_read_for(&run_id);
        assert_eq!(point_count(&run_id), Some(4));

        let every = every_window(&run_id);
        let at: Vec<i64> = every.iter().map(|p| p.at).collect();
        let spec_at = run_file::require(&run_id).unwrap().spec().created_at;
        assert_eq!(at, vec![spec_at, 10, 20, 30]);
        assert!(every.iter().all(|p| p.meta.run_id == run_id));

        let picked = windows_at(&run_id, &[1, 3]);
        let indices: Vec<usize> = picked.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, vec![1, 3]);
        assert_eq!(picked[0].1.at, 10);
        assert!(windows_at(&run_id, &[]).is_empty());

        let revision = context_revision(&every[2].context);
        let found = at_revision(&run_id, &revision).unwrap();
        assert_eq!(found.at, 20);
        assert!(at_revision(&run_id, "no-such-revision").is_none());
    })
    .await;
}

/// What a stage wrote on the step that took it to the next one is listed
/// under the stage that wrote it, and the stage entered still starts with the
/// window as that step left it.
#[tokio::test]
async fn a_stage_is_credited_with_what_it_wrote_on_its_way_out() {
    crate::runstate::with_isolated_runs_dir_async("history-way-out", |_d| async move {
        let run_id = recorded();
        let start = run_file::require(&run_id).unwrap();
        let from = start.state_at(0).unwrap().cursor.stage.to_string();
        said(&run_id, 10, "one");
        step(&run_id, 20, Vec::new(), |s| {
            s.context.regions[0].entries.push(EntryState {
                text: "the render".to_string(),
                parts: Vec::new(),
                tokens: 2,
                timestamp: 20,
                kind: EntryKind::Text,
                meta: EntryMeta::None,
                key: None,
                reasoning: None,
            });
            s.cursor.stage = leviath_runtime::spec::names::StageName::new("implement").unwrap();
        });

        let every = every_window(&run_id);
        let listed: Vec<(i64, &str)> = every
            .iter()
            .map(|p| (p.at, p.meta.current_stage.as_str()))
            .collect();
        assert_eq!(
            listed[1..],
            [(10, from.as_str()), (20, from.as_str()), (20, "implement")]
        );
        let holds = |p: &RunPoint| {
            p.context
                .regions
                .iter()
                .flat_map(|r| &r.entries)
                .any(|e| e.content == "the render")
        };
        assert!(holds(&every[2]), "the stage that wrote it ends with it");
        assert!(holds(&every[3]));
        assert_eq!(point_count(&run_id), Some(4));
    })
    .await;
}

/// A state with no window at all is no point of the history, and the points
/// after it keep counting from where the last one left off.
#[tokio::test]
async fn a_state_with_no_window_is_no_point() {
    crate::runstate::with_isolated_runs_dir_async("history-no-window", |_d| async move {
        let run_id = recorded();
        said(&run_id, 10, "one");
        let regions = run_file::require(&run_id)
            .unwrap()
            .latest_state()
            .unwrap()
            .context
            .regions;
        step(&run_id, 20, Vec::new(), |s| s.context.regions.clear());
        step(&run_id, 30, Vec::new(), |s| s.context.regions = regions);
        assert_eq!(point_count(&run_id), Some(3));
        let at: Vec<i64> = every_window(&run_id).iter().map(|p| p.at).collect();
        let spec_at = run_file::require(&run_id).unwrap().spec().created_at;
        assert_eq!(at, vec![spec_at, 10, 30]);
        let picked = windows_at(&run_id, &[2]);
        assert_eq!(picked[0].1.at, 30);
    })
    .await;
}

/// Start `run_id`'s file again from the state it started in, as a run
/// converted from an earlier release that listed it with `first_point_at`,
/// converted at step `seq`.
fn as_converted(run_id: &str, first_point_at: Option<i64>, seq: u64) {
    use leviath_runtime::runfile::{CheckpointPolicy, RunFileReader, RunFileWriter};
    let path = run_file::path(run_id);
    let reader = RunFileReader::open(&path).unwrap();
    let mut spec = reader.spec().clone();
    spec.listed = Some(leviath_runtime::spec::run_spec::ListedAs {
        model: None,
        num_stages: 1,
        max_child_depth: 0,
        blueprint_digest: None,
        seq,
        last_progress_at: None,
        clock: None,
        empty_output: false,
        stages: Vec::new(),
        first_point_at,
    });
    let start = reader.state_at(0).unwrap();
    RunFileWriter::create(
        &path,
        &spec,
        &leviath_runtime::spec::env::CodeFiles::new(),
        &start,
        CheckpointPolicy::default(),
    )
    .unwrap();
}

/// A converted run's history starts when its release listed the first point,
/// and one whose release listed no history lists none up to the step it was
/// converted at: only the steps this build took after it.
#[tokio::test]
async fn a_converted_history_starts_where_its_release_listed_it() {
    crate::runstate::with_isolated_runs_dir_async("history-converted", |_d| async move {
        let run_id = recorded();
        as_converted(&run_id, Some(5), 1);
        said(&run_id, 10, "one");
        let at: Vec<i64> = every_window(&run_id).iter().map(|p| p.at).collect();
        assert_eq!(at, vec![5, 10]);

        let unlisted = recorded();
        as_converted(&unlisted, None, 1);
        said(&unlisted, 10, "one");
        // Nothing to list yet: no history at all, as its release answered.
        let spec = HistorySpec::resolve(&unlisted, None, None, None).unwrap();
        let err = page(&unlisted, &spec).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("No context history for run '{unlisted}'")
        );
        said(&unlisted, 20, "two");
        let at: Vec<i64> = every_window(&unlisted).iter().map(|p| p.at).collect();
        assert_eq!(at, vec![20]);
        assert_eq!(point_count(&unlisted), Some(1));
    })
    .await;
}

#[tokio::test]
async fn a_history_pages_forwards_and_backwards() {
    crate::runstate::with_isolated_runs_dir_async("history-page", |_d| async move {
        let run_id = history_run();
        let first = HistorySpec::resolve(&run_id, Some(3), None, None).unwrap();
        let page_one = page(&run_id, &first).unwrap();
        assert_eq!(page_one.total, 4);
        assert_eq!(page_one.points.len(), 3);
        let cursor = page_one.next_cursor.expect("one more point");
        let rest = HistorySpec::resolve(&run_id, Some(3), None, Some(&cursor)).unwrap();
        let page_two = page(&run_id, &rest).unwrap();
        assert_eq!(page_two.points.len(), 1);
        assert_eq!(page_two.points[0].at, 30);
        assert!(page_two.next_cursor.is_none());

        let newest = HistorySpec::resolve(&run_id, Some(2), Some("desc"), None).unwrap();
        let down = page(&run_id, &newest).unwrap();
        let at: Vec<i64> = down.points.iter().map(|p| p.at).collect();
        assert_eq!(at, vec![30, 20]);
        let cursor = down.next_cursor.unwrap();
        let older = HistorySpec::resolve(&run_id, Some(2), Some("desc"), Some(&cursor)).unwrap();
        let at: Vec<i64> = page(&run_id, &older)
            .unwrap()
            .points
            .iter()
            .map(|p| p.at)
            .collect();
        assert_eq!(at[0], 10);
    })
    .await;
}

#[tokio::test]
async fn a_run_with_no_readable_file_has_no_history() {
    crate::runstate::with_isolated_runs_dir_async("history-none", |_d| async move {
        assert!(point_count("ghost").is_none());
        assert!(every_window("ghost").is_empty());
        assert!(at_revision("ghost", "r").is_none());
        let spec = HistorySpec::resolve("ghost", None, None, None).unwrap();
        assert_eq!(page("ghost", &spec).unwrap_err().code(), "NOT_FOUND");

        let run_id = recorded();
        stateless(&run_id);
        assert!(point_count(&run_id).is_none());

        // A start that reads and a step that does not.
        let run_id = recorded();
        let mut bytes = std::fs::read(run_file::path(&run_id)).unwrap();
        bytes.extend(
            leviath_runtime::runfile::codec::encode(
                leviath_runtime::runfile::codec::FrameKind::Delta,
                &1u64,
            )
            .unwrap(),
        );
        super::super::run_file::tests::garbage(&run_id, &bytes);
        assert!(point_count(&run_id).is_none());
    })
    .await;
}

/// A visitor that stops at the first point stops the walk there.
#[tokio::test]
async fn a_walk_stopped_at_the_start_reads_nothing_more() {
    crate::runstate::with_isolated_runs_dir_async("history-stop", |_d| async move {
        let run_id = history_run();
        let start = windows_at(&run_id, &[0]);
        assert_eq!(start.len(), 1);
        let revision = context_revision(&start[0].1.context);
        assert_eq!(at_revision(&run_id, &revision).unwrap().at, start[0].1.at);
    })
    .await;
}
