//! `run_history`: reading a run in the caller's own tree, off the serve loop.
//!
//! What the world holds (each live run's recorded children, and the live
//! state of the run asked about) is read on the loop, which is the only place
//! the world can be read. Everything else is a run file: the tree below a run
//! the world no longer holds, the spec, the state at a step, the edges taken
//! and the answer. Those are read on a blocking task, which replies.

use std::path::PathBuf;

use super::*;

/// One `run_history` read: what the world said about it, and where the run
/// files are.
pub(super) struct HistoryRead {
    /// The run to read.
    run_id: String,
    /// The run asking, whose tree it must be in.
    caller_run_id: String,
    /// The step to read the state at, or `None` for now.
    at: Option<u64>,
    /// Where run files are kept, when this host keeps any.
    runs_dir: Option<PathBuf>,
    /// Each run the world holds, with the children it recorded.
    live_children: HashMap<String, Vec<String>>,
    /// The run's state now, when the world holds it and no step was asked.
    live_now: Option<crate::state::RunState>,
}

impl WorldHost {
    /// Answer a `run_history` op: read what the world holds now, and the rest
    /// from run files on a task of its own.
    pub(super) fn history(
        &self,
        run_id: String,
        caller_run_id: String,
        at: Option<u64>,
        reply: tokio::sync::oneshot::Sender<Result<RunHistory, String>>,
    ) {
        let world = self.world.world();
        let live_children = self
            .by_run_id
            .iter()
            .filter(|(_, agent)| {
                world
                    .get::<crate::fanout::Slimmed>(agent.entity())
                    .is_none()
            })
            .filter_map(|(id, agent)| {
                world
                    .get::<AgentState>(agent.entity())
                    .map(|s| (id.clone(), s.spawned_children_ids.clone()))
            })
            .collect();
        let live_now = match at {
            Some(_) => None,
            None => self
                .live_entity(&run_id)
                .and_then(|agent| crate::state::inspect::inspect(world, agent.entity())),
        };
        let read = HistoryRead {
            run_id,
            caller_run_id,
            at,
            runs_dir: self.world.runs_dir().map(Into::into),
            live_children,
            live_now,
        };
        tokio::task::spawn_blocking(move || {
            let _ = reply.send(read.run());
        });
    }
}

impl HistoryRead {
    /// The run's spec in brief, its state (now, or as of step `at`), and every
    /// edge it took, all from its run file except a live run's current state,
    /// which the world gave.
    ///
    /// Only the caller's own tree can be read: the caller, the runs it
    /// started, and theirs. An agent has no business reading an unrelated
    /// run, which may hold another person's work; an operator reads any run
    /// with `lev run show`. The tree is walked through each run's recorded
    /// children, so a finished child that is no longer in the world is still
    /// found, and nothing is paged back in to answer.
    pub(super) fn run(self) -> Result<RunHistory, String> {
        let run_id = self.run_id.as_str();
        if !self.in_recorded_tree() {
            return Err(format!(
                "'{run_id}' is not this run or one it started. run_history reads only this \
                 run's own tree: itself, the runs it started, and theirs"
            ));
        }
        let path = self
            .runs_dir
            .as_ref()
            .ok_or_else(|| "this host keeps no run files".to_string())?
            .join(run_id)
            .join(leviath_core::files::RUN_FILE);
        let reader = crate::runfile::RunFileReader::open(&path)
            .map_err(|e| format!("the run file of '{run_id}' cannot be read: {e}"))?;
        let last_seq = reader.last_seq();
        let (recorded, deltas) = reader
            .state_at(self.at.unwrap_or(last_seq))
            .and_then(|state| reader.deltas(0, last_seq).map(|deltas| (state, deltas)))
            .map_err(|e| e.to_string())?;
        // Now, for a run the world holds, is what the world holds, with the
        // files beside its run file as its file last named them.
        let state = match self.live_now {
            Some(mut state) => {
                state.files = recorded.files;
                state.blobs = recorded.blobs;
                state
            }
            None => recorded,
        };
        let answer = state.files.final_output.as_ref().map(|file| {
            file.read(reader.dir())
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .map_err(|e| e.to_string())
        });
        let transitions = deltas
            .iter()
            .flat_map(|delta| {
                delta
                    .transitions()
                    .into_iter()
                    .map(|t| (delta.seq, t.clone()))
            })
            .collect();
        Ok(RunHistory {
            summary: crate::spec::summary::SpawnSummary::of(reader.spec()),
            state,
            last_seq,
            transitions,
            answer,
        })
    }

    /// Whether the run is the caller or a run below it, following each run's
    /// recorded children: live, or as its run file last had them.
    fn in_recorded_tree(&self) -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![self.caller_run_id.clone()];
        while let Some(id) = stack.pop() {
            if id == self.run_id {
                return true;
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            stack.extend(self.children_of(&id));
        }
        false
    }

    /// The children `run_id` recorded: as the world holds them, else as its
    /// run file last had them, else none.
    fn children_of(&self, run_id: &str) -> Vec<String> {
        if let Some(children) = self.live_children.get(run_id) {
            return children.clone();
        }
        self.runs_dir
            .as_ref()
            .map(|dir| dir.join(run_id).join(leviath_core::files::RUN_FILE))
            .and_then(|path| crate::runfile::RunFileReader::open(&path).ok())
            .and_then(|reader| reader.latest_state().ok())
            .map(|state| state.children.iter().map(ToString::to_string).collect())
            .unwrap_or_default()
    }
}
