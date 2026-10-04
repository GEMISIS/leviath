//! Reading the runs directory, off the draw loop.
//!
//! Everything the run list knows comes from disk: every run's record, and
//! for the run on screen its stage ledger, the graph it runs and its context
//! window. With thousands of runs, asking the disk those questions on the
//! draw loop would put every stat and parse between one frame and the next,
//! and between a key and its answer.
//!
//! The records come from the run index, a stat apiece. The rest of a run is in
//! its run file, which only the run on screen has read: the list draws from
//! records alone, and only the detail view, the explorer and the band draw a
//! ledger or a graph, all of them for the run on screen.
//!
//! [`RunLoader`] does the reading and returns a [`RunSnapshot`]. The dashboard
//! runs one on a thread of its own ([`spawn_run_feed`]) and picks up the newest
//! snapshot each tick without waiting for it. Tests call
//! [`RunLoader::collect`] directly, which reads the same files the same way.
//!
//! While the detail view is open the thread also replays the run's history
//! (its window at every recorded point, and the path it took), the most
//! expensive read the dashboard makes: a long run's file is megabytes of
//! steps. The detail view draws at once from the cheap data and takes the
//! history when it lands.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use leviath_core::run_meta::StageRecord;
use leviath_runtime::spec::graph::RunGraph;
use tokio::sync::watch;

use crate::runstate::{self, ContextSnapshot, FileStamp, RunHistory, RunMeta, StatCache};
use crate::tui::flowgraph::StageGraph;

/// One run as the run list needs it, all shared: a snapshot is handed to the
/// draw loop every tick, and nothing in it is copied to get there.
#[derive(Debug, Clone)]
pub(crate) struct RunEntry {
    pub(crate) meta: Arc<RunMeta>,
    /// The run's stage ledger; empty until it has one, and until the run has
    /// been on screen.
    pub(crate) stages: Arc<Vec<StageRecord>>,
    /// Whether `stages` and `graph` were read, which they are once the run is
    /// on screen.
    detail_read: bool,
    /// The graph the run runs, read off its run file's spec, or `None` while
    /// the run has no run file this build reads, and until the run has been
    /// on screen.
    pub(crate) graph: Option<Arc<StageGraph>>,
}

