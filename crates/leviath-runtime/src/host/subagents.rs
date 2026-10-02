//! Sub-agent operations, which are the only control ops an *agent* can issue.
//!
//! Spawning a child, checking on one, cancelling a subtree. Kept apart from the
//! control loop because these arrive from inside the world (an agent's tool
//! call) rather than from a client, and the tree walks they need - ancestry,
//! cancellation - exist nowhere else. The channel they arrive on is handed out
//! here too, so the sender and its only reader are in one file.

use super::*;
use crate::fanout::FanOutWaiting;

impl WorldHost {
    /// Service one [`SubAgentOp`] from a tool lane, replying on its oneshot.
    pub(super) fn handle_subagent(&mut self, op: SubAgentOp) {
        match op {
            SubAgentOp::Spawn {
                request,
                parent_run_id,
                reply,
            } => match self.child_caller(&parent_run_id) {
                Ok(caller) => self.start(*request, caller, Some(parent_run_id), reply),
                Err(issues) => {
                    let _ = reply.send(Err(issues));
                }
            },
            SubAgentOp::Validate {
                request,
                parent_run_id,
                reply,
            } => match self.child_caller(&parent_run_id) {
                Ok(caller) => self.validate(*request, caller, reply),
                Err(issues) => {
                    let _ = reply.send(Err(issues));
                }
            },
            SubAgentOp::History {
                run_id,
                caller_run_id,
                at,
                reply,
            } => {
                let _ = reply.send(self.history(&run_id, &caller_run_id, at));
            }
            SubAgentOp::Check { run_id, reply } => {
                let report = self.live_entity(&run_id).and_then(|agent| {
                    self.world.agent_status(agent).map(|status| SubAgentReport {
                        status,
                        final_output: self
                            .world
                            .world()
                            .get::<crate::persistence::FinalOutput>(agent.entity())
                            .map(|o| o.0.clone()),
                    })
                });
                let _ = reply.send(report);
            }
            SubAgentOp::Send {
                run_id,
                caller_run_id,
                content,
                target_region,
                reply,
            } => {
                if !self.is_within_tree(&run_id, &caller_run_id) {
                    let _ = reply.send(false);
                    return;
                }
                // Page the target in if it was unloaded, so delivery finds it.
                let _ = self.resolve_or_reload(&run_id, PageIn::Address);
                let ok = self
                    .world
                    .send_message(AgentMessage {
                        agent_id: run_id,
                        from: caller_run_id,
                        content,
                        target_region,
                        // A sub-agent's `send_message` carries text only.
                        parts: Vec::new(),
                    })
                    .is_ok();
                let _ = reply.send(ok);
            }
            SubAgentOp::Kill {
                run_id,
                caller_run_id,
                reply,
            } => {
                let within = self.is_within_tree(&run_id, &caller_run_id);
                let _ = reply.send(within && self.cancel_tree(&run_id));
            }
        }
    }

