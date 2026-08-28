//! Agent hierarchy tree building and endpoints.

use axum::extract::Path as AxumPath;
use axum::http::StatusCode;
use axum::response::Json;

use super::types::*;
use crate::runstate::{self, RunMeta};

pub(super) fn build_tree(runs: &[RunMeta], parent_id: Option<&str>) -> Vec<AgentTreeNode> {
    runs.iter()
        .filter(|r| r.parent_run_id.as_deref() == parent_id)
        .map(|r| {
            let children = build_tree(runs, Some(&r.run_id));
            AgentTreeNode {
                run_id: r.run_id.clone(),
                agent_name: r.agent_name.clone(),
                status: r.status.wire().to_string(),
                stage: r.current_stage.clone(),
                iteration: r.iteration,
                prompt_tokens: r.prompt_tokens,
                completion_tokens: r.completion_tokens,
                cost_usd: r.cost_usd,
                subtree_cost_usd: subtree_spend(
                    r.cost_usd,
                    r.unpriced_calls,
                    &children
                        .iter()
                        .map(|c| (c.subtree_cost_usd, 0))
                        .collect::<Vec<_>>(),
                )
                .0,
                children,
            }
        })
        .collect()
}

/// A subtree's spend: this run plus everything under it, and how much of it
/// could not be priced.
///
/// `None` when anything in the subtree is unpriced. Adding up only the priced
/// part would produce a number that looks like the answer and is smaller than
/// the truth by an amount nobody can see, which is the one failure worse than
/// having no number.
fn subtree_spend(
    own: Option<f64>,
    own_unpriced: usize,
    below: &[(Option<f64>, usize)],
) -> (Option<f64>, usize) {
    let unpriced = own_unpriced + below.iter().map(|(_, u)| u).sum::<usize>();
    // `Option` sums to `None` the moment one term is `None`, which is exactly
    // the rule wanted here and leaves no branch to get wrong.
    let total: Option<f64> = below
        .iter()
        .map(|(cost, _)| *cost)
        .chain(std::iter::once(own))
        .sum();
    (total.filter(|_| unpriced == 0), unpriced)
}

pub(super) fn build_tree_status(runs: &[RunMeta], parent_id: Option<&str>) -> Vec<TreeStatusNode> {
    runs.iter()
        .filter(|r| r.parent_run_id.as_deref() == parent_id)
        .map(|r| {
            let children = build_tree_status(runs, Some(&r.run_id));
            let subtree_prompt: usize = r.prompt_tokens
                + children
                    .iter()
                    .map(|c| c.subtree_prompt_tokens)
                    .sum::<usize>();
            let subtree_completion: usize = r.completion_tokens
                + children
                    .iter()
                    .map(|c| c.subtree_completion_tokens)
                    .sum::<usize>();
            let below: Vec<(Option<f64>, usize)> = children
                .iter()
                .map(|c| (c.subtree_cost_usd, c.subtree_unpriced_calls))
                .collect();
            let spend = subtree_spend(r.cost_usd, r.unpriced_calls, &below);
            TreeStatusNode {
                run_id: r.run_id.clone(),
                agent_name: r.agent_name.clone(),
                status: r.status.wire().to_string(),
                stage: r.current_stage.clone(),
                prompt_tokens: r.prompt_tokens,
                completion_tokens: r.completion_tokens,
                subtree_prompt_tokens: subtree_prompt,
                subtree_completion_tokens: subtree_completion,
                cost_usd: r.cost_usd,
                subtree_cost_usd: spend.0,
                subtree_unpriced_calls: spend.1,
                children,
            }
        })
        .collect()
}

pub(super) async fn agents_tree() -> Json<Vec<AgentTreeNode>> {
    let runs = runstate::list_runs();
    let tree = build_tree(&runs, None);
    Json(tree)
}

