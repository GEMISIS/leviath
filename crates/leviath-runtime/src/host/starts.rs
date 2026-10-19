//! Starting runs: a request goes off the world to be resolved, recorded and
//! bound, and what comes back is placed.
//!
//! The work that can take seconds (reading a blueprint, choosing models,
//! running seeds, writing the run file, connecting what the run binds to)
//! happens on a task of its own through the installed [`RunStarter`]. The
//! world keeps ticking meanwhile. Each finished start comes back to the serve
//! loop as a [`Started`], and placing it is an [`insert`], which cannot fail,
//! followed by the parent links for a child run.
//!
//! [`insert`]: crate::insert::insert

use super::*;
use crate::spec::env::Caller;
use crate::spec::issues::SpawnIssues;

use crate::spec::request::SpawnRequest;
use crate::spec::summary::SpawnSummary;
use crate::state::RunState;
use tokio::sync::oneshot;

/// A start that has finished off the world.
pub(super) struct Started {
    /// What the starter made of the request.
    result: Result<PreparedRun, SpawnIssues>,
    /// The run that asked for it, for a child run.
    parent: Option<String>,
    /// Who is waiting to hear how it went.
    reply: oneshot::Sender<Result<crate::spec::summary::Spawned, SpawnIssues>>,
    /// The starter that made it, which brings the world up to date before
    /// the run is placed in it.
    starter: Arc<dyn RunStarter>,
    /// The idempotency key the start holds while it runs, when it was sent
    /// under one.
    claim: Option<crate::spec::names::IdempotencyKey>,
}

impl WorldHost {
    /// Send `request` off to be started for `caller`. The reply comes once the
    /// run is placed, or with every reason it was refused. A request under an
    /// idempotency key that is already held is answered by what holds it
    /// instead (see `host::claims`).
    pub(super) fn start(
        &mut self,
        request: SpawnRequest,
        caller: Caller,
        parent: Option<String>,
        reply: oneshot::Sender<Result<crate::spec::summary::Spawned, SpawnIssues>>,
    ) {
        let Some(starter) = self.starter.clone() else {
            let _ = reply.send(Err(host_refusal("this host cannot start runs")));
            return;
        };
        let asked = request.idempotency();
        let reply = match &asked {
            Some(asked) => {
                let runs = self.world.runs_dir();
                match self
                    .claims
                    .admit(asked, reply, &|run| claims::record_exists(runs, run))
                {
                    Some(reply) => reply,
                    None => return,
                }
            }
            None => reply,
        };
        let claim = asked.map(|asked| asked.key);
        let tx = self.started_tx.clone();
        self.starting += 1;
        tokio::spawn(async move {
            // On a task of its own, so a starter that panics fails this one
            // start and nothing else.
            let starting = starter.clone();
            let job = tokio::spawn(async move { starting.start(request, caller).await });
            let result = job
                .await
                .unwrap_or_else(|_| Err(host_refusal("starting the run panicked")));
            let _ = tx.send(Started {
                result,
                parent,
                reply,
                starter,
                claim,
            });
        });
    }

    /// Resolve `request` for `caller` without starting it.
    pub(super) fn validate(
        &mut self,
        request: SpawnRequest,
        caller: Caller,
        reply: oneshot::Sender<Result<SpawnSummary, SpawnIssues>>,
    ) {
        let Some(starter) = self.starter.clone() else {
            let _ = reply.send(Err(host_refusal("this host cannot start runs")));
            return;
        };
        tokio::spawn(async move {
            let job = tokio::spawn(async move { starter.check(request, caller).await });
            let result = job
                .await
                .unwrap_or_else(|_| Err(host_refusal("checking the run panicked")));
            let _ = reply.send(result);
        });
    }

    /// Answer `reply` with a run's state: live when the world holds it, else
    /// the last one its run file recorded. The file is read on the blocking
    /// pool, so the world loop does not wait on it: a run file is a run's
    /// whole history.
    pub(super) fn inspect(&self, run_id: &str, reply: oneshot::Sender<Option<Box<RunState>>>) {
        let live = self
            .live_entity(run_id)
            .and_then(|agent| crate::state::inspect::inspect(self.world.world(), agent.entity()));
        let path = self
            .world
            .runs_dir()
            .map(|dir| dir.join(run_id).join(leviath_core::files::RUN_FILE));
        let (Some(path), None) = (path, &live) else {
            let _ = reply.send(live.map(Box::new));
            return;
        };
        tokio::task::spawn_blocking(move || {
            let recorded = crate::runfile::RunFileTail::read(&path)
                .map(|tail| tail.state)
                .ok();
            let _ = reply.send(recorded.map(Box::new));
        });
    }

    /// Place a finished start in the world and answer whoever asked for it,
    /// and whoever sent the same request under its key while it ran.
    pub(super) fn place(&mut self, started: Started) {
        self.starting = self.starting.saturating_sub(1);
        let Started {
            result,
            parent,
            reply,
            starter,
            claim,
        } = started;
        let outcome = match result {
            Ok(prepared) => self.insert_started(prepared, parent, starter),
            Err(issues) => {
                // The refusal goes back over the socket to a caller that may
                // already have gone, so the daemon's log keeps its own copy.
                tracing::error!(issues = %issues, "a run was refused at its start");
                Err(issues)
            }
        };
        if let Some(key) = claim {
            self.claims.settle(&key, &outcome);
        }
        let _ = reply.send(outcome);
    }

    /// Insert a started run in the world, under the run that asked for it
    /// when there is one.
    fn insert_started(
        &mut self,
        prepared: PreparedRun,
        parent: Option<String>,
        starter: Arc<dyn RunStarter>,
    ) -> Result<crate::spec::summary::Spawned, SpawnIssues> {
        let run_id = prepared.spec.run_id.clone();
        let warnings = prepared.spec.warnings();
        // Said here as well as to the caller, who may not be watching: a run
        // that loops for ever is found by reading the daemon's log.
        let said = warnings
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        if !warnings.is_empty() {
            tracing::warn!(run_id = %run_id, warnings = %said, "a run started that may never finish");
        }
        // Insertion runs outside the pipeline schedule, so it is not covered by
        // `run_isolated`'s panic guard: a binding that panics as it is applied
        // would otherwise unwind the serve task and take the daemon with it.
        let placed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            starter.before_insert(&mut self.world, &prepared.spec);
            crate::insert::insert(
                self.world.world_mut(),
                prepared.spec,
                prepared.bindings,
                &prepared.state,
            )
        }));
        let Ok(entity) = placed else {
            tracing::error!(run_id = %run_id, "placing a started run panicked");
            // Its record may already be written; it says the run never
            // started, so a restart does not hand its key back to it.
            if let Some(runs) = self.world.runs_dir() {
                let record = runs
                    .join(run_id.as_str())
                    .join(leviath_core::files::RUN_FILE);
                let _ = claims::mark_refused(&record);
            }
            return Err(host_refusal("placing the run panicked"));
        };
        let agent = self.world.own_agent(entity);
        self.register(run_id.to_string(), agent);
        self.world.wake_handle().notify_one();
        if let Some(parent) = parent {
            self.link_child(&parent, entity);
        }
        Ok(crate::spec::summary::Spawned {
            run_id,
            warnings,
            deduplicated: false,
        })
    }

    /// Place every start still out, waiting for each to come back. For a
    /// caller driving the host itself rather than through [`Self::serve`].
    pub async fn finish_starts(&mut self) {
        // The host holds a sender, so the channel never closes under it.
        while self.starting > 0
            && let Some(started) = self.started_rx.recv().await
        {
            self.place(started);
        }
    }
}
