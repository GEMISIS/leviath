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
                args,
                parent_run_id,
                max_depth,
                reply,
            } => {
                let _ = reply.send(self.spawn_child(*args, &parent_run_id, max_depth));
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
                self.resolve_or_reload(&run_id);
                let ok = self
                    .world
                    .send_message(AgentMessage {
                        agent_id: run_id,
                        content,
                        target_region,
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

    /// Spawn a child agent under `parent_run_id`, linking `ParentRef` /
    /// `SubAgentChildren` and registering its run id. `Err` if the parent is not
    /// live, the depth limit is reached, or the spawner rejects it.
    pub(super) fn spawn_child(
        &mut self,
        mut args: SpawnArgs,
        parent_run_id: &str,
        max_depth: usize,
    ) -> Result<String, String> {
        // Record the parentage so the child's run metadata nests it in the tree.
        args.parent_run_id = Some(parent_run_id.to_string());
        let parent = self
            .live_entity(parent_run_id)
            .ok_or_else(|| format!("parent run '{parent_run_id}' is not live"))?
            // Same world as the child about to be spawned into it, so the raw
            // entity is what the ECS links want.
            .entity();
        let parent_depth = self
            .world
            .world()
            .get::<ParentRef>(parent)
            .map_or(0, |p| p.depth);
        let child_depth = parent_depth + 1;
        if child_depth > max_depth {
            return Err(format!(
                "sub-agent depth limit ({max_depth}) reached; not spawning deeper"
            ));
        }
        let run_id = args.run_id.clone();
        let child = match self.spawner.as_mut() {
            Some(spawner) => spawner(&mut self.world, &args)?,
            None => return Err("this daemon cannot spawn agents".to_string()),
        };
        let world = self.world.world_mut();
        world.entity_mut(child).insert(ParentRef {
            parent_entity: parent,
            parent_agent_id: parent_run_id.to_string(),
            depth: child_depth,
        });
        match world.get_mut::<SubAgentChildren>(parent) {
            Some(mut kids) => kids.children.push(child),
            None => {
                world.entity_mut(parent).insert(SubAgentChildren {
                    children: vec![child],
                    max_child_depth: max_depth,
                });
            }
        }
        // Record the child's run-id on the parent's serializable state so the
        // tree is persisted (and restart can rebuild `SubAgentChildren`). A
        // spawning parent always carries `AgentState`.
        world
            .get_mut::<crate::components::AgentState>(parent)
            .expect("a spawning parent always has AgentState")
            .spawned_children_ids
            .push(run_id.clone());
        // Seed the child's context from the parent per any declared blueprint
        // context transform (planner→coder region mapping, etc.).
        crate::context_transform::apply_context_transforms(
            world,
            crate::world::AgentId::in_world(world, parent),
            crate::world::AgentId::in_world(world, child),
        );
        // The spawner ran against this world, so the child is ours.
        let child_agent = self.world.own_agent(child);
        self.by_run_id.insert(run_id.clone(), child_agent);
        Ok(run_id)
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
        let (Some(target), Some(root)) = (
            self.resolve_or_reload(run_id),
            self.resolve_or_reload(ancestor),
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
        let Some(root) = self.resolve_or_reload(run_id) else {
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
        let Some(root) = self.resolve_or_reload(run_id) else {
            return false;
        };
        let mut acted = was_unloaded;
        for e in self.subtree(root.entity()) {
            acted |= self.world.resume(self.world.own_agent(e));
            if let Some(mut fan_out) = self.world.world_mut().get_mut::<FanOutWaiting>(e) {
                acted |= fan_out.set_paused(false);
            }
        }
        acted
    }

    pub(super) fn cancel_tree(&mut self, run_id: &str) -> bool {
        let Some(root) = self.resolve_or_reload(run_id) else {
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
                // The hub is keyed by agent id but the emitted-interaction set is
                // keyed by request id, so drop the ids that are no longer pending.
                let still_open: HashSet<String> = self
                    .interactions
                    .pending()
                    .into_iter()
                    .map(|(_, req)| req.id)
                    .collect();
                self.emitted_interactions
                    .retain(|id| still_open.contains(id));
            }
        }
        cancelled
    }

    /// A sender for [`SubAgentOp`]s. The daemon hands a clone to each agent's tool
    /// state so the sub-agent tools can reach the world through the host.
    pub fn subagent_sender(&self) -> UnboundedSender<SubAgentOp> {
        self.subagent_tx.clone()
    }
}
