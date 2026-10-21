//! Agent hierarchy tree building and endpoints.
//!
//! Every subtree figure the server reports, REST and GraphQL alike, comes from
//! the one walk here, so the two surfaces add a fan-out up the same way.

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::run_index::RunSnapshot;
use super::types::*;
use crate::runstate::RunMeta;

/// What a run and everything under it add up to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Rollup {
    pub(super) prompt_tokens: usize,
    pub(super) completion_tokens: usize,
    pub(super) cached_tokens: usize,
    pub(super) cache_write_tokens: usize,
    /// The subtree's spend. `None` when anything in it is unpriced: adding up
    /// only the priced part would produce a number that looks like the answer
    /// and is smaller than the truth by an amount nobody can see, which is the
    /// one failure worse than having no number.
    pub(super) cost_usd: Option<f64>,
    /// The priced part, kept while `cost_usd` is `None`.
    pub(super) cost_priced_usd: f64,
    /// True only when every run in the subtree carried the provider's own
    /// figures.
    pub(super) cost_is_exact: bool,
    /// Calls in the subtree no provider priced.
    pub(super) unpriced_calls: usize,
    /// Each run's working time added up. Agent-seconds, so two workers busy
    /// for a minute side by side count two minutes.
    pub(super) working_secs: u64,
    /// How many runs in the subtree have their working clock running.
    pub(super) working_now: usize,
    /// The deepest nesting below the run.
    pub(super) depth: usize,
    /// How many runs are below it, at any depth.
    pub(super) descendants: usize,
}

impl Rollup {
    /// One run's own figures, as if nothing hung off it.
    fn own(meta: &RunMeta, now: i64) -> Self {
        Self {
            prompt_tokens: meta.prompt_tokens,
            completion_tokens: meta.completion_tokens,
            cached_tokens: meta.cached_tokens,
            cache_write_tokens: meta.cache_write_tokens,
            cost_usd: meta.cost_usd.filter(|_| meta.unpriced_calls == 0),
            cost_priced_usd: meta.cost_priced_usd,
            cost_is_exact: meta.cost_is_exact,
            unpriced_calls: meta.unpriced_calls,
            working_secs: meta.active_runtime_secs(now),
            working_now: usize::from(meta.active.is_some_and(|clock| clock.since.is_some())),
            depth: 0,
            descendants: 0,
        }
    }

    /// Fold one child's whole subtree into this one.
    fn absorb(&mut self, below: &Rollup) {
        self.prompt_tokens += below.prompt_tokens;
        self.completion_tokens += below.completion_tokens;
        self.cached_tokens += below.cached_tokens;
        self.cache_write_tokens += below.cache_write_tokens;
        // Both sides are already `None` wherever something is unpriced, so the
        // sum is `None` the moment either is, which leaves no branch to get
        // wrong.
        self.cost_usd = self.cost_usd.zip(below.cost_usd).map(|(a, b)| a + b);
        self.cost_priced_usd += below.cost_priced_usd;
        self.cost_is_exact &= below.cost_is_exact;
        self.unpriced_calls += below.unpriced_calls;
        self.working_secs += below.working_secs;
        self.working_now += below.working_now;
        self.depth = self.depth.max(below.depth + 1);
        self.descendants += below.descendants + 1;
    }
}

/// Walk the subtree under `root` once, bottom up.
///
/// `visit` is handed each run with its own subtree's roll-up and whatever the
/// visits of its children returned, so one walk builds a nested tree and its
/// totals together. A run already walked is skipped: a record that names its
/// own descendant as its parent is counted once rather than walked until the
/// stack runs out.
pub(super) fn walk<T>(
    snapshot: &RunSnapshot,
    root: &Arc<RunMeta>,
    now: i64,
    visit: &mut impl FnMut(&Arc<RunMeta>, &Rollup, Vec<T>) -> T,
) -> (Rollup, T) {
    let mut seen = HashSet::from([root.run_id.clone()]);
    walk_from(snapshot, root, now, &mut seen, visit)
}

