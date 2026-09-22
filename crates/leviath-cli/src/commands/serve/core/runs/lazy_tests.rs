//! Tests for the lazy run walk.
//!
//! The predicate here counts every confirmation it runs, because most of what
//! the walk promises is about reads that do not happen.

use std::collections::HashSet;
use std::sync::Mutex;

use futures_util::future::BoxFuture;

use super::super::{ParentFilter, RunSpec, SortKey, Source, walk};
use super::*;
use crate::commands::serve::graphql::paging::order::{Order, OrderDirection, Term};
use crate::commands::serve::graphql::paging::walk as paging;
use crate::commands::serve::testutil::state_with_agent_paths;
use crate::runstate::{RunStatus, create_run, with_isolated_runs_dir_async};

/// A predicate whose second half only a file could answer, keeping a tally of
/// every run it was asked to settle.
#[derive(Debug, Default)]
struct Deep {
    /// Runs whose cheap half already fails.
    drop_names: Vec<String>,
    /// Runs the file half says no to.
    deep_refuses: Vec<String>,
    /// Every run id confirmed, in the order they were.
    confirmed: Mutex<Vec<String>>,
}

impl Deep {
    fn confirmed(&self) -> Vec<String> {
        leviath_core::sync::lock(&self.confirmed).clone()
    }
}

impl RunPredicate for Deep {
    fn matches(&self, meta: &Arc<RunMeta>, _ctx: &MatchContext) -> bool {
        !self.drop_names.contains(&meta.agent_name)
    }

    fn verdict(&self, meta: &Arc<RunMeta>, ctx: &MatchContext) -> Verdict {
        match self.matches(meta, ctx) {
            false => Verdict::Drop,
            true => Verdict::NeedsIo,
        }
    }

    fn confirm<'a>(
        &'a self,
        meta: &'a Arc<RunMeta>,
        _ctx: &'a MatchContext,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            leviath_core::sync::lock(&self.confirmed).push(meta.run_id.clone());
            !self.deep_refuses.contains(&meta.run_id)
        })
    }

    fn digest_part(&self) -> String {
        "deep".to_string()
    }
}

/// A predicate that answers from the record alone, inheriting the defaults.
#[derive(Debug)]
struct Shallow;

impl RunPredicate for Shallow {
    fn matches(&self, meta: &Arc<RunMeta>, _ctx: &MatchContext) -> bool {
        meta.agent_name != "banned"
    }

    fn digest_part(&self) -> String {
        "shallow".to_string()
    }
}

fn meta(id: &str, agent: &str, started_at: i64) -> RunMeta {
    let mut meta = RunMeta::new(
        id.to_string(),
        agent.to_string(),
        "/agents/agent".to_string(),
        "task".to_string(),
        None,
        "/work".to_string(),
        1,
    );
    meta.started_at = started_at;
    meta.updated_at = started_at;
    meta
}

fn runs(count: usize) -> Vec<Arc<RunMeta>> {
    (0..count)
        .map(|at| {
            Arc::new(meta(
                &format!("run-{at:03}"),
                "agent",
                i64::try_from(at).expect("small"),
            ))
        })
        .collect()
}

/// A spec with every filter off, which the tests then narrow.
fn spec() -> RunSpec {
    RunSpec {
        limit: 2,
        cursor: None,
        statuses: Vec::new(),
        sort: SortKey::Started,
        descending: false,
        order: Order::new(vec![Term {
            field: SortKey::Started,
            direction: OrderDirection::Asc,
        }]),
        q: None,
        sources: Vec::new(),
        fields: None,
        ids: None,
        since: None,
        parent: ParentFilter::Any,
        blueprint: None,
        predicate: None,
        preloaded: None,
        digest: "abcd1234".to_string(),
    }
}

/// The same spec, newest first: both the primary key the record filters read
/// and the compiled order the walk compares on.
fn newest_first() -> RunSpec {
    let mut spec = spec();
    spec.descending = true;
    spec.order = Order::new(vec![Term {
        field: SortKey::Started,
        direction: OrderDirection::Desc,
    }]);
    spec
}

