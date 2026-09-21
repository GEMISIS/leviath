//! Reading the runs directory, off the draw loop.
//!
//! Everything the run list knows comes from disk: every run's `meta.json` and
//! `stages.json`, the blueprint each one was started from, and the context
//! window of the run on screen. With thousands of runs, asking the disk those
//! questions on the draw loop would put every stat, parse and manifest read
//! between one frame and the next, and between a key and its answer.
//!
//! [`RunLoader`] does the reading and returns a [`RunSnapshot`]. The dashboard
//! runs one on a thread of its own ([`spawn_run_feed`]) and picks up the newest
//! snapshot each tick without waiting for it. Tests call
//! [`RunLoader::collect`] directly, which reads the same files the same way.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use leviath_core::run_meta::StageRecord;
use tokio::sync::watch;

use crate::runstate::{self, ContextSnapshot, RunMeta, StatCache};
use crate::tui::flowgraph::StageGraph;

/// One run as the run list needs it, all shared: a snapshot is handed to the
/// draw loop every tick, and nothing in it is copied to get there.
#[derive(Debug, Clone)]
pub(crate) struct RunEntry {
    pub(crate) meta: Arc<RunMeta>,
    /// The run's stage ledger; empty until it has one, and in the first,
    /// list-only snapshot.
    pub(crate) stages: Arc<Vec<StageRecord>>,
    /// Whether `stages` was read, rather than skipped for a list-only pass.
    stages_read: bool,
    /// The graph of the blueprint the run was started from, or `None` when
    /// its manifest cannot be read.
    pub(crate) graph: Option<Arc<StageGraph>>,
}

/// Everything read from the runs directory in one pass.
#[derive(Debug, Clone)]
pub(crate) struct RunSnapshot {
    /// When the pass began: anything that changed on disk after this moment
    /// may be missing from it.
    pub(crate) taken_at: std::time::Instant,
    /// Newest first, the same order as [`runstate::list_runs`].
    pub(crate) runs: Vec<RunEntry>,
    /// The context window of the run on screen, by run id.
    pub(crate) context: Option<(String, Arc<ContextSnapshot>)>,
}

/// The reader behind a [`RunSnapshot`], with its caches. Each file is parsed
/// again only when its stat changes, and a blueprint's graph is built once per
/// manifest version however many runs were started from it: a fan-out of fifty
/// workers is fifty runs and one blueprint.
#[derive(Default)]
pub(crate) struct RunLoader {
    listing: runstate::RunDirListing,
    metas: StatCache<RunMeta>,
    stages: StatCache<Vec<StageRecord>>,
    contexts: StatCache<ContextSnapshot>,
    graphs: StatCache<StageGraph>,
    /// Last round's runs, by the address of their `meta.json` record. The
    /// meta cache hands back the same record until the file changes, so a
    /// finished run found here is unchanged since last round, and so are its
    /// stage ledger and graph: they are reused without asking the disk.
    last: HashMap<usize, RunEntry>,
}

impl RunLoader {
    /// Read the runs directory. `showing` is the run whose context window is
    /// worth reading, the one the detail view draws; `with_stages` is false
    /// only for the first snapshot, so the list can appear before every stage
    /// ledger has been parsed.
    pub(crate) fn collect(&mut self, showing: Option<&str>, with_stages: bool) -> RunSnapshot {
        let taken_at = std::time::Instant::now();
        let metas = runstate::list_runs_cached(&mut self.metas, &mut self.listing);
        // Runs come and go only when the directory is listed again.
        if self.listing.relisted() {
            let live_dirs = self.listing.dir_set();
            self.stages.retain_under(&live_dirs);
            self.contexts.retain_under(&live_dirs);
        }
        let mut graphs: HashMap<&str, Option<Arc<StageGraph>>> = HashMap::new();
        let mut runs = Vec::with_capacity(metas.len());
        let mut last = HashMap::with_capacity(metas.len());
        for meta in &metas {
            let key = Arc::as_ptr(meta) as usize;
            // A finished run whose record is the one read last round. Its
            // ledger is final too, unless last round skipped the ledgers.
            if let Some(known) = self.last.remove(&key)
                && !runstate::settle_window(meta).is_zero()
                && (known.stages_read || !with_stages)
            {
                runs.push(known.clone());
                last.insert(key, known);
                continue;
            }
            let stages = if with_stages {
                runstate::read_stages_index_settled(
                    &meta.run_id,
                    &mut self.stages,
                    runstate::settle_window(meta),
                )
            } else {
                Arc::default()
            };
            let graph = graphs
                .entry(meta.agent_path.as_str())
                .or_insert_with(|| {
                    super::graph::load_stage_graph_cached(&meta.agent_path, &mut self.graphs)
                })
                .clone();
            let entry = RunEntry {
                meta: meta.clone(),
                stages,
                stages_read: with_stages,
                graph,
            };
            runs.push(entry.clone());
            last.insert(key, entry);
        }
        self.last = last;
        let context = showing.and_then(|id| {
            metas.iter().any(|run| run.run_id == id).then_some(())?;
            runstate::read_context_snapshot_cached(id, &mut self.contexts)
                .map(|snapshot| (id.to_string(), snapshot))
        });
        RunSnapshot {
            taken_at,
            runs,
            context,
        }
    }
}

