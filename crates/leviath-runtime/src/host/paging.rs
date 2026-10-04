//! Paging an unloaded run back in without stopping the world to do it.
//!
//! An op that names a run the world does not hold (a message, a pause, a
//! resume, a cancel, a sub-agent's send or kill) needs the run back first.
//! Reading its file and binding it to this machine is I/O, so the installed
//! [`Reloader`] does it in a [`PageJob`] off the serve loop, and the op waits,
//! held here, until the job lands. Placing what came back is a
//! [`PlacePage`], run on the world; then every op waiting on that run is
//! handled as though the run had been there all along. A job that is done by
//! the time it is asked (one that needed no waiting) is placed at once and its
//! op goes straight through.

use std::future::Future;
use std::pin::Pin;

use super::*;

/// Puts a paged-in run in the world, or says why it was not placed.
pub type PlacePage = Box<dyn FnOnce(&mut PipelineWorld) -> Result<AgentId, NotPlaced> + Send>;

/// A run being read back and bound off the loop.
pub type PageJob = Pin<Box<dyn Future<Output = Result<PlacePage, NotPlaced>> + Send>>;

/// An op held until the run it names is paged in.
pub(super) enum Deferred {
    /// A client's control op.
    Control(ControlOp),
    /// An agent's sub-agent op.
    Sub(SubAgentOp),
}

impl Deferred {
    /// The run this op needs in the world, and what it is wanted for. Ops
    /// that never page a run in name none.
    fn page_target(&self) -> Option<(&str, PageIn)> {
        match self {
            Deferred::Control(ControlOp::Message { agent_id, .. }) => {
                Some((agent_id, PageIn::Address))
            }
            Deferred::Control(
                ControlOp::Pause { run_id, .. } | ControlOp::Cancel { run_id, .. },
            ) => Some((run_id, PageIn::Address)),
            Deferred::Control(ControlOp::Resume { run_id, .. }) => Some((run_id, PageIn::Resume)),
            Deferred::Sub(SubAgentOp::Send { run_id, .. } | SubAgentOp::Kill { run_id, .. }) => {
                Some((run_id, PageIn::Address))
            }
            _ => None,
        }
    }
}

/// A page-in that finished off the loop.
pub(super) struct Paged {
    run_id: String,
    result: Result<PlacePage, NotPlaced>,
}

impl WorldHost {
    /// Hand `op` on to be handled now, or, when it names a run that has to
    /// be paged in first, start that and hold `op` until it lands.
    pub(super) fn page_first(&mut self, op: Deferred) {
        let Some((run_id, purpose)) = op.page_target().map(|(id, p)| (id.to_string(), p)) else {
            return self.handle_held(op);
        };
        if let Some(waiting) = self.paging.get_mut(&run_id) {
            waiting.push(op);
            return;
        }
        if self.live_entity(&run_id).is_some() {
            return self.handle_held(op);
        }
        let Some(reload) = self.reloader.as_mut() else {
            return self.handle_held(op);
        };
        let mut job = reload(&run_id, purpose);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        if let std::task::Poll::Ready(result) = job.as_mut().poll(&mut cx) {
            self.place_paged(&run_id, result);
            self.handle_held(op);
            self.paged.remove(&run_id);
            return;
        }
        let tx = self.paged_tx.clone();
        let id = run_id.clone();
        tokio::spawn(async move {
            let result = job.await;
            let _ = tx.send(Paged { run_id: id, result });
        });
        self.paging.insert(run_id, vec![op]);
    }

    /// A page-in landed: place the run, then handle every op that waited on it.
    pub(super) fn landed_page(&mut self, paged: Paged) {
        let Paged { run_id, result } = paged;
        self.place_paged(&run_id, result);
        for op in self.paging.remove(&run_id).unwrap_or_default() {
            self.handle_held(op);
        }
        self.paged.remove(&run_id);
    }

    /// Place what a page-in came back with, and keep how it went for the ops
    /// about to be handled: a run placed is registered and leaves the rows of
    /// runs held out of the world and of runs that finished; one held is held
    /// for the reason found just now.
    fn place_paged(&mut self, run_id: &str, result: Result<PlacePage, NotPlaced>) {
        let placed = result.and_then(|place| place(&mut self.world));
        match &placed {
            Ok(entity) => {
                self.by_run_id.insert(run_id.to_string(), *entity);
                self.parked.remove(run_id);
                self.finished.retain(|(_, entry)| entry.run_id != run_id);
            }
            Err(NotPlaced::Held(entry)) => {
                self.parked.insert(run_id.to_string(), (**entry).clone());
            }
            Err(_) => {}
        }
        self.paged.insert(run_id.to_string(), placed);
    }

    /// Handle an op whose run is in the world, or will not be.
    fn handle_held(&mut self, op: Deferred) {
        match op {
            Deferred::Control(op) => self.handle_now(op),
            Deferred::Sub(op) => self.handle_subagent_now(op),
        }
    }

    /// Wait for every page-in in flight to land, placing each and handling
    /// the ops it held, for a caller driving the host without its serve loop.
    pub async fn land_pages(&mut self) {
        while !self.paging.is_empty() {
            let paged = self.paged_rx.recv().await.expect("the host holds a sender");
            self.landed_page(paged);
        }
    }
}