fn context() -> MatchContext {
    MatchContext::at(1_700_000_000)
}

fn sift_for(spec: &RunSpec) -> RunSift {
    RunSift::new(spec, HashSet::new(), context())
}

/// Run the five steps over `items` without going near an `AppState`.
async fn page_of(spec: &RunSpec, items: Vec<Arc<RunMeta>>) -> Page<RunSift> {
    paging::walk(sift_for(spec), items, spec.cursor.as_ref(), spec.limit).await
}

fn ids(page: &Page<RunSift>) -> Vec<String> {
    page.items.iter().map(|meta| meta.run_id.clone()).collect()
}

#[tokio::test]
async fn a_run_listing_pages_in_the_order_it_was_asked_for() {
    let page = page_of(&newest_first(), runs(4)).await;
    assert_eq!(ids(&page), vec!["run-003", "run-002"]);
    assert!(next_cursor(&page, "abcd1234").is_some());

    let oldest_first = spec();
    let page = page_of(&oldest_first, runs(4)).await;
    assert_eq!(ids(&page), vec!["run-000", "run-001"]);
}

/// The single-key cursor is the REST one, which is what lets a client walk a
/// listing across the two surfaces.
#[tokio::test]
async fn the_cursor_a_page_ends_on_is_the_one_rest_mints() {
    let page = page_of(&spec(), runs(4)).await;
    let minted = next_cursor(&page, "abcd1234").expect("another page follows");
    let rest = crate::commands::serve::cursor::encode(
        "started_at",
        "asc",
        "abcd1234",
        CursorKey::Int(1),
        "run-001",
    );
    assert_eq!(minted, rest);
    assert_eq!(
        next_cursor(&page_of(&spec(), runs(2)).await, "abcd1234"),
        None
    );
}

/// Every filter the REST listing applies from the record alone is applied here
/// without opening anything.
#[tokio::test]
async fn the_record_filters_all_answer_without_a_file() {
    let mut all = runs(4);
    all.push(Arc::new(meta("run-other", "other-agent", 9)));

    let mut by_blueprint = spec();
    by_blueprint.blueprint = Some("agent".to_string());
    by_blueprint.limit = 10;
    assert_eq!(page_of(&by_blueprint, all.clone()).await.items.len(), 4);

    let mut since = spec();
    since.since = Some(2);
    since.limit = 10;
    assert_eq!(
        ids(&page_of(&since, all.clone()).await),
        vec!["run-002", "run-003", "run-other"]
    );

    let mut roots_only = spec();
    roots_only.parent = ParentFilter::SubAgents;
    roots_only.limit = 10;
    assert_eq!(page_of(&roots_only, all.clone()).await.items.len(), 0);

    let mut by_status = spec();
    by_status.statuses = vec!["complete".to_string()];
    by_status.limit = 10;
    assert_eq!(page_of(&by_status, all.clone()).await.items.len(), 0);
    let mut finished = meta("run-done", "agent", 20);
    finished.status = RunStatus::Complete;
    let mut with_done = all;
    with_done.push(Arc::new(finished));
    assert_eq!(ids(&page_of(&by_status, with_done).await), vec!["run-done"]);
}

/// The promise of the seek, for runs: page two opens nothing for page one.
#[tokio::test]
async fn page_two_confirms_no_run_that_precedes_the_cursor() {
    let mut with_predicate = spec();
    with_predicate.limit = 3;
    with_predicate.predicate = Some(Arc::new(Deep::default()));

    let first = page_of(&with_predicate, runs(8)).await;
    assert_eq!(ids(&first), vec!["run-000", "run-001", "run-002"]);
    let raw = next_cursor(&first, "abcd1234").expect("another page follows");

    let mut second_page = spec();
    second_page.limit = 3;
    let counter = Arc::new(Deep::default());
    second_page.predicate = Some(Arc::clone(&counter) as Arc<dyn RunPredicate>);
    second_page.cursor = Some(
        sift_for(&second_page)
            .order()
            .decode(&raw, "abcd1234")
            .expect("the cursor resumes this walk"),
    );

    let second = page_of(&second_page, runs(8)).await;
    assert_eq!(ids(&second), vec!["run-003", "run-004", "run-005"]);
    let read = counter.confirmed();
    assert!(!read.iter().any(|id| id.as_str() < "run-003"));
    assert_eq!(read.first().map(String::as_str), Some("run-003"));
}

