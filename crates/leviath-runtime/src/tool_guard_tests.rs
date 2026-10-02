//! A tool batch that panics still reports.

use super::*;

fn ids() -> Vec<String> {
    vec!["c1".to_string(), "c2".to_string()]
}

fn text(results: &[ToolResult]) -> Vec<(String, String)> {
    results
        .iter()
        .map(|(id, content)| (id.clone(), content.as_str().to_string()))
        .collect()
}

/// A batch that runs reports what it returns.
#[tokio::test]
async fn a_batch_that_runs_reports_its_results() {
    let exec: BoxedToolExec =
        Box::new(|| Box::pin(async { vec![("c1".to_string(), "ok".into())] }));
    let results = guarded(exec, ids())().await;
    assert_eq!(text(&results), [("c1".to_string(), "ok".to_string())]);
}

/// A batch that panics while it runs, or while it is being built, reports
/// an error for each of its calls.
#[tokio::test]
async fn a_batch_that_panics_reports_each_call_failed() {
    let silent = crate::test_support::SilentPanics::install();
    let running: BoxedToolExec = Box::new(|| {
        Box::pin(async {
            tokio::task::yield_now().await;
            panic!("the executor blew up")
        })
    });
    let building: BoxedToolExec = Box::new(|| panic!("the batch could not be built"));
    let while_running = guarded(running, ids())().await;
    let while_building = guarded(building, ids())().await;
    drop(silent);
    for (results, why) in [
        (while_running, "the executor blew up"),
        (while_building, "the batch could not be built"),
    ] {
        let results = text(&results);
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].0, "c2");
        assert!(results[0].1.starts_with("[error]"), "{}", results[0].1);
        assert!(results[0].1.contains(why), "{}", results[0].1);
    }
}