    /// What `run_history` reads of `run_id` for `caller_run_id`: the run's
    /// spec in brief, its state (now, or as of step `at`), and every edge it
    /// took, all from its run file except a live run's current state, which
    /// is read off the world.
    ///
    /// Only the caller's own tree can be read: the caller, the runs it
    /// started, and theirs. An agent has no business reading an unrelated
    /// run, which may hold another person's work; an operator reads any run
    /// with `lev run show`. The tree is walked through each run's recorded
    /// children, so a finished child that is no longer in the world is still
    /// found, and nothing is paged back in to answer.
    pub(super) fn history(
        &self,
        run_id: &str,
        caller_run_id: &str,
        at: Option<u64>,
    ) -> Result<RunHistory, String> {
        if !self.in_recorded_tree(run_id, caller_run_id) {
            return Err(format!(
                "'{run_id}' is not this run or one it started. run_history reads only this \
                 run's own tree: itself, the runs it started, and theirs"
            ));
        }
        let path = self
            .world
            .runs_dir()
            .ok_or_else(|| "this host keeps no run files".to_string())?
            .join(run_id)
            .join(leviath_core::files::RUN_FILE);
        let reader = crate::runfile::RunFileReader::open(&path)
            .map_err(|e| format!("the run file of '{run_id}' cannot be read: {e}"))?;
        let last_seq = reader.last_seq();
        let (recorded, deltas) = reader
            .state_at(at.unwrap_or(last_seq))
            .and_then(|state| reader.deltas(0, last_seq).map(|deltas| (state, deltas)))
            .map_err(|e| e.to_string())?;
        // Now, for a run the world holds, is what the world holds.
        let live = match at {
            Some(_) => None,
            None => self.live_entity(run_id).and_then(|agent| {
                crate::state::inspect::inspect(self.world.world(), agent.entity())
            }),
        };
        let state = live.unwrap_or(recorded);
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
        })
    }

    /// Whether `run_id` is `ancestor` or a run below it, following each run's
    /// recorded children (live, or as its run file last had them).
    fn in_recorded_tree(&self, run_id: &str, ancestor: &str) -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![ancestor.to_string()];
        while let Some(id) = stack.pop() {
            if id == run_id {
                return true;
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(state) = self.inspect(&id) {
                stack.extend(state.children.iter().map(ToString::to_string));
            }
        }
        false
    }

    /// Who a child of `parent_run_id` is started for: that run, with the
    /// policy it was trusted with and where it sits in its tree. A parent the
    /// world does not hold, or one placed without a spec, cannot start one.
    fn child_caller(
        &self,
        parent_run_id: &str,
    ) -> Result<crate::spec::env::Caller, crate::spec::issues::SpawnIssues> {
        let parent = self
            .live_entity(parent_run_id)
            .ok_or_else(|| host_refusal(format!("parent run '{parent_run_id}' is not live")))?
            .entity();
        let world = self.world.world();
        let spec = world
            .get::<crate::insert::RunSpecC>(parent)
            .ok_or_else(|| {
                host_refusal(format!(
                    "parent run '{parent_run_id}' has no run spec to start a child from"
                ))
            })?
            .0
            .clone();
        let depth = world
            .get::<ParentRef>(parent)
            .map_or(usize::from(spec.placement.depth), |p| p.depth);
        Ok(crate::spec::env::Caller::Child {
            parent: spec.run_id.clone(),
            policy: spec.launch.clone(),
            depth: u8::try_from(depth).unwrap_or(u8::MAX),
        })
    }

    /// Link a placed child to the run that started it: `ParentRef` on the
    /// child, the child on the parent's `SubAgentChildren` and its persisted
    /// list of children, and the parent's context carried into the child by
    /// any context transform its graph declares. A parent that went away
    /// while the child was starting leaves the child standing on its own.
    pub(super) fn link_child(&mut self, parent_run_id: &str, child: Entity) {
        let Some(parent) = self.live_entity(parent_run_id).map(|a| a.entity()) else {
            tracing::warn!(parent = %parent_run_id, "a child run's parent went away while it started");
            return;
        };
        let world = self.world.world_mut();
        let depth = world.get::<ParentRef>(parent).map_or(0, |p| p.depth) + 1;
        let max_child_depth = world
            .get::<crate::insert::RunSpecC>(parent)
            .map_or(0, |s| usize::from(s.0.launch.max_depth));
        world.entity_mut(child).insert(ParentRef {
            parent_entity: parent,
            parent_agent_id: parent_run_id.to_string(),
            depth,
        });
        match world.get_mut::<SubAgentChildren>(parent) {
            Some(mut kids) => kids.children.push(child),
            None => {
                world.entity_mut(parent).insert(SubAgentChildren {
                    children: vec![child],
                    max_child_depth,
                });
            }
        }
        let child_id = world
            .get::<AgentState>(child)
            .map(|s| s.agent_id.clone())
            .unwrap_or_default();
        // Recorded on the parent's serializable state too, so the tree is in
        // the parent's run file and a restart can rebuild `SubAgentChildren`.
        world
            .get_mut::<AgentState>(parent)
            .into_iter()
            .for_each(|mut state| state.spawned_children_ids.push(child_id.clone()));
        crate::context_transform::apply_context_transforms(
            world,
            crate::world::AgentId::in_world(world, parent),
            crate::world::AgentId::in_world(world, child),
        );
    }

    /// Cancel a run and every descendant, paging the root in from disk first if it
    /// had been unloaded. Returns whether the run was found in the world.
    ///
    /// Cancelling only the root would leave its sub-agents and fan-out workers
    /// running - they are independent agents the schedule keeps driving, so they
    /// would carry on spending tokens with no parent to report to. Each cancelled
    /// agent's open interactions are closed too, so nothing is left blocked on a
    /// prompt for a run that is going away.
    /// Whether `run_id` is `ancestor` itself or one of its descendants.
    ///
    /// `send_to_agent` and `kill_agent` took any run id at all. Nothing tied the
    /// target to the caller, so an agent could cancel an unrelated run, inject
    /// text into its context, or - worst - hand it data: a message is added to
    /// the target as `Public` regardless of the sender's taint, so an agent
    /// holding `Private` context whose own outbound tools were gated could pass
    /// it to a sibling whose tools were not. That is a laundering channel
    /// straight through the middle of taint tracking.
    ///
    /// A downward walk from the caller, the same shape [`cancel_tree`] uses:
    /// parentage is recorded as `SubAgentChildren`, so "is it mine" is "is it in
    /// my subtree".
    ///
    /// [`cancel_tree`]: Self::cancel_tree
    pub(super) fn is_within_tree(&mut self, run_id: &str, ancestor: &str) -> bool {
        if run_id == ancestor {
            return true;
        }
        // Both ends as entities: the host already maps run ids to them, and
        // comparing entities avoids re-reading an id component per node.
        let (Ok(target), Ok(root)) = (
            self.resolve_or_reload(run_id, PageIn::Address),
            self.resolve_or_reload(ancestor, PageIn::Address),
        ) else {
            return false;
        };
        // `SubAgentChildren` links are raw entities within this world, so the
        // walk stays in that space and only the endpoints are world-scoped.
        let target = target.entity();
        let mut stack = vec![root.entity()];
        while let Some(e) = stack.pop() {
            if e == target {
                return true;
            }
            if let Some(kids) = self.world.world().get::<SubAgentChildren>(e) {
                stack.extend(kids.children.iter().copied());
            }
        }
        false
    }

    /// Every entity in `root`'s sub-agent tree, parent before children.
    fn subtree(&self, root: Entity) -> Vec<Entity> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(e) = stack.pop() {
            out.push(e);
            if let Some(kids) = self.world.world().get::<SubAgentChildren>(e) {
                stack.extend(kids.children.iter().copied());
            }
        }
        out
    }

    /// Pause a run and everything it spawned.
    ///
    /// Pausing a fan-out parent on its own does nothing a user would recognise
    /// as a pause: the parent is `Waiting` - a status the merge poll depends on,
    /// so `PipelineWorld::pause` rightly refuses to overwrite it - while the
    /// children that are actually burning tokens run on. So the request is
    /// applied to the whole tree, exactly as if each child had been paused by
    /// hand, and each fan-out parent is latched so the collector does not start
    /// the next queued worker behind the pause.
    ///
    /// Reports whether anything took, which is what tells a caller a
    /// still-running tree apart from one that was already finished.
    pub(super) fn pause_tree(&mut self, run_id: &str) -> bool {
        let Ok(root) = self.resolve_or_reload(run_id, PageIn::Address) else {
            return false;
        };
        let mut acted = false;
        for e in self.subtree(root.entity()) {
            acted |= self.world.pause(self.world.own_agent(e));
            if let Some(mut fan_out) = self.world.world_mut().get_mut::<FanOutWaiting>(e) {
                acted |= fan_out.set_paused(true);
            }
        }
        acted
    }

    /// Resume a run and everything it spawned.
    ///
    /// The mirror of [`Self::pause_tree`], and it must not give up on the root:
    /// a fan-out parent is `Waiting`, which `PipelineWorld::resume` refuses, so
    /// resuming the tree through the parent would otherwise report failure and
    /// leave every paused child paused with nothing left to resume them.
    pub(super) fn resume_tree(&mut self, run_id: &str) -> bool {
        // Whether this run had to come back from disk. Paging in a stopped run
        // restores it ready to work, so the `resume` calls below find nothing
        // paused and all report false - while the run is, in fact, going again.
        // Loading it back is the act of resuming it, so it counts as one.
        let was_unloaded = self.live_entity(run_id).is_none();
        let Ok(root) = self.resolve_or_reload(run_id, PageIn::Resume) else {
            return false;
        };
        let mut acted = was_unloaded;
        for e in self.subtree(root.entity()) {
            acted |= self.world.resume(self.world.own_agent(e));
            if let Some(mut fan_out) = self.world.world_mut().get_mut::<FanOutWaiting>(e) {
                acted |= fan_out.set_paused(false);
            }
            // Every agent in the tree, not just the one that was named: a
            // fan-out worker is as capable of being stuck on a permission as
            // its parent, and the pause stopped all of them.
            self.on_resumed(e);
        }
        acted
    }

    pub(super) fn cancel_tree(&mut self, run_id: &str) -> bool {
        let Ok(root) = self.resolve_or_reload(run_id, PageIn::Address) else {
            return false;
        };
        let mut cancelled = false;
        for e in self.subtree(root.entity()) {
            // Read the agent id before cancelling - the entity stays valid until
            // it is reaped, but reading first keeps this independent of that.
            let agent_id = self
                .world
                .world()
                .get::<AgentState>(e)
                .map(|s| s.agent_id.clone());
            cancelled |= self.world.cancel(self.world.own_agent(e));
            if let Some(agent_id) = agent_id {
                self.interactions.cancel_for_agent(&agent_id);
            }
        }
        cancelled
    }

    /// Cancel a run the world cannot hold, on its file. A run held out of
    /// the world for this machine moves from the held rows to the finished
    /// ones, cancelled.
    pub(super) fn force_cancel(&mut self, run_id: &str) -> bool {
        let forced = self
            .force_terminator
            .as_mut()
            .is_some_and(|terminate| terminate(run_id));
        if forced && let Some(mut entry) = self.parked.remove(run_id) {
            entry.status = AgentStatus::Cancelled;
            entry.wait_reason = None;
            self.record_finished(entry, chrono::Utc::now().timestamp());
        }
        forced
    }

    /// A sender for [`SubAgentOp`]s. The daemon hands a clone to each agent's tool
    /// state so the sub-agent tools can reach the world through the host.
    pub fn subagent_sender(&self) -> UnboundedSender<SubAgentOp> {
        self.subagent_tx.clone()
    }
}