/// The loader's end of the snapshot channel: `None` until the first read.
type SnapshotSender = watch::Sender<Option<Arc<RunSnapshot>>>;

/// The dashboard's end of a running [`RunLoader`] thread.
pub(crate) struct RunFeed {
    /// The newest snapshot; `None` until the first one lands.
    snapshots: watch::Receiver<Option<Arc<RunSnapshot>>>,
    /// Which run the detail view is drawing, sent when it changes.
    showing: std_mpsc::Sender<Option<String>>,
    /// The last value sent on `showing`, so a tick sends nothing new.
    last_showing: Option<String>,
}

impl RunFeed {
    /// Tell the loader which run is on screen; it reads that run's context
    /// window at once rather than on its next round.
    pub(crate) fn show(&mut self, run_id: Option<&str>) {
        if self.last_showing.as_deref() == run_id {
            return;
        }
        self.last_showing = run_id.map(str::to_string);
        // A loader that has gone away takes no more questions; the list keeps
        // the last snapshot it sent.
        let _ = self.showing.send(self.last_showing.clone());
    }

    /// The newest snapshot, if one arrived since the last call.
    pub(crate) fn take(&mut self) -> Option<Arc<RunSnapshot>> {
        if !self.snapshots.has_changed().unwrap_or(false) {
            return None;
        }
        self.snapshots.borrow_and_update().clone()
    }
}

/// Start a [`RunLoader`] on a thread of its own, reading every `interval` and
/// straight away when the run on screen changes. The thread ends when the
/// returned [`RunFeed`] is dropped.
///
/// A thread rather than a task: every step of a round is blocking file I/O,
/// and a round over thousands of runs is long enough to hold up whatever
/// else a runtime worker had queued.
pub(crate) fn spawn_run_feed(interval: Duration) -> RunFeed {
    let (snap_tx, snapshots) = watch::channel(None);
    let (showing, show_rx) = std_mpsc::channel();
    std::thread::Builder::new()
        .name("lev-dash-runs".to_string())
        .spawn(move || run_feed_loop(RunLoader::default(), snap_tx, show_rx, interval))
        .expect("spawn the run loader thread");
    RunFeed {
        snapshots,
        showing,
        last_showing: None,
    }
}

/// The loader thread's body; see [`spawn_run_feed`].
fn run_feed_loop(
    mut loader: RunLoader,
    snapshots: SnapshotSender,
    showing: std_mpsc::Receiver<Option<String>>,
    interval: Duration,
) {
    let mut on_screen: Option<String> = None;
    // The list first, then the rest: every column of the run list comes from
    // `meta.json`, so the list can be drawn before the stage ledgers are read.
    let mut with_stages = false;
    loop {
        let snapshot = loader.collect(on_screen.as_deref(), with_stages);
        if snapshots.send(Some(Arc::new(snapshot))).is_err() {
            return;
        }
        let wait = if with_stages {
            interval
        } else {
            Duration::ZERO
        };
        with_stages = true;
        match showing.recv_timeout(wait) {
            Ok(run_id) => on_screen = run_id,
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        }
        // Several changes queued while reading: only the last one matters.
        while let Ok(run_id) = showing.try_recv() {
            on_screen = run_id;
        }
    }
}