/// A filter that reads nothing reads nothing, count included.
#[tokio::test]
async fn a_record_only_listing_never_confirms_and_still_counts() {
    let mut page = page_of(&spec(), runs(5)).await;
    assert_eq!(page.total().await, 5);
}

/// Search over the parsed record is settled where it is found; only a source
/// that lives in a file makes a run wait.
#[tokio::test]
async fn a_search_of_the_record_is_settled_without_a_file() {
    let mut by_meta = spec();
    by_meta.limit = 10;
    by_meta.q = Some("needle".to_string());
    by_meta.sources = vec![Source::Meta];
    let sift = sift_for(&by_meta);
    assert_eq!(
        sift.test(&Arc::new(meta("run-a", "needle", 1))),
        Verdict::Keep
    );
    // Nothing in the record matches, and no source can look anywhere else.
    assert_eq!(
        sift.test(&Arc::new(meta("run-b", "agent", 1))),
        Verdict::Drop
    );

    let mut deep = spec();
    deep.q = Some("needle".to_string());
    deep.sources = vec![Source::Meta, Source::Context];
    // The record says nothing, so this one waits for the file half.
    assert_eq!(
        sift_for(&deep).test(&Arc::new(meta("run-b", "agent", 1))),
        Verdict::NeedsIo
    );
    // The record already matched, so the file half is never reached.
    assert_eq!(
        sift_for(&deep).test(&Arc::new(meta("run-c", "needle", 1))),
        Verdict::Keep
    );
}

/// A search that only a file can answer is just another confirmation, so it
/// has no scan budget to run out of.
#[tokio::test]
async fn a_file_backed_search_settles_at_step_four() {
    with_isolated_runs_dir_async("runs-lazy-search", |_dir| async move {
        for at in 0..3 {
            create_run(&meta(&format!("run-{at:03}"), "agent", at)).expect("created");
        }
        std::fs::write(
            crate::runstate::run_dir("run-001").join(leviath_core::files::CONTEXT_FILE),
            "the needle is in this run's window",
        )
        .expect("wrote a context window");

        let mut searching = spec();
        searching.limit = 10;
        searching.q = Some("needle".to_string());
        searching.sources = vec![Source::Context];
        let page = page_of(&searching, runs(3)).await;
        assert_eq!(ids(&page), vec!["run-001"]);
    })
    .await;
}

/// A predicate that reads nothing and a search that has to are two separate
/// halves of one answer: the predicate is settled at step one and only the
/// search waits.
#[tokio::test]
async fn a_record_only_predicate_beside_a_file_backed_search() {
    with_isolated_runs_dir_async("runs-lazy-mixed", |_dir| async move {
        create_run(&meta("run-000", "agent", 0)).expect("created");
        create_run(&meta("run-001", "agent", 1)).expect("created");
        std::fs::write(
            crate::runstate::run_dir("run-001").join(leviath_core::files::CONTEXT_FILE),
            "the needle is in this run's window",
        )
        .expect("wrote a context window");

        let mut mixed = spec();
        mixed.limit = 10;
        mixed.q = Some("needle".to_string());
        mixed.sources = vec![Source::Meta, Source::Context];
        // Its `verdict` is the default, so it answers from the record alone.
        mixed.predicate = Some(Arc::new(Shallow));

        let sift = sift_for(&mixed);
        // The record says nothing, so the search half waits.
        assert_eq!(
            sift.test(&Arc::new(meta("run-001", "agent", 1))),
            Verdict::NeedsIo
        );
        // The record matched, so nothing is left to settle.
        assert_eq!(
            sift.test(&Arc::new(meta("run-002", "needle", 2))),
            Verdict::Keep
        );

        let page = page_of(&mixed, runs(2)).await;
        assert_eq!(ids(&page), vec!["run-001"]);
    })
    .await;
}