pub(super) async fn agent_tree_status(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<TreeStatusNode>, ApiError> {
    let runs = runstate::list_runs();
    let root = runs.iter().find(|r| r.run_id == id).ok_or((
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: format!("Agent run '{}' not found", id),
        }),
    ))?;

    let children = build_tree_status(&runs, Some(&id));
    let subtree_prompt: usize = root.prompt_tokens
        + children
            .iter()
            .map(|c| c.subtree_prompt_tokens)
            .sum::<usize>();
    let subtree_completion: usize = root.completion_tokens
        + children
            .iter()
            .map(|c| c.subtree_completion_tokens)
            .sum::<usize>();

    let below: Vec<(Option<f64>, usize)> = children
        .iter()
        .map(|c| (c.subtree_cost_usd, c.subtree_unpriced_calls))
        .collect();
    let spend = subtree_spend(root.cost_usd, root.unpriced_calls, &below);

    Ok(Json(TreeStatusNode {
        run_id: root.run_id.clone(),
        agent_name: root.agent_name.clone(),
        status: root.status.wire().to_string(),
        stage: root.current_stage.clone(),
        prompt_tokens: root.prompt_tokens,
        completion_tokens: root.completion_tokens,
        subtree_prompt_tokens: subtree_prompt,
        subtree_completion_tokens: subtree_completion,
        cost_usd: root.cost_usd,
        subtree_cost_usd: spend.0,
        subtree_unpriced_calls: spend.1,
        children,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let tree = build_tree(&runs, None);
        assert!(tree.is_empty());
    }

    #[test]
    fn build_tree_single_root() {
        let runs = vec![make_meta("run-1", "agent-a", None)];
        let tree = build_tree(&runs, None);
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
        let tree = build_tree(&runs, None);
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
        let tree = build_tree(&runs, None);
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
        let tree = build_tree(&runs, None);
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
        let runs = vec![waiting, interactive];

        let tree = build_tree(&runs, None);
        assert_eq!(tree[0].status, "waiting_input");
        assert_eq!(tree[0].children[0].status, "complete_interactive");

        let tree = build_tree_status(&runs, None);
        assert_eq!(tree[0].status, "waiting_input");
        assert_eq!(tree[0].children[0].status, "complete_interactive");
    }

    #[test]
    fn build_tree_status_empty() {
        let runs: Vec<RunMeta> = vec![];
        let tree = build_tree_status(&runs, None);
        assert!(tree.is_empty());
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
        let tree = build_tree_status(&runs, None);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].subtree_prompt_tokens, 110); // 10 + 100
        assert_eq!(tree[0].subtree_completion_tokens, 55); // 5 + 50
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
        let tree = build_tree_status(&runs, None);
        assert_eq!(tree[0].subtree_prompt_tokens, 60); // 10 + 20 + 30
    }

    #[test]
    fn build_tree_from_subtree() {
        let runs = vec![
            make_meta("r", "a", None),
            make_meta("c1", "b", Some("r")),
            make_meta("c2", "c", Some("r")),
        ];
        // Build from a specific parent
        let subtree = build_tree(&runs, Some("r"));
        assert_eq!(subtree.len(), 2);
    }

    // ─── agents_tree / agent_tree_status (real runstate, unique run ids) ────
    //
    // `runstate::list_runs()` reads the real on-disk runs directory (there's
    // no test-isolated override in this crate's test suite), so these tests
    // use unique run-id prefixes and only assert on their own entries, then
    // clean up afterward.

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

                let Json(tree) = agents_tree().await;
                assert!(tree.iter().any(|n| n.run_id == run_id));
            },
        )
        .await;
    }

    #[tokio::test]
    async fn agent_tree_status_returns_not_found_for_missing_id() {
        let (status, Json(body)) =
            agent_tree_status(AxumPath("definitely-not-a-real-run-id".to_string()))
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

                let Json(node) = agent_tree_status(AxumPath(run_id.to_string()))
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
}