#[cfg(test)]
impl RunFeed {
    /// A feed with no loader thread behind it: the test holds the other ends
    /// and plays the loader.
    pub(crate) fn detached() -> (Self, SnapshotSender, std_mpsc::Receiver<Option<String>>) {
        let (snap_tx, snapshots) = watch::channel(None);
        let (showing, show_rx) = std_mpsc::channel();
        let feed = Self {
            snapshots,
            showing,
            last_showing: None,
        };
        (feed, snap_tx, show_rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runstate::{RunStatus, create_run, with_isolated_runs_dir};

    fn run(id: &str, agent_path: &str, status: RunStatus, started_at: i64) -> RunMeta {
        let mut meta = RunMeta::new(
            id.to_string(),
            "agent".to_string(),
            agent_path.to_string(),
            "task".to_string(),
            None,
            "/work".to_string(),
            1,
        );
        meta.status = status;
        meta.started_at = started_at;
        meta
    }

    fn context() -> ContextSnapshot {
        ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 1,
            max_tokens: 10,
            regions: vec![],
        }
    }

    /// A manifest file the graph loader can parse, from a bundled agent.
    fn write_manifest(dir: &std::path::Path) -> String {
        let manifest = crate::bundled::BUNDLED_AGENTS[0]
            .files
            .iter()
            .find(|(path, _)| *path == leviath_core::files::MANIFEST_FILENAME)
            .map(|(_, content)| *content)
            .unwrap();
        let path = dir.join(leviath_core::files::MANIFEST_FILENAME);
        std::fs::write(&path, manifest).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// One pass reads every run, newest first; the list-only pass leaves the
    /// ledgers out; the run on screen, and only that one, brings its context
    /// window; and runs of one blueprint share one graph.
    #[test]
    fn collect_reads_the_runs_directory() {
        let blueprint = tempfile::tempdir().unwrap();
        let agent_path = write_manifest(blueprint.path());
        with_isolated_runs_dir("run-loader-collect", |_| {
            create_run(&run("older", &agent_path, RunStatus::Complete, 10)).unwrap();
            create_run(&run("newer", &agent_path, RunStatus::Running, 20)).unwrap();
            create_run(&run("lost", "/no/such/blueprint", RunStatus::Complete, 5)).unwrap();
            for id in ["older", "newer"] {
                runstate::write_stages_index(id, &[StageRecord::new("main".to_string(), 0)])
                    .unwrap();
            }
            runstate::write_context_snapshot("newer", &context()).unwrap();

            let mut loader = RunLoader::default();
            let list_only = loader.collect(Some("newer"), false);
            let ids: Vec<&str> = list_only
                .runs
                .iter()
                .map(|r| r.meta.run_id.as_str())
                .collect();
            assert_eq!(ids, ["newer", "older", "lost"]);
            assert!(list_only.runs.iter().all(|r| r.stages.is_empty()));
            assert_eq!(list_only.context.as_ref().unwrap().0, "newer");
            let (a, b) = (&list_only.runs[0].graph, &list_only.runs[1].graph);
            assert!(Arc::ptr_eq(a.as_ref().unwrap(), b.as_ref().unwrap()));
            assert!(list_only.runs[2].graph.is_none(), "an unreadable blueprint");

            // The full pass reads the ledgers, the settled run's included,
            // which the list-only pass had skipped.
            let full = loader.collect(Some("older"), true);
            assert!(full.runs.iter().take(2).all(|r| r.stages.len() == 1));
            assert!(full.context.is_none(), "`older` has no context file");

            // A run the directory does not hold brings no context either.
            assert!(loader.collect(Some("gone"), true).context.is_none());
        });
    }

    /// A finished run whose record has not changed is handed back as it was,
    /// ledger and all, without asking the disk; a live run is read again.
    #[test]
    fn collect_reuses_a_settled_run_and_rereads_a_live_one() {
        with_isolated_runs_dir("run-loader-reuse", |_| {
            create_run(&run("done", "/p", RunStatus::Complete, 10)).unwrap();
            create_run(&run("live", "/p", RunStatus::Running, 20)).unwrap();
            let one = [StageRecord::new("main".to_string(), 0)];
            let two = [
                StageRecord::new("main".to_string(), 0),
                StageRecord::new("next".to_string(), 1),
            ];
            for id in ["done", "live"] {
                runstate::write_stages_index(id, &one).unwrap();
            }
            let mut loader = RunLoader::default();
            let first = loader.collect(None, true);

            // Both ledgers change on disk. The finished run's record did not,
            // so its ledger is not looked at; the live one's is.
            for id in ["done", "live"] {
                runstate::write_stages_index(id, &two).unwrap();
            }
            let second = loader.collect(None, true);
            let by_id = |snap: &RunSnapshot, id: &str| {
                snap.runs
                    .iter()
                    .find(|r| r.meta.run_id == id)
                    .unwrap()
                    .clone()
            };
            assert!(Arc::ptr_eq(
                &by_id(&first, "done").stages,
                &by_id(&second, "done").stages
            ));
            assert_eq!(by_id(&second, "live").stages.len(), 2);

            // With the directory settled it is not listed again, and the same
            // runs come back.
            loader.listing.age();
            let third = loader.collect(None, true);
            assert!(!loader.listing.relisted());
            assert_eq!(third.runs.len(), 2);
        });
    }

    /// The feed hands over each new snapshot once, and tells the loader about
    /// the run on screen only when it changes.
    #[test]
    fn a_feed_takes_each_snapshot_once_and_shows_changes_only() {
        let (mut feed, snap_tx, show_rx) = RunFeed::detached();
        assert!(feed.take().is_none(), "nothing sent yet");
        let snapshot = Arc::new(RunSnapshot {
            taken_at: std::time::Instant::now(),
            runs: vec![],
            context: None,
        });
        snap_tx.send(Some(snapshot.clone())).unwrap();
        assert!(Arc::ptr_eq(&feed.take().unwrap(), &snapshot));
        assert!(
            feed.take().is_none(),
            "the same snapshot is not taken twice"
        );

        feed.show(Some("a"));
        feed.show(Some("a"));
        feed.show(None);
        assert_eq!(show_rx.try_recv().unwrap(), Some("a".to_string()));
        assert_eq!(show_rx.try_recv().unwrap(), None);
        assert!(show_rx.try_recv().is_err(), "the repeat sent nothing");

        // A loader that has gone away is not an error for the dashboard.
        drop(show_rx);
        drop(snap_tx);
        feed.show(Some("b"));
        assert!(feed.take().is_none());
    }

    /// The loader thread sends snapshots, reads the run it is shown, and
    /// carries on until the dashboard goes.
    #[test]
    fn the_feed_thread_reads_the_run_it_is_shown() {
        with_isolated_runs_dir("run-loader-thread", |_| {
            create_run(&run("r1", "/p", RunStatus::Running, 10)).unwrap();
            runstate::write_context_snapshot("r1", &context()).unwrap();
            let mut feed = spawn_run_feed(Duration::from_millis(1));
            feed.show(Some("r1"));
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut seen_context = false;
            // Whether a round of the wait finds a snapshot ready is the
            // scheduler's business: the loader can finish both its rounds
            // before the first `take`, or not be started yet. `is_some_and`
            // keeps that out of the test's own branches, which are counted.
            while !seen_context && std::time::Instant::now() < deadline {
                seen_context = feed.take().is_some_and(|snapshot| {
                    assert_eq!(snapshot.runs.len(), 1);
                    snapshot.context.is_some()
                });
                std::thread::yield_now();
            }
            assert!(seen_context, "the run on screen had its context read");
        });
    }

    /// The loop returns when nobody takes its snapshots any more, and when
    /// nobody can tell it what is on screen any more.
    #[test]
    fn the_feed_loop_returns_when_either_end_goes() {
        with_isolated_runs_dir("run-loader-loop-ends", |_| {
            // Nobody receiving snapshots: the first send fails.
            let (snap_tx, snap_rx) = watch::channel(None);
            let (_show_tx, show_rx) = std_mpsc::channel();
            drop(snap_rx);
            run_feed_loop(RunLoader::default(), snap_tx, show_rx, Duration::ZERO);

            // Nobody sending: two queued changes are taken, then the wait for
            // the next sees the sender gone.
            let (snap_tx, snap_rx) = watch::channel(None);
            let (show_tx, show_rx) = std_mpsc::channel();
            show_tx.send(Some("queued".to_string())).unwrap();
            show_tx.send(Some("latest".to_string())).unwrap();
            drop(show_tx);
            run_feed_loop(RunLoader::default(), snap_tx, show_rx, Duration::ZERO);
            assert!(snap_rx.borrow().is_some(), "it sent before it stopped");
        });
    }
}
