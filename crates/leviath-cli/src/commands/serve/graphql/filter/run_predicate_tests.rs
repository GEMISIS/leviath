//! Tests for the adapter between the run mirror and the run listing.

use async_graphql::{ID, InputType, Value};

use super::*;
use crate::commands::serve::core::runs::predicate::{MatchContext, RunTree};
use crate::commands::serve::graphql::paging::digest::{MAX_FILTER_DEPTH, MAX_FILTER_NODES};
use crate::commands::serve::graphql::paging::walk::Verdict;
use crate::commands::serve::testutil::state_with_agent_paths;
use crate::runstate::{RunStatus, create_run, with_isolated_runs_dir_async};

/// One run on disk, started at a known second so ordering is assertable.
fn meta(id: &str, started_at: i64) -> RunMeta {
    let mut record = RunMeta::new(
        id.to_string(),
        "agent".to_string(),
        "/agents/agent".to_string(),
        "task".to_string(),
        None,
        "/work".to_string(),
        1,
    );
    record.started_at = started_at;
    record.updated_at = started_at;
    record
}

/// A filter, read off the wire the way a request delivers one.
fn filter(json: serde_json::Value) -> RunFilter {
    let value = Value::from_json(json).expect("the filter is a GraphQL value");
    RunFilter::parse(Some(value)).expect("the filter parses")
}

/// The context one comparison runs in, with the tree these runs make.
fn context(runs: &[Arc<RunMeta>]) -> MatchContext {
    MatchContext {
        now: 1_700_000_000,
        tree: Arc::new(RunTree::of(runs)),
    }
}

/// A filter that asks nothing contributes nothing, which is what keeps an
/// unfiltered GraphQL cursor readable by the REST listing.
#[test]
fn a_filter_that_asks_nothing_compiles_to_no_predicate() {
    assert!(
        compile(RunFilter::default())
            .expect("an empty filter compiles")
            .is_none()
    );
}

/// A filter answerable from the record is settled at the cheap pass, in both
/// directions, and digests as what the client wrote.
#[test]
fn a_record_filter_is_settled_without_a_read() {
    let predicate = compile(filter(serde_json::json!({ "task": { "eq": "task" } })))
        .expect("it compiles")
        .expect("it asks something");
    let kept = Arc::new(meta("run-a", 1));
    let ctx = context(&[Arc::clone(&kept)]);
    assert!(predicate.matches(&kept, &ctx));
    assert_eq!(predicate.verdict(&kept, &ctx), Verdict::Keep);
    assert_eq!(predicate.digest_part(), "{task:{eq:\"task\"}}");

    let other = compile(filter(serde_json::json!({ "task": { "eq": "else" } })))
        .expect("it compiles")
        .expect("it asks something");
    assert!(!other.matches(&kept, &ctx));
    assert_eq!(other.verdict(&kept, &ctx), Verdict::Drop);
}

/// A filter that names a file is undecided until the walk settles it, and a
/// run that has no such file is refused when it does.
#[tokio::test]
async fn a_file_backed_filter_waits_for_the_confirmation() {
    with_isolated_runs_dir_async("run-predicate-io", |_dir| async move {
        create_run(&meta("run-a", 1)).expect("created");
        let predicate = compile(filter(
            serde_json::json!({ "finalOutput": { "isNull": false } }),
        ))
        .expect("it compiles")
        .expect("it asks something");
        let run = Arc::new(meta("run-a", 1));
        let ctx = context(&[Arc::clone(&run)]);
        assert_eq!(predicate.verdict(&run, &ctx), Verdict::NeedsIo);
        // `matches` is the half that reads nothing, so an undecided filter is
        // not a match to it.
        assert!(!predicate.matches(&run, &ctx));
        assert!(
            !predicate.confirm(&run, &ctx).await,
            "no answer was written"
        );
    })
    .await;
}

/// The relation a filter walks is the tree the listing lent it.
#[test]
fn a_relation_filter_reads_the_listings_own_tree() {
    let mut worker = meta("worker", 2);
    worker.parent_run_id = Some("root".to_string());
    let worker = Arc::new(worker);
    let runs = vec![Arc::new(meta("root", 1)), Arc::clone(&worker)];
    let ctx = context(&runs);

    let under = compile(filter(
        serde_json::json!({ "ancestorIds": { "has": "root" } }),
    ))
    .expect("it compiles")
    .expect("it asks something");
    assert!(under.matches(&worker, &ctx));
    assert!(!under.matches(&runs[0], &ctx));

    let of_root = compile(filter(
        serde_json::json!({ "parent": { "id": { "eq": "root" } } }),
    ))
    .expect("it compiles")
    .expect("it asks something");
    assert!(of_root.matches(&worker, &ctx));
    assert!(!of_root.matches(&runs[0], &ctx));
}

/// `id` on the top level says which runs to read as well as which to keep.
#[test]
fn the_ids_a_filter_names_are_read_from_it() {
    assert!(
        named_ids(&RunFilter::default())
            .expect("no ids is not a refusal")
            .is_none()
    );
    assert!(
        named_ids(&filter(serde_json::json!({ "id": { "isNull": true } })))
            .expect("a filter that names no id is not a refusal")
            .is_none()
    );
    assert_eq!(
        named_ids(&filter(serde_json::json!({ "id": { "eq": "run-a" } }))).expect("one id"),
        Some(vec!["run-a".to_string()])
    );
    assert_eq!(
        named_ids(&filter(
            serde_json::json!({ "id": { "eq": "run-a", "in": ["run-b"] } })
        ))
        .expect("both forms"),
        Some(vec!["run-a".to_string(), "run-b".to_string()])
    );
}