/// A run the file half refuses does not take a slot; the page fills from
/// behind it rather than coming back short.
#[tokio::test]
async fn a_run_a_file_refuses_does_not_take_a_slot() {
    let mut with_predicate = spec();
    with_predicate.predicate = Some(Arc::new(Deep {
        deep_refuses: vec!["run-001".to_string()],
        ..Deep::default()
    }));
    let page = page_of(&with_predicate, runs(5)).await;
    assert_eq!(ids(&page), vec!["run-000", "run-002"]);
}

/// The cheap half drops a run before anything is read for it.
#[tokio::test]
async fn the_cheap_half_of_a_predicate_drops_a_run_unread() {
    let counter = Arc::new(Deep {
        drop_names: vec!["other-agent".to_string()],
        ..Deep::default()
    });
    let mut with_predicate = spec();
    with_predicate.limit = 10;
    with_predicate.predicate = Some(Arc::clone(&counter) as Arc<dyn RunPredicate>);

    let mut all = runs(2);
    all.push(Arc::new(meta("run-other", "other-agent", 9)));
    let page = page_of(&with_predicate, all).await;
    assert_eq!(ids(&page), vec!["run-000", "run-001"]);
    assert!(!counter.confirmed().contains(&"run-other".to_string()));
}

/// The whole listing, not what is left of it, and only what the page left
/// unsettled is read for it.
#[tokio::test]
async fn a_total_counts_the_listing_and_reuses_the_pages_reads() {
    let counter = Arc::new(Deep {
        deep_refuses: vec!["run-004".to_string()],
        ..Deep::default()
    });
    let mut with_predicate = spec();
    with_predicate.predicate = Some(Arc::clone(&counter) as Arc<dyn RunPredicate>);

    let mut page = page_of(&with_predicate, runs(10)).await;
    assert_eq!(page.total().await, 9);
    let read = counter.confirmed();
    assert_eq!(read.len(), 10);
    let mut once = read.clone();
    once.sort();
    once.dedup();
    assert_eq!(once.len(), 10);
}

/// The wrapper reads the shared index, which is what the resolver calls.
#[tokio::test]
async fn the_walk_reads_the_run_index() {
    with_isolated_runs_dir_async("runs-lazy-index", |dir| async move {
        for at in 0..3 {
            create_run(&meta(&format!("run-{at:03}"), "agent", at)).expect("created");
        }
        let state = state_with_agent_paths(vec![dir.join("agents")]);
        let ordered = newest_first();
        let mut page = walk(&state, &ordered).await;
        assert_eq!(ids(&page), vec!["run-002", "run-001"]);
        assert_eq!(page.total().await, 3);
        assert!(next_cursor(&page, &ordered.digest).is_some());
    })
    .await;
}

/// A subtree filter is resolved from the index's parent map before the walk,
/// because a grandchild names its parent and not its ancestor.
#[tokio::test]
async fn a_subtree_filter_is_resolved_from_the_index() {
    with_isolated_runs_dir_async("runs-lazy-subtree", |dir| async move {
        create_run(&meta("root", "agent", 1)).expect("created");
        let mut worker = meta("worker", "agent", 2);
        worker.parent_run_id = Some("root".to_string());
        create_run(&worker).expect("created");
        let mut grandchild = meta("grandchild", "agent", 3);
        grandchild.parent_run_id = Some("worker".to_string());
        create_run(&grandchild).expect("created");

        let state = state_with_agent_paths(vec![dir.join("agents")]);
        let mut under = spec();
        under.limit = 10;
        under.parent = ParentFilter::Under("root".to_string());
        let page = walk(&state, &under).await;
        assert_eq!(ids(&page), vec!["worker", "grandchild"]);
    })
    .await;
}
