//! Continuing a finished run: in place, or as a new run forked from it.
//!
//! Reading the run back, reseeding it and binding it are I/O, so the
//! installed [`Continuer`] does them off the serve loop, as a page-in does,
//! after the persistence lane has written everything it held for the run.
//! What comes back is placed, and [`crate::continuation::apply`] continues
//! it. The caller hears back once the step that reopened the run is on disk,
//! so a continuation it was told started survives a restart.
//!
//! One continuation of a run at a time: while one is under way, a second is
//! refused as a conflict, and every other op naming the run waits for it to
//! land. A run that has finished but is still in the world (it is unloaded a
//! pass or two later) is continued once it has been unloaded.

use std::future::Future;
use std::pin::Pin;

use super::*;
use crate::continuation::ContinueMessage;
use crate::persistence_bridge::Appended;
use crate::spec::issues::{IssueCode, SpawnIssues, SpecPath};
use tokio::sync::oneshot;

/// What continuing a run asks for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContinueRequest {
    /// The message the new segment starts with.
    pub message: ContinueMessage,
    /// Start a new run carrying the finished one's context, leaving the
    /// finished run as it is, rather than continuing it in place.
    pub fork: bool,
}

/// A continuation that started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Continued {
    /// The run that is going again: the new run's id for a fork.
    pub run_id: String,
    /// The segment that started.
    pub segment: u32,
    /// The run it was forked from. `None` for a run continued in place.
    pub forked_from: Option<String>,
}

/// Puts a continued run in the world: its id, and its entity.
pub type PlaceContinued =
    Box<dyn FnOnce(&mut PipelineWorld) -> Result<(String, AgentId), SpawnIssues> + Send>;

/// A continuation being read back, reseeded and bound off the loop.
pub type ContinueTask = Pin<Box<dyn Future<Output = Result<PlaceContinued, SpawnIssues>> + Send>>;

/// Resolves once the persistence lane has written everything it held for the
/// run, so the run's file can be read as it stands.
pub type LaneWritten = Pin<Box<dyn Future<Output = ()> + Send>>;

/// One continuation for the [`Continuer`] to prepare.
pub struct ContinueJob {
    /// The finished run.
    pub run_id: String,
    /// The message, and whether to fork.
    pub request: ContinueRequest,
    /// Resolves once the run's file holds every step of it: read it after.
    pub written: LaneWritten,
}

/// The daemon-installed function that prepares a continuation off the serve
/// loop: reads the finished run back, writes a fork's new run file when one
/// is asked for, reseeds and binds, and returns how to place it. Installed
/// with [`WorldHost::set_continuer`]; without one every continuation is
/// refused.
pub type Continuer = Box<dyn FnMut(ContinueJob) -> ContinueTask + Send>;

/// Who hears how a continuation went: the front door that asked for it.
pub(super) enum ContinueReply {
    /// `ControlOp::Continue`.
    Continue(oneshot::Sender<Result<Continued, SpawnIssues>>),
    /// A message sent to a finished run, which continues it.
    Message(oneshot::Sender<Result<bool, String>>),
}

impl ContinueReply {
    fn send(self, outcome: Result<Continued, SpawnIssues>) {
        match self {
            Self::Continue(reply) => {
                let _ = reply.send(outcome);
            }
            Self::Message(reply) => {
                let outcome = outcome
                    .map(|_| true)
                    .map_err(|issues| crate::continuation::refusal_text(&issues));
                let _ = reply.send(outcome);
            }
        }
    }
}

/// A continuation that finished off the loop.
pub(super) struct ContinueLanded {
    run_id: String,
    fork: bool,
    result: Result<PlaceContinued, SpawnIssues>,
    reply: ContinueReply,
}

/// A continuation's refusal at the root of the request.
fn refusal(code: IssueCode, message: impl Into<String>) -> SpawnIssues {
    crate::continuation::refused(code, SpecPath::root(), message)
}

impl WorldHost {
    /// Install what continues this host's finished runs.
    pub fn set_continuer(&mut self, continuer: Continuer) {
        self.continuer = Some(continuer);
    }

    /// Continue the run `run_id` as `request` asks, answering `reply`.
    pub(super) fn continue_run(
        &mut self,
        run_id: String,
        request: ContinueRequest,
        reply: ContinueReply,
    ) {
        if let Some(agent) = self.live_entity(&run_id) {
            return self.continue_live(run_id, agent, request, reply);
        }
        if self.continuing.contains_key(&run_id) {
            return reply.send(Err(refusal(
                IssueCode::Conflict,
                format!("run '{run_id}' is already being continued by another request"),
            )));
        }
        let Some(continuer) = self.continuer.as_mut() else {
            return reply.send(Err(refusal(
                IssueCode::Unavailable,
                "this host cannot continue runs",
            )));
        };
        let fork = request.fork;
        let written = crate::continuation::lane_barrier(self.world.world(), &run_id);
        let job = continuer(ContinueJob {
            run_id: run_id.clone(),
            request,
            written: Box::pin(async move {
                let _ = written.await;
            }),
        });
        // A fork only reads the run, so any number may run at once, and
        // nothing else about the run has to wait for one.
        if !fork {
            self.continuing.insert(run_id.clone(), Vec::new());
        }
        self.continuations += 1;
        let tx = self.continued_tx.clone();
        tokio::spawn(async move {
            // On a task of its own, so a continuer that panics fails this
            // continuation and nothing else.
            let result = tokio::spawn(job).await.unwrap_or_else(|_| {
                Err(refusal(
                    IssueCode::Unavailable,
                    "preparing the continuation panicked",
                ))
            });
            let _ = tx.send(ContinueLanded {
                run_id,
                fork,
                result,
                reply,
            });
        });
    }