fn walk_from<T>(
    snapshot: &RunSnapshot,
    run: &Arc<RunMeta>,
    now: i64,
    seen: &mut HashSet<String>,
    visit: &mut impl FnMut(&Arc<RunMeta>, &Rollup, Vec<T>) -> T,
) -> (Rollup, T) {
    let mut rollup = Rollup::own(run, now);
    let mut children = Vec::new();
    for child in snapshot.under(Some(&run.run_id)) {
        if seen.insert(child.run_id.clone()) {
            let (below, node) = walk_from(snapshot, child, now, seen, visit);
            rollup.absorb(&below);
            children.push(node);
        }
    }
    let node = visit(run, &rollup, children);
    (rollup, node)
}

/// The runs under `parent_id` (the roots when `None`) as a tree, each node
/// carrying its subtree's spend. A walk of the snapshot's parent map, so a
/// thousand runs cost a thousand node visits rather than a scan per level.
pub(super) fn build_tree(
    snapshot: &RunSnapshot,
    parent_id: Option<&str>,
    now: i64,
) -> Vec<AgentTreeNode> {
    let mut node = |r: &Arc<RunMeta>, rollup: &Rollup, children| AgentTreeNode {
        run_id: r.run_id.clone(),
        agent_name: r.agent_name.clone(),
        status: r.status.wire().to_string(),
        stage: r.current_stage.clone(),
        iteration: r.iteration,
        prompt_tokens: r.prompt_tokens,
        completion_tokens: r.completion_tokens,
        cost_usd: r.cost_usd,
        subtree_cost_usd: rollup.cost_usd,
        children,
    };
    snapshot
        .under(parent_id)
        .map(|r| walk(snapshot, r, now, &mut node).1)
        .collect()
}

/// `root` and everything under it, each node carrying its subtree's totals.
pub(super) fn build_tree_status(
    snapshot: &RunSnapshot,
    root: &Arc<RunMeta>,
    now: i64,
) -> TreeStatusNode {
    let mut node = |r: &Arc<RunMeta>, rollup: &Rollup, children| TreeStatusNode {
        run_id: r.run_id.clone(),
        agent_name: r.agent_name.clone(),
        status: r.status.wire().to_string(),
        stage: r.current_stage.clone(),
        prompt_tokens: r.prompt_tokens,
        completion_tokens: r.completion_tokens,
        subtree_prompt_tokens: rollup.prompt_tokens,
        subtree_completion_tokens: rollup.completion_tokens,
        cost_usd: r.cost_usd,
        subtree_cost_usd: rollup.cost_usd,
        subtree_unpriced_calls: rollup.unpriced_calls,
        subtree_working_secs: rollup.working_secs,
        children,
    };
    walk(snapshot, root, now, &mut node).1
}

pub(super) async fn runs_tree(State(state): State<AppState>) -> Json<Vec<AgentTreeNode>> {
    let snapshot = state.caches.run_index.snapshot().await;
    let tree = build_tree(&snapshot, None, leviath_core::duration::now_secs());
    Json(tree)
}

