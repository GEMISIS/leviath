//! One run listing for every read route.
//!
//! `GET /api/runs`, `GET /api/agents`, the tree routes and a run's children all
//! start from "every run on disk". Reading that fresh per request means a
//! `read_dir` and a parse of every `meta.json` to serve a page of fifty, and
//! the reads hold a runtime worker thread while they happen. This index keeps
//! one [`StatCache`] for the process, so a request pays a stat per run (one
//! per second for a finished run) and parses only what changed, and it does
//! that work on the blocking pool.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::runstate::{self, RunMeta, StatCache};

/// The shared parse cache over the runs directory, and its listing. Cloning
/// shares them; a fresh one starts empty and fills on its first read.
#[derive(Clone, Default)]
pub(super) struct RunIndex {
    cache: Arc<Mutex<(StatCache<RunMeta>, runstate::RunDirListing)>>,
}

/// Every run as of one read of the runs directory, newest first, with the
/// parent-to-children map built once so a tree is a walk rather than a re-scan
/// of the whole list per level.
pub(super) struct RunSnapshot {
    runs: Vec<Arc<RunMeta>>,
    by_id: HashMap<String, usize>,
    children: HashMap<String, Vec<usize>>,
    roots: Vec<usize>,
}

impl RunSnapshot {
    pub(super) fn new(runs: Vec<Arc<RunMeta>>) -> Self {
        let mut by_id = HashMap::with_capacity(runs.len());
        let mut children: HashMap<String, Vec<usize>> = HashMap::new();
        let mut roots = Vec::new();
        for (at, meta) in runs.iter().enumerate() {
            by_id.insert(meta.run_id.clone(), at);
            match &meta.parent_run_id {
                Some(parent) => children.entry(parent.clone()).or_default().push(at),
                None => roots.push(at),
            }
        }
        Self {
            runs,
            by_id,
            children,
            roots,
        }
    }

    /// Every run, newest first, for a caller that goes on to filter and sort
    /// the list itself.
    pub(super) fn into_runs(self) -> Vec<Arc<RunMeta>> {
        self.runs
    }

    pub(super) fn get(&self, run_id: &str) -> Option<&Arc<RunMeta>> {
        self.by_id.get(run_id).and_then(|&at| self.runs.get(at))
    }

    /// Every run under `root`, at any depth, by id. The root itself is not in it.
    ///
    /// One walk of the parent map rather than a pass per level: a fan-out that
    /// fans out again is still one traversal, and a record that somehow names
    /// itself as an ancestor cannot spin it, because a run already seen is not
    /// walked twice.
    pub(super) fn descendants_of(&self, root: &str) -> std::collections::HashSet<String> {
        let mut found = std::collections::HashSet::new();
        let mut frontier = vec![root.to_string()];
        while let Some(parent) = frontier.pop() {
            for at in self.children.get(&parent).into_iter().flatten() {
                if let Some(child) = self.runs.get(*at)
                    && found.insert(child.run_id.clone())
                {
                    frontier.push(child.run_id.clone());
                }
            }
        }
        found
    }

    /// The runs directly under `parent`, or the roots when `parent` is `None`,
    /// in listing order. A parent nothing hangs off yields nothing.
    pub(super) fn under(&self, parent: Option<&str>) -> impl Iterator<Item = &Arc<RunMeta>> {
        let indices = match parent {
            None => Some(&self.roots),
            Some(id) => self.children.get(id),
        };
        indices
            .into_iter()
            .flatten()
            .filter_map(|&at| self.runs.get(at))
    }
}