    /// A continuation of a run the world still holds: refused while it is
    /// going, and for a sub-agent; one that has finished is continued once
    /// it has been unloaded, when its last step is on its way to disk.
    fn continue_live(
        &mut self,
        run_id: String,
        agent: AgentId,
        request: ContinueRequest,
        reply: ContinueReply,
    ) {
        let world = self.world.world();
        let finished = world
            .get::<AgentState>(agent.entity())
            .is_some_and(|s| crate::pipeline::is_terminal_status(&s.status));
        if !finished {
            return reply.send(Err(refusal(
                IssueCode::Conflict,
                format!(
                    "run '{run_id}' has not finished; send it a message, or wait for it to finish"
                ),
            )));
        }
        if let Some(parent) = world.get::<ParentRef>(agent.entity()) {
            return reply.send(Err(refusal(
                IssueCode::NotAllowed,
                format!(
                    "run '{run_id}' is a sub-agent of run '{}'; continue the run that started it",
                    parent.parent_agent_id
                ),
            )));
        }
        self.after_unload
            .entry(run_id)
            .or_default()
            .push((request, reply));
    }

    /// Continue every run that waited for `run_id` to be unloaded.
    pub(super) fn unloaded(&mut self, run_id: &str) {
        for (request, reply) in self.after_unload.remove(run_id).unwrap_or_default() {
            self.continue_run(run_id.to_string(), request, reply);
        }
    }

    /// A continuation landed: place the run, continue it, and answer once the
    /// step that reopened it is on disk. Then handle every op that waited on
    /// the run.
    pub(super) fn landed_continuation(&mut self, landed: ContinueLanded) {
        let ContinueLanded {
            run_id,
            fork,
            result,
            reply,
        } = landed;
        self.continuations = self.continuations.saturating_sub(1);
        let held = match fork {
            true => Vec::new(),
            false => self.continuing.remove(&run_id).unwrap_or_default(),
        };
        match result.and_then(|place| self.place_continued(place)) {
            Ok((run_id, applied)) => self.answer_when_written(run_id, applied, reply),
            Err(issues) => reply.send(Err(issues)),
        }
        for op in held {
            self.page_first(op);
        }
        self.world.wake_handle().notify_one();
    }

    /// Place what a continuation came back with, and continue it.
    fn place_continued(
        &mut self,
        place: PlaceContinued,
    ) -> Result<(String, crate::continuation::Applied), SpawnIssues> {
        // Placing runs outside the pipeline schedule, so a binding that
        // panics as it is applied would otherwise take the daemon with it.
        let placed =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| place(&mut self.world)))
                .unwrap_or_else(|_| {
                    Err(refusal(
                        IssueCode::Unavailable,
                        "placing the continued run panicked",
                    ))
                });
        let (run_id, agent) = placed?;
        self.register(run_id.clone(), agent);
        self.finished.retain(|(_, entry)| entry.run_id != run_id);
        let applied = crate::continuation::apply(self.world.world_mut(), agent.entity())
            .ok_or_else(|| {
                refusal(
                    IssueCode::Unavailable,
                    "the run was placed without its continuation",
                )
            })?;
        Ok((run_id, applied))
    }

    /// Answer `reply` once the step that reopened `run_id` has landed, and
    /// say on the event stream that the segment started.
    fn answer_when_written(
        &self,
        run_id: String,
        applied: crate::continuation::Applied,
        reply: ContinueReply,
    ) {
        let events = self.events.clone();
        tokio::spawn(async move {
            let crate::continuation::Applied {
                segment,
                stage,
                forked_from,
                landed,
            } = applied;
            match landed.await {
                Ok(Appended::Landed { .. } | Appended::NoJournal) => {
                    let _ = events.send(WorldEvent::Continued {
                        run_id: run_id.clone(),
                        agent_id: run_id.clone(),
                        segment,
                        stage,
                        forked_from: forked_from.clone(),
                    });
                    reply.send(Ok(Continued {
                        run_id,
                        segment,
                        forked_from,
                    }));
                }
                Ok(Appended::Failed) => reply.send(Err(refusal(
                    IssueCode::Unavailable,
                    format!(
                        "the step that reopened run '{run_id}' could not be written, so the run \
                         is failed for it"
                    ),
                ))),
                Err(_) => reply.send(Err(refusal(
                    IssueCode::Unavailable,
                    format!(
                        "the daemon stopped before the step that reopened run '{run_id}' was \
                         written"
                    ),
                ))),
            }
        });
    }
}

#[cfg(test)]
#[path = "continuing_tests.rs"]
mod tests;