/// More ids than one request may name is a refusal rather than a very large
/// read.
#[test]
fn too_many_ids_are_refused() {
    let many: Vec<String> = (0..=run_core::MAX_IDS)
        .map(|at| format!("run-{at}"))
        .collect();
    let refusal =
        named_ids(&filter(serde_json::json!({ "id": { "in": many } }))).expect_err("over the cap");
    assert!(refusal.to_string().contains("at most"), "{}", refusal);
}

/// What a filter asks of the listing: the ids it names, the predicate it
/// compiles to, and nothing about searching.
#[test]
fn a_filter_becomes_what_the_listing_asks_for() {
    let asked =
        asking(filter(serde_json::json!({ "id": { "eq": "run-a" } })), 7).expect("it compiles");
    assert_eq!(asked.limit, 7);
    assert_eq!(asked.ids, Some(vec!["run-a".to_string()]));
    assert!(asked.predicate.is_some());
    assert!(asked.q.is_none() && asked.sources.is_empty());
    assert_eq!(asked.sort, SortKey::Started);
    assert!(asked.descending);
    assert_eq!(asked.parent, ParentFilter::Any);
}

/// The one function the bulk mutations and the subscriptions resolve a run
/// filter with: every run it matches, reads included, with no page to stop at.
#[tokio::test]
async fn a_selection_is_every_run_the_filter_matches() {
    with_isolated_runs_dir_async("run-predicate-selection", |dir| async move {
        for at in 0..3 {
            let mut run = meta(&format!("run-{at}"), 100 + at);
            run.status = match at {
                0 => RunStatus::Error,
                _ => RunStatus::Complete,
            };
            create_run(&run).expect("created");
        }
        let state = state_with_agent_paths(vec![dir.join("agents")]);

        let all = selection(None, &state).await.expect("every run");
        assert_eq!(all.len(), 3);

        let failed = selection(
            Some(filter(serde_json::json!({ "status": { "eq": "ERROR" } }))),
            &state,
        )
        .await
        .expect("the failed run");
        assert_eq!(
            failed
                .iter()
                .map(|meta| meta.run_id.clone())
                .collect::<Vec<_>>(),
            vec!["run-0".to_string()]
        );
    })
    .await;
}

/// An export runs on the ids the filter resolved to, so writing the file does
/// not answer the filter a second time.
#[tokio::test]
async fn an_export_selection_names_its_runs() {
    with_isolated_runs_dir_async("run-predicate-everything", |dir| async move {
        create_run(&meta("run-a", 100)).expect("created");
        create_run(&meta("run-b", 200)).expect("created");
        let state = state_with_agent_paths(vec![dir.join("agents")]);

        let asked = everything(
            Some(filter(serde_json::json!({ "id": { "eq": "run-b" } }))),
            &state,
        )
        .await
        .expect("a spec");
        assert_eq!(asked.ids, Some(vec!["run-b".to_string()]));
        assert!(asked.predicate.is_none(), "the filter is already answered");
        assert_eq!(asked.limit, usize::MAX);
    })
    .await;
}

/// A filter too large to walk is refused before a single run is compared.
///
/// Nesting reaches the size limit long before the depth one, because every
/// level of a run filter is a whole mirror: the two limits are one refusal from
/// a client's side, and this is the one a real filter hits.
#[test]
fn a_filter_past_the_size_limit_is_refused() {
    let deep = (0..MAX_FILTER_DEPTH + 2).fold(
        serde_json::json!({ "task": { "eq": "x" } }),
        |inner, _| serde_json::json!({ "not": inner }),
    );
    let refusal = compile(filter(deep)).expect_err("too large to walk");
    assert!(refusal.to_string().contains("split the query"), "{refusal}");

    let named: Vec<String> = (0..MAX_FILTER_NODES + 2)
        .map(|at| format!("run-{at}"))
        .collect();
    let wide = serde_json::json!({ "id": { "in": named } });
    let refusal = compile(filter(wide)).expect_err("too large to walk");
    assert!(refusal.to_string().contains("split the query"), "{refusal}");
}

/// An id is compared as the opaque token it is, so the mirror's own `id`
/// filter still applies to what a batch fetch read.
#[test]
fn the_id_filter_still_applies_to_what_was_read() {
    let predicate = compile(filter(serde_json::json!({ "id": { "eq": "run-a" } })))
        .expect("it compiles")
        .expect("it asks something");
    let wanted = Arc::new(meta("run-a", 1));
    let other = Arc::new(meta("run-b", 1));
    let ctx = context(&[Arc::clone(&wanted), Arc::clone(&other)]);
    assert!(predicate.matches(&wanted, &ctx));
    assert!(!predicate.matches(&other, &ctx));
    // The id the filter carries reads back as itself.
    assert_eq!(ID("run-a".to_string()).as_str(), "run-a");
}