#[cfg(test)]
impl RunEntry {
    /// A run as a list-only pass reads it, from its record alone.
    pub(crate) fn listed(meta: RunMeta) -> Self {
        Self {
            meta: Arc::new(meta),
            stages: Arc::default(),
            detail_read: false,
            graph: None,
        }
    }
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

/// One run's history, read off its run file by the loader thread.
#[derive(Debug)]
pub(crate) struct LoadedHistory {
    /// The run it belongs to.
    pub(crate) run_id: String,
    /// The run file's stat, taken before it was read.
    pub(crate) stamp: Option<FileStamp>,
    /// What the run file holds.
    pub(crate) history: RunHistory,
}

/// How often one run's history is read again. A replay of a long run is
/// tens of milliseconds, too much to repeat on every round a live run grows.
const HISTORY_REREAD: Duration = Duration::from_secs(1);

/// The stat of the run file the history the dashboard holds was read at;
/// `None` when it holds none for the run.
pub(crate) type Held = Option<Option<FileStamp>>;

/// Which run's history the dashboard holds, and at what stat: written every
/// tick, and read by the loader when it decides whether to read a history.
/// Shared rather than sent, so that taking a history does not wake the
/// loader for a round of its own.
type HeldSlot = Arc<std::sync::Mutex<(Option<String>, Held)>>;

/// What `slot` says the dashboard holds of `run_id`'s history; nothing when
/// it speaks of another run (the dashboard has moved on since).
fn held_for(slot: &HeldSlot, run_id: &str) -> Held {
    let held = leviath_core::sync::lock(slot);
    match &held.0 {
        Some(run) if run == run_id => held.1,
        _ => None,
    }
}

/// The reader behind a [`RunSnapshot`], with its caches. Each file is parsed
/// again only when its stat changes. A run's graph is read once, since a run's
/// spec never changes, and drawn once however many runs share it: a fan-out
/// of fifty workers is fifty runs and one graph.
#[derive(Default)]
pub(crate) struct RunLoader {
    listing: runstate::RunDirListing,
    metas: StatCache<RunMeta>,
    stages: StatCache<Vec<StageRecord>>,
    contexts: StatCache<ContextSnapshot>,
    /// Each run's drawn graph, by run id.
    run_graphs: HashMap<String, Arc<StageGraph>>,
    /// Every distinct graph read so far and its drawing, so runs of one graph
    /// share one.
    drawn: Vec<(RunGraph, Arc<StageGraph>)>,
    /// The run whose history was read last, and when.
    history_read: Option<(String, std::time::Instant)>,
    /// Last round's runs, by the address of their run record. The meta cache
    /// hands back the same record until the run file changes, so a
    /// finished run found here is unchanged since last round, and so are its
    /// stage ledger and graph: they are reused without asking the disk.
    last: HashMap<usize, RunEntry>,
}

impl RunLoader {
    /// Read the runs directory. `showing` is the run whose ledger, graph and
    /// context window are worth reading, the one the detail view draws;
    /// `with_stages` is false only for the first snapshot, so the list can
    /// appear before any run file has been read.
    pub(crate) fn collect(&mut self, showing: Option<&str>, with_stages: bool) -> RunSnapshot {
        let taken_at = std::time::Instant::now();
        let metas = runstate::list_runs_cached(&mut self.metas, &mut self.listing);
        // Runs come and go only when the directory is listed again.
        if self.listing.relisted() {
            let live_dirs = self.listing.dir_set();
            self.stages.retain_under(&live_dirs);
            self.contexts.retain_under(&live_dirs);
            let ids: HashSet<&str> = metas.iter().map(|m| m.run_id.as_str()).collect();
            self.run_graphs.retain(|id, _| ids.contains(id.as_str()));
        }
        let mut runs = Vec::with_capacity(metas.len());
        let mut last = HashMap::with_capacity(metas.len());
        for meta in &metas {
            let key = Arc::as_ptr(meta) as usize;
            let detail = with_stages && showing == Some(meta.run_id.as_str());
            // A finished run whose record is the one read last round. Its
            // ledger and graph are final too, once they have been read.
            if let Some(known) = self.last.remove(&key)
                && !runstate::settle_window(meta).is_zero()
                && (known.detail_read || !detail)
            {
                runs.push(known.clone());
                last.insert(key, known);
                continue;
            }
            let (stages, graph) = match detail {
                true => (
                    runstate::read_stages_index_settled(
                        &meta.run_id,
                        &mut self.stages,
                        runstate::settle_window(meta),
                    ),
                    self.graph_of(&meta.run_id),
                ),
                false => (Arc::default(), None),
            };
            let entry = RunEntry {
                meta: meta.clone(),
                stages,
                detail_read: detail,
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

    /// The history of `run_id`, unless the dashboard holds it as the run
    /// file stands (`held`), or it was read less than [`HISTORY_REREAD`] ago.
    pub(crate) fn history_of(&mut self, run_id: &str, held: Held) -> Option<LoadedHistory> {
        let stamp = runstate::run_file_stamp(run_id);
        let recent = self
            .history_read
            .as_ref()
            .is_some_and(|(read, at)| read == run_id && at.elapsed() < HISTORY_REREAD);
        if held == Some(stamp) || recent {
            return None;
        }
        // Stat before reading: an append that lands during the read makes
        // the next round read again, rather than hiding behind a newer stat.
        let history = runstate::run_history(run_id);
        self.history_read = Some((run_id.to_string(), std::time::Instant::now()));
        Some(LoadedHistory {
            run_id: run_id.to_string(),
            stamp,
            history,
        })
    }

    /// The drawn graph of `run_id`, read off the front of its run file the
    /// first time and kept after. A run with no readable run file yet is
    /// asked again next round.
    fn graph_of(&mut self, run_id: &str) -> Option<Arc<StageGraph>> {
        if let Some(known) = self.run_graphs.get(run_id) {
            return Some(known.clone());
        }
        let spec = runstate::run_file::spec_in(&runstate::run_dir(run_id)).ok()?;
        let graph = &spec.graph;
        let drawn = match self.drawn.iter().find(|(seen, _)| seen == graph) {
            Some((_, drawn)) => drawn.clone(),
            None => {
                let drawn = Arc::new(StageGraph::from_graph(graph));
                self.drawn.push((graph.clone(), drawn.clone()));
                drawn
            }
        };
        self.run_graphs.insert(run_id.to_string(), drawn.clone());
        Some(drawn)
    }
}

/// The loader's end of the snapshot channel: `None` until the first read.
type SnapshotSender = watch::Sender<Option<Arc<RunSnapshot>>>;

/// What the dashboard tells the loader: the run the cursor is on, and
/// whether its history is wanted, which it is while the detail view is open.
type Shown = (Option<String>, bool);

/// The dashboard's end of a running [`RunLoader`] thread.
pub(crate) struct RunFeed {
    /// The newest snapshot; `None` until the first one lands.
    snapshots: watch::Receiver<Option<Arc<RunSnapshot>>>,
    /// The history of the run on screen, each read once.
    histories: std_mpsc::Receiver<LoadedHistory>,
    /// Which run the detail view is drawing, sent when it changes.
    showing: std_mpsc::Sender<Shown>,
    /// The last value sent on `showing`, so a tick sends nothing new.
    last_showing: Shown,
    /// What of the history of the run on screen is held.
    held: HeldSlot,
}

impl RunFeed {
    /// Tell the loader which run is on screen, and, when its history is
    /// wanted, what of it is held; it reads them at once rather than on its
    /// next round.
    pub(crate) fn show(&mut self, run_id: Option<&str>, history: Option<Held>) {
        *leviath_core::sync::lock(&self.held) = (run_id.map(str::to_string), history.flatten());
        let wanted = history.is_some();
        if self.last_showing.0.as_deref() == run_id && self.last_showing.1 == wanted {
            return;
        }
        self.last_showing = (run_id.map(str::to_string), wanted);
        // A loader that has gone away takes no more questions; the list keeps
        // the last snapshot it sent.
        let _ = self.showing.send(self.last_showing.clone());
    }

    /// The newest history the loader read, if one arrived since the last
    /// call. Only the newest matters: an older one is of a run left since, or
    /// of the same run before it grew.
    pub(crate) fn take_history(&mut self) -> Option<LoadedHistory> {
        self.histories.try_iter().last()
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
    let (feed, ends) = RunFeed::new();
    std::thread::Builder::new()
        .name("lev-dash-runs".to_string())
        .spawn(move || run_feed_loop(RunLoader::default(), ends, interval))
        .expect("spawn the run loader thread");
    feed
}

/// The loader thread's ends of the feed's channels.
pub(crate) struct FeedEnds {
    /// Where the snapshots go.
    pub(crate) snapshots: SnapshotSender,
    /// Where the histories go.
    pub(crate) histories: std_mpsc::Sender<LoadedHistory>,
    /// What the dashboard says is on screen.
    pub(crate) showing: std_mpsc::Receiver<Shown>,
    /// What of its history the dashboard holds.
    pub(crate) held: HeldSlot,
}

impl RunFeed {
    /// A feed and the ends a loader holds: the thread's in the dashboard, a
    /// test's when it plays the loader.
    pub(crate) fn new() -> (Self, FeedEnds) {
        let (snap_tx, snapshots) = watch::channel(None);
        let (history_tx, histories) = std_mpsc::channel();
        let (showing, show_rx) = std_mpsc::channel();
        let held = HeldSlot::default();
        let feed = Self {
            snapshots,
            histories,
            showing,
            last_showing: (None, false),
            held: held.clone(),
        };
        let ends = FeedEnds {
            snapshots: snap_tx,
            histories: history_tx,
            showing: show_rx,
            held,
        };
        (feed, ends)
    }
}

/// The loader thread's body; see [`spawn_run_feed`].
fn run_feed_loop(mut loader: RunLoader, ends: FeedEnds, interval: Duration) {
    let FeedEnds {
        snapshots,
        histories,
        showing,
        held,
    } = ends;
    let mut on_screen: Shown = (None, false);
    // The list first, then the rest: every column of the run list comes from
    // the run's record, so the list can be drawn before the stage ledgers are read.
    let mut with_stages = false;
    loop {
        let snapshot = loader.collect(on_screen.0.as_deref(), with_stages);
        if snapshots.send(Some(Arc::new(snapshot))).is_err() {
            return;
        }
        // The history after the snapshot: the detail view draws from the
        // snapshot first, and the history fills in the path and the past.
        let history = match &on_screen {
            (Some(run_id), true) => loader.history_of(run_id, held_for(&held, run_id)),
            _ => None,
        };
        if let Some(history) = history
            && histories.send(history).is_err()
        {
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
            regions: vec![crate::test_fixtures::fixtures::region("task")],
        }
    }

    /// One pass reads every run, newest first, from its record; and the run
    /// on screen, and only that one, brings its ledger, its graph and its
    /// context window. The list-only pass reads no run file at all.
    #[test]
    fn collect_reads_the_runs_directory() {
        let agent_path = "/p";
        with_isolated_runs_dir("run-loader-collect", |_| {
            create_run(&run("older", agent_path, RunStatus::Complete, 10)).unwrap();
            create_run(&run("newer", agent_path, RunStatus::Running, 20)).unwrap();
            create_run(&run("lost", "/no/such/blueprint", RunStatus::Complete, 5)).unwrap();
            for id in ["older", "newer"] {
                runstate::write_stages_index(id, &[StageRecord::new("main".to_string(), 0)])
                    .unwrap();
            }
            runstate::write_context_snapshot("newer", &context()).unwrap();
            runstate::write_context_snapshot("older", &context()).unwrap();

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
            // The list is drawn from the run index alone: no run file is
            // opened for a graph before the rows are on screen.
            assert!(list_only.runs.iter().all(|r| r.graph.is_none()));

            // The full pass reads the ledger and the graph of the run on
            // screen, a settled one included, and of no other.
            let full = loader.collect(Some("older"), true);
            let read: Vec<(&str, usize, bool)> = full
                .runs
                .iter()
                .map(|r| (r.meta.run_id.as_str(), r.stages.len(), r.graph.is_some()))
                .collect();
            assert_eq!(
                read,
                [("newer", 0, false), ("older", 1, true), ("lost", 0, false)]
            );
            assert_eq!(full.context.as_ref().unwrap().0, "older");
            // Moving on keeps what was read for a finished run.
            let moved = loader.collect(Some("newer"), true);
            assert_eq!(moved.runs[0].stages.len(), 1);
            assert_eq!(moved.runs[1].stages.len(), 1);

            // A run the directory does not hold brings no context either.
            assert!(loader.collect(Some("gone"), true).context.is_none());
            // Nor a graph: one whose file went between the listing and the
            // read is asked for again next round.
            assert!(loader.graph_of("gone").is_none());
        });
    }

    /// A run's graph is read off its run file's spec, once, and runs of the
    /// same graph share one drawing; a run of another graph has its own.
    #[tokio::test]
    async fn runs_of_one_graph_share_one_drawing_read_off_their_run_files() {
        use leviath_runtime::runfile::{CheckpointPolicy, RunFileWriter};
        crate::runstate::with_isolated_runs_dir_async("run-loader-graphs", |_| async move {
            let agent = tempfile::tempdir().unwrap();
            let manifest = crate::test_support::write_test_agent(
                agent.path(),
                crate::test_support::inline_coder_manifest(),
            );
            let mut registry = leviath_runtime::ProviderRegistry::new();
            registry.register(
                "anthropic".to_string(),
                Arc::new(crate::test_support::FakeProvider::new().context_window(100_000)),
            );
            let first = crate::daemon::starter::testing::run_on_disk(
                crate::config::Config::default(),
                registry,
                &runstate::runs_dir(),
                &manifest,
            );
            // The same spec again, as a run of its own.
            let reader = runstate::run_file::open_in(&runstate::run_dir(&first)).unwrap();
            let mut spec = reader.spec().clone();
            spec.run_id = leviath_runtime::spec::names::RunId::new("twin").unwrap();
            let twin = runstate::run_dir("twin");
            std::fs::create_dir_all(&twin).unwrap();
            RunFileWriter::create(
                &runstate::run_file::path_in(&twin),
                &spec,
                &leviath_runtime::spec::env::CodeFiles::new(),
                &reader.state_at(0).unwrap(),
                CheckpointPolicy::default(),
            )
            .unwrap();
            create_run(&run("lost", "/p", RunStatus::Complete, 1)).unwrap();

            let mut loader = RunLoader::default();
            let graph_of = |snap: &RunSnapshot, id: &str| {
                snap.runs
                    .iter()
                    .find(|r| r.meta.run_id == id)
                    .unwrap()
                    .graph
                    .clone()
            };
            let mut shown = |id: &str| graph_of(&loader.collect(Some(id), true), id);
            let one = shown(&first).expect("read off the run file");
            assert!(Arc::ptr_eq(&one, &shown("twin").unwrap()));
            assert!(!Arc::ptr_eq(&one, &shown("lost").unwrap()));
            assert!(one.node("analyze").is_some());
            // The next pass hands back the drawing it kept.
            assert!(Arc::ptr_eq(&one, &shown(&first).unwrap()));
            // A run whose file is not there (yet) has no drawing, and is
            // asked again next round.
            assert!(loader.graph_of("no-such-run").is_none());
        })
        .await;
    }

    /// A finished run whose record has not changed is handed back as it was,
    /// ledger and all, without asking the disk; the live run on screen is
    /// read again.
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
            let first = loader.collect(Some("done"), true);

            // Both ledgers change on disk. The finished run's record did not,
            // so its ledger is not looked at; the live one's is.
            for id in ["done", "live"] {
                runstate::write_stages_index(id, &two).unwrap();
            }
            let second = loader.collect(Some("live"), true);
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

    /// The feed hands over each new snapshot once, and the newest history
    /// once, and tells the loader about the run on screen only when it, or
    /// whether its history is wanted, changes.
    #[test]
    fn a_feed_takes_each_snapshot_once_and_shows_changes_only() {
        let (mut feed, ends) = RunFeed::new();
        assert!(feed.take().is_none(), "nothing sent yet");
        let snapshot = Arc::new(RunSnapshot {
            taken_at: std::time::Instant::now(),
            runs: vec![],
            context: None,
        });
        ends.snapshots.send(Some(snapshot.clone())).unwrap();
        assert!(Arc::ptr_eq(&feed.take().unwrap(), &snapshot));
        assert!(
            feed.take().is_none(),
            "the same snapshot is not taken twice"
        );

        assert!(feed.take_history().is_none(), "nothing read yet");
        for run_id in ["older", "newer"] {
            ends.histories
                .send(LoadedHistory {
                    run_id: run_id.to_string(),
                    stamp: None,
                    history: RunHistory::default(),
                })
                .unwrap();
        }
        assert_eq!(feed.take_history().unwrap().run_id, "newer");
        assert!(feed.take_history().is_none(), "each is taken once");

        feed.show(Some("a"), None);
        feed.show(Some("a"), None);
        feed.show(Some("a"), Some(None));
        // What is held changes without a word to the loader.
        feed.show(Some("a"), Some(Some(None)));
        assert_eq!(held_for(&ends.held, "a"), Some(None));
        assert_eq!(held_for(&ends.held, "b"), None, "said of another run");
        feed.show(None, Some(None));
        let shown: Vec<Shown> = ends.showing.try_iter().collect();
        assert_eq!(
            shown,
            [
                (Some("a".to_string()), false),
                (Some("a".to_string()), true),
                (None, true)
            ],
            "the repeats sent nothing"
        );

        // A loader that has gone away is not an error for the dashboard.
        drop(ends);
        feed.show(Some("b"), None);
        assert!(feed.take().is_none());
        assert!(feed.take_history().is_none());
    }

    /// A history is read unless the dashboard holds it as the run file
    /// stands, and not twice in a second for one run: not on every round of
    /// a run that keeps writing.
    #[test]
    fn a_history_is_read_unless_it_is_held_as_it_stands() {
        with_isolated_runs_dir("run-loader-history", |_| {
            let mut loader = RunLoader::default();
            let first = loader.history_of("r1", None).expect("none held");
            assert_eq!((first.run_id.as_str(), first.stamp), ("r1", None));
            assert!(first.history.points.is_empty(), "no run file, no points");
            assert!(
                loader.history_of("r1", None).is_none(),
                "read less than a second ago"
            );
            let age = |loader: &mut RunLoader| {
                let (_, at) = loader.history_read.as_mut().unwrap();
                *at -= HISTORY_REREAD;
            };
            age(&mut loader);
            assert!(
                loader.history_of("r1", Some(None)).is_none(),
                "held as it stands"
            );

            let dir = runstate::run_dir("r1");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(runstate::run_file::path_in(&dir), b"grown").unwrap();
            let again = loader
                .history_of("r1", Some(None))
                .expect("held, but the run file has moved on");
            assert!(again.stamp.is_some());
            assert!(
                loader.history_of("r1", Some(None)).is_none(),
                "still stale, but read less than a second ago"
            );
            assert_eq!(loader.history_of("r2", None).unwrap().run_id, "r2");
        });
    }

    /// The loader thread sends snapshots, reads the run it is shown and its
    /// history when that is wanted, and carries on until the dashboard goes.
    #[test]
    fn the_feed_thread_reads_the_run_it_is_shown() {
        with_isolated_runs_dir("run-loader-thread", |_| {
            create_run(&run("r1", "/p", RunStatus::Running, 10)).unwrap();
            runstate::write_context_snapshot("r1", &context()).unwrap();
            let mut feed = spawn_run_feed(Duration::from_millis(1));
            feed.show(Some("r1"), Some(None));
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut seen_context = false;
            let mut seen_history = false;
            // Whether a round of the wait finds a snapshot ready is the
            // scheduler's business: the loader can finish both its rounds
            // before the first `take`, or not be started yet. `is_some_and`
            // keeps that out of the test's own branches, which are counted.
            while !(seen_context && seen_history) && std::time::Instant::now() < deadline {
                seen_context |= feed.take().is_some_and(|snapshot| {
                    assert_eq!(snapshot.runs.len(), 1);
                    snapshot.context.is_some()
                });
                seen_history |= feed.take_history().is_some_and(|h| h.run_id == "r1");
                std::thread::yield_now();
            }
            assert!(seen_context, "the run on screen had its context read");
            assert!(seen_history, "the run on screen had its history read");
        });
    }

    /// The loop returns when nobody takes its snapshots any more, when nobody
    /// takes its histories any more, and when nobody can tell it what is on
    /// screen any more.
    #[test]
    fn the_feed_loop_returns_when_either_end_goes() {
        with_isolated_runs_dir("run-loader-loop-ends", |_| {
            // Nobody receiving snapshots: the first send fails.
            let (feed, ends) = RunFeed::new();
            drop(feed);
            run_feed_loop(RunLoader::default(), ends, Duration::ZERO);

            // Nobody receiving histories: the first one read is not sent.
            let (feed, ends) = RunFeed::new();
            let RunFeed {
                snapshots,
                histories,
                showing,
                ..
            } = feed;
            drop(histories);
            showing.send((Some("r".to_string()), true)).unwrap();
            run_feed_loop(RunLoader::default(), ends, Duration::ZERO);
            assert!(snapshots.borrow().is_some(), "it sent before it stopped");

            // Nobody sending: two queued changes are taken, then the wait for
            // the next sees the sender gone.
            let (feed, ends) = RunFeed::new();
            let RunFeed {
                snapshots,
                histories,
                showing,
                ..
            } = feed;
            showing.send((Some("queued".to_string()), false)).unwrap();
            showing.send((Some("latest".to_string()), false)).unwrap();
            drop(showing);
            run_feed_loop(RunLoader::default(), ends, Duration::ZERO);
            assert!(snapshots.borrow().is_some(), "it sent before it stopped");
            assert!(histories.try_recv().is_err(), "no history was wanted");
        });
    }
}