impl RunIndex {
    /// The runs on disk right now, read through the cache on the blocking pool.
    ///
    /// The lock is taken inside the blocking task and released before it
    /// returns, so it is never held across an await. Requests that arrive
    /// together take turns; a warm pass over a thousand settled runs is a stat
    /// of the runs directory and one per live run, so the turn is short.
    pub(super) async fn snapshot(&self) -> RunSnapshot {
        let cache = Arc::clone(&self.cache);
        super::blocking::blocking(move || {
            let mut guard = leviath_core::sync::lock(&cache);
            let (metas, listing) = &mut *guard;
            RunSnapshot::new(runstate::list_runs_cached(metas, listing))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runstate::{RunStatus, create_run, with_isolated_runs_dir_async};

    fn meta(id: &str, parent: Option<&str>, started_at: i64) -> RunMeta {
        let mut meta = RunMeta::new(
            id.to_string(),
            "agent".to_string(),
            "/agents/agent".to_string(),
            "task".to_string(),
            None,
            "/work".to_string(),
            1,
        );
        meta.started_at = started_at;
        meta.parent_run_id = parent.map(str::to_string);
        meta
    }

    fn snapshot_of(runs: Vec<RunMeta>) -> RunSnapshot {
        RunSnapshot::new(runs.into_iter().map(Arc::new).collect())
    }

    fn ids<'a>(runs: impl Iterator<Item = &'a Arc<RunMeta>>) -> Vec<String> {
        runs.map(|r| r.run_id.clone()).collect()
    }

    #[test]
    fn a_snapshot_answers_roots_children_and_lookups_without_rescanning() {
        let snap = snapshot_of(vec![
            meta("root-b", None, 3),
            meta("kid-1", Some("root-a"), 2),
            meta("root-a", None, 1),
            meta("kid-2", Some("root-a"), 0),
        ]);
        assert_eq!(ids(snap.under(None)), vec!["root-b", "root-a"]);
        assert_eq!(ids(snap.under(Some("root-a"))), vec!["kid-1", "kid-2"]);
        assert!(snap.under(Some("kid-1")).next().is_none());
        assert!(snap.under(Some("nobody")).next().is_none());
        assert_eq!(snap.get("kid-2").map(|r| r.started_at), Some(0));
        assert!(snap.get("nobody").is_none());
        assert_eq!(snap.into_runs().len(), 4);
    }

    #[tokio::test]
    async fn the_index_reads_the_runs_dir_and_reuses_parsed_records() {
        with_isolated_runs_dir_async("run-index-reuse", |_dir| async move {
            let mut finished = meta("finished", None, 10);
            finished.status = RunStatus::Complete;
            create_run(&finished).unwrap();
            create_run(&meta("live", Some("finished"), 20)).unwrap();

            let index = RunIndex::default();
            let first = index.snapshot().await;
            let first_finished = Arc::clone(first.get("finished").unwrap());
            assert_eq!(ids(first.under(Some("finished"))), vec!["live"]);
            assert_eq!(ids(first.into_runs().iter()), vec!["live", "finished"]);

            // A settled run's record is shared rather than parsed again.
            let second = index.snapshot().await;
            assert!(
                Arc::ptr_eq(&first_finished, second.get("finished").unwrap()),
                "an unchanged finished run is re-served"
            );

            // A run that appears is picked up; one that disappears is dropped.
            create_run(&meta("newer", None, 30)).unwrap();
            std::fs::remove_dir_all(runstate::run_dir("live")).unwrap();
            let third = index.snapshot().await;
            assert_eq!(ids(third.into_runs().iter()), vec!["newer", "finished"]);
        })
        .await;
    }

    #[tokio::test]
    async fn a_garbled_record_is_skipped_and_a_missing_dir_is_empty() {
        with_isolated_runs_dir_async("run-index-garbled", |_dir| async move {
            let index = RunIndex::default();
            assert!(index.snapshot().await.into_runs().is_empty());

            create_run(&meta("good", None, 1)).unwrap();
            let bad = runstate::run_dir("bad");
            std::fs::create_dir_all(&bad).unwrap();
            std::fs::write(bad.join(leviath_core::files::META_FILE), "{not json").unwrap();
            assert_eq!(ids(index.snapshot().await.into_runs().iter()), vec!["good"]);
        })
        .await;
    }

    /// A subtree is every run below one, at any depth, and never the root.
    #[tokio::test]
    async fn a_subtree_walk_finds_every_level() {
        with_isolated_runs_dir_async("run-index-subtree", |_dir| async move {
            create_run(&meta("root", None, 1)).unwrap();
            create_run(&meta("worker", Some("root"), 2)).unwrap();
            create_run(&meta("grandchild", Some("worker"), 3)).unwrap();
            create_run(&meta("elsewhere", None, 4)).unwrap();

            let snapshot = RunIndex::default().snapshot().await;
            let under = snapshot.descendants_of("root");
            assert_eq!(under.len(), 2, "{under:?}");
            assert!(under.contains("worker") && under.contains("grandchild"));
            assert!(!under.contains("root"), "never the root itself");
            assert!(!under.contains("elsewhere"));
            // A run with nothing under it has an empty subtree rather than no
            // answer: a fan-out that has not started yet is an ordinary record.
            assert!(snapshot.descendants_of("elsewhere").is_empty());
        })
        .await;
    }

    /// A record that names itself as its own ancestor cannot spin the walk.
    ///
    /// Nothing writes such a record, and a walk that trusted the parent map
    /// would hang rather than answer: the guard is that a run already seen is
    /// not walked again.
    #[test]
    fn a_cycle_in_the_parent_map_terminates() {
        let one = std::sync::Arc::new(meta("one", Some("two"), 1));
        let two = std::sync::Arc::new(meta("two", Some("one"), 2));
        let snapshot = RunSnapshot::new(vec![one, two]);
        let under = snapshot.descendants_of("one");
        assert_eq!(under.len(), 2, "both, once each: {under:?}");
    }
}