pub(super) async fn run_tree_status(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<TreeStatusNode>, ApiError> {
    let snapshot = state.caches.run_index.snapshot().await;
    let root = snapshot.get(&id).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: format!("Run '{}' not found", id),
        }),
    ))?;
    Ok(Json(build_tree_status(
        &snapshot,
        root,
        leviath_core::duration::now_secs(),
    )))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::runstate::{self, RunMeta};

    fn snap(runs: Vec<RunMeta>) -> RunSnapshot {
        RunSnapshot::new(runs.into_iter().map(Arc::new).collect())
    }

    fn test_state() -> AppState {
        crate::commands::serve::testutil::state_with_agent_paths(Vec::new())
    }

    fn clock(banked_secs: u64, since: Option<i64>) -> leviath_core::run_meta::ActiveClock {
        leviath_core::run_meta::ActiveClock { banked_secs, since }
    }

    /// The tree-status of one run in `snapshot`, at second zero.
    fn status_of(snapshot: &RunSnapshot, id: &str) -> TreeStatusNode {
        build_tree_status(snapshot, snapshot.get(id).expect("run in snapshot"), 0)
    }

    fn make_meta(id: &str, name: &str, parent: Option<&str>) -> RunMeta {
        let mut meta = RunMeta::new(
            id.to_string(),
            name.to_string(),
            "/path".to_string(),
            "task".to_string(),
            None,
            "/work".to_string(),
            1,
        );
        meta.parent_run_id = parent.map(|s| s.to_string());
        meta
    }

    #[test]
    fn build_tree_empty() {
        let runs: Vec<RunMeta> = vec![];
        let tree = build_tree(&snap(runs), None, 0);
        assert!(tree.is_empty());
    }

    #[test]
    fn build_tree_single_root() {
        let runs = vec![make_meta("run-1", "agent-a", None)];
        let tree = build_tree(&snap(runs), None, 0);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].run_id, "run-1");
        assert!(tree[0].children.is_empty());
    }

    #[test]
    fn build_tree_parent_child() {
        let runs = vec![
            make_meta("parent", "agent-a", None),
            make_meta("child", "agent-b", Some("parent")),
        ];
        let tree = build_tree(&snap(runs), None, 0);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].run_id, "parent");
        assert_eq!(tree[0].children.len(), 1);
        assert_eq!(tree[0].children[0].run_id, "child");
    }

    #[test]
    fn build_tree_multiple_roots() {
        let runs = vec![
            make_meta("root-1", "a", None),
            make_meta("root-2", "b", None),
        ];
        let tree = build_tree(&snap(runs), None, 0);
        assert_eq!(tree.len(), 2);
    }

    #[test]
    fn build_tree_nested() {
        let runs = vec![
            make_meta("r", "a", None),
            make_meta("c1", "b", Some("r")),
            make_meta("c2", "c", Some("r")),
            make_meta("gc", "d", Some("c1")),
        ];
        let tree = build_tree(&snap(runs), None, 0);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].children.len(), 2);
        let c1 = &tree[0].children[0];
        assert_eq!(c1.run_id, "c1");
        assert_eq!(c1.children.len(), 1);
        assert_eq!(c1.children[0].run_id, "gc");
    }

    /// The tree routes rendered a status through `Display`, which is
    /// PascalCase and squashes the two multi-word states into `WaitingInput`
    /// and `CompleteInteractive` - words no other route on this server sends.
    /// A client walking a tree and then fetching one of its runs got two
    /// different spellings of the one state.
    #[test]
    fn a_tree_node_spells_its_status_the_way_a_run_does() {
        let mut waiting = make_meta("p", "a", None);
        waiting.status = crate::runstate::RunStatus::WaitingInput;
        let mut interactive = make_meta("c", "b", Some("p"));
        interactive.status = crate::runstate::RunStatus::CompleteInteractive;
        let runs = snap(vec![waiting, interactive]);

        let tree = build_tree(&runs, None, 0);
        assert_eq!(tree[0].status, "waiting_input");
        assert_eq!(tree[0].children[0].status, "complete_interactive");

        let node = status_of(&runs, "p");
        assert_eq!(node.status, "waiting_input");
        assert_eq!(node.children[0].status, "complete_interactive");
    }

    #[test]
    fn a_leaf_is_its_own_subtree() {
        let mut leaf = make_meta("l", "a", None);
        leaf.prompt_tokens = 4;
        leaf.active = Some(clock(9, None));
        let node = status_of(&snap(vec![leaf]), "l");
        assert!(node.children.is_empty());
        assert_eq!(node.subtree_prompt_tokens, 4);
        assert_eq!(node.subtree_working_secs, 9);
    }

    #[test]
    fn build_tree_status_aggregates_tokens() {
        let mut parent = make_meta("p", "a", None);
        parent.prompt_tokens = 10;
        parent.completion_tokens = 5;
        let mut child = make_meta("c", "b", Some("p"));
        child.prompt_tokens = 100;
        child.completion_tokens = 50;
        let runs = vec![parent, child];
        let node = status_of(&snap(runs), "p");
        assert_eq!(node.subtree_prompt_tokens, 110); // 10 + 100
        assert_eq!(node.subtree_completion_tokens, 55); // 5 + 50
    }

    #[test]
    fn build_tree_status_deep_aggregation() {
        let mut root = make_meta("r", "a", None);
        root.prompt_tokens = 10;
        let mut child = make_meta("c", "b", Some("r"));
        child.prompt_tokens = 20;
        let mut grandchild = make_meta("gc", "c", Some("c"));
        grandchild.prompt_tokens = 30;
        let runs = vec![root, child, grandchild];
        let node = status_of(&snap(runs), "r");
        assert_eq!(node.subtree_prompt_tokens, 60); // 10 + 20 + 30
    }

    /// Each node's working time is its own plus everything under it, read at
    /// the one instant the request was answered.
    #[test]
    fn build_tree_status_adds_up_working_time() {
        let mut root = make_meta("r", "a", None);
        root.active = Some(clock(30, None));
        let mut busy = make_meta("c", "b", Some("r"));
        busy.active = Some(clock(5, Some(960)));
        let mut helper = make_meta("gc", "c", Some("c"));
        helper.active = Some(clock(1, Some(990)));
        let runs = snap(vec![root, busy, helper]);
        let node = build_tree_status(&runs, runs.get("r").unwrap(), 1_000);
        assert_eq!(node.subtree_working_secs, 30 + 45 + 11);
        assert_eq!(node.children[0].subtree_working_secs, 45 + 11);
        assert_eq!(node.children[0].children[0].subtree_working_secs, 11);
    }

    /// The subtree's spend is null once anything in it is unpriced, on every
    /// node above the unpriced run and on none beside it.
    #[test]
    fn an_unpriced_run_makes_only_the_subtrees_above_it_unknown() {
        let mut root = make_meta("r", "a", None);
        root.cost_usd = Some(0.5);
        let mut priced = make_meta("p", "b", Some("r"));
        priced.cost_usd = Some(0.25);
        let mut unpriced = make_meta("u", "c", Some("r"));
        unpriced.cost_usd = None;
        unpriced.unpriced_calls = 1;
        let runs = snap(vec![root, priced, unpriced]);

        let node = status_of(&runs, "r");
        assert_eq!(node.subtree_cost_usd, None);
        assert_eq!(node.subtree_unpriced_calls, 1);
        let beside = node.children.iter().find(|c| c.run_id == "p").unwrap();
        assert_eq!(beside.subtree_cost_usd, Some(0.25));

        let tree = build_tree(&runs, None, 0);
        assert_eq!(tree[0].subtree_cost_usd, None);
        let p = tree[0].children.iter().find(|c| c.run_id == "p").unwrap();
        assert_eq!(p.subtree_cost_usd, Some(0.25));
    }

    /// A run whose own figure is missing is unknown even with no unpriced
    /// call counted, and one whose calls went unpriced is unknown even with a
    /// figure: either makes the whole subtree's spend unknown.
    #[test]
    fn either_sign_of_an_unpriced_run_makes_the_spend_unknown() {
        let mut no_figure = make_meta("a", "a", None);
        no_figure.cost_usd = None;
        assert_eq!(
            status_of(&snap(vec![no_figure]), "a").subtree_cost_usd,
            None
        );

        let mut stale_figure = make_meta("b", "a", None);
        stale_figure.cost_usd = Some(1.0);
        stale_figure.unpriced_calls = 1;
        assert_eq!(
            status_of(&snap(vec![stale_figure]), "b").subtree_cost_usd,
            None
        );
    }

    #[test]
    fn build_tree_from_subtree() {
        let runs = vec![
            make_meta("r", "a", None),
            make_meta("c1", "b", Some("r")),
            make_meta("c2", "c", Some("r")),
        ];
        let subtree = build_tree(&snap(runs), Some("r"), 0);
        assert_eq!(subtree.len(), 2);
    }

    // ─── runs_tree / run_tree_status (real runstate, unique run ids) ────
    //
    // These read a runs directory the test isolates, with unique run ids and
    // assertions only on their own entries, and clean up afterward.

    struct RunCleanup<'a>(&'a [&'a str]);
    impl Drop for RunCleanup<'_> {
        fn drop(&mut self) {
            for id in self.0 {
                let _ = std::fs::remove_dir_all(runstate::run_dir(id));
            }
        }
    }

    #[tokio::test]
    async fn agents_tree_includes_created_run() {
        crate::runstate::with_isolated_runs_dir_async(
            "agents_tree_includes_created_run",
            |_d| async move {
                let run_id = "test-tree-agents-tree-1";
                let _cleanup = RunCleanup(&[run_id]);
                let meta = make_meta(run_id, "agent-tree-test", None);
                runstate::create_run(&meta).unwrap();

                let Json(tree) = runs_tree(State(test_state())).await;
                assert!(tree.iter().any(|n| n.run_id == run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_tree_status_returns_not_found_for_missing_id() {
        let (status, Json(body)) = run_tree_status(
            State(test_state()),
            AxumPath("definitely-not-a-real-run-id".to_string()),
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.error.contains("definitely-not-a-real-run-id"));
    }

    #[tokio::test]
    async fn agent_tree_status_returns_tree_with_subtree_totals() {
        crate::runstate::with_isolated_runs_dir_async(
            "agent_tree_status_returns_tree_with_subtree_totals",
            |_d| async move {
                let run_id = "test-tree-status-root";
                let child_id = "test-tree-status-child";
                let _cleanup = RunCleanup(&[run_id, child_id]);

                let mut root = make_meta(run_id, "root-agent", None);
                root.prompt_tokens = 10;
                root.completion_tokens = 2;
                runstate::create_run(&root).unwrap();

                let mut child = make_meta(child_id, "child-agent", Some(run_id));
                child.prompt_tokens = 20;
                child.completion_tokens = 3;
                runstate::create_run(&child).unwrap();

                let Json(node) = run_tree_status(State(test_state()), AxumPath(run_id.to_string()))
                    .await
                    .unwrap();
                assert_eq!(node.run_id, run_id);
                assert_eq!(node.subtree_prompt_tokens, 30);
                assert_eq!(node.subtree_completion_tokens, 5);
                assert_eq!(node.children.len(), 1);
                assert_eq!(node.children[0].run_id, child_id);
            },
        )
        .await;
    }

    /// A record that names itself, or its own child, as its parent is walked
    /// once rather than until the stack runs out.
    #[tokio::test]
    async fn a_parent_loop_is_walked_once() {
        crate::runstate::with_isolated_runs_dir_async("tree_status_parent_loop", |_d| async move {
            let ids = ["test-tree-loop-a", "test-tree-loop-b"];
            let _cleanup = RunCleanup(&ids);
            let mut a = make_meta(ids[0], "a", Some(ids[1]));
            a.prompt_tokens = 1;
            runstate::create_run(&a).unwrap();
            let mut b = make_meta(ids[1], "b", Some(ids[0]));
            b.prompt_tokens = 2;
            runstate::create_run(&b).unwrap();

            let Json(node) = run_tree_status(State(test_state()), AxumPath(ids[0].to_string()))
                .await
                .unwrap();
            assert_eq!(node.subtree_prompt_tokens, 3);
            assert_eq!(node.children.len(), 1);
            assert!(
                node.children[0].children.is_empty(),
                "a is not under b again"
            );
        })
        .await;
    }
}
