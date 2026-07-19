//! The world host: the daemon-side wrapper that owns a single [`PipelineWorld`],
//! maps stable **run ids** to ECS entities, and interleaves external **control
//! operations** with driving the world — all on one task, so there is never any
//! locking around the world.
//!
//! Clients (a control socket, the TUI, the CLI) don't hold entities — those are
//! generational indices meaningful only inside the world. They address agents by
//! run id. The host keeps the `run_id → Entity` map and turns each
//! [`ControlOp`] into the corresponding [`PipelineWorld`] call, replying on the
//! op's oneshot channel.
//!
//! The serve loop drives the world to quiescence, then parks until either an
//! async result wakes it, a control op arrives, or shutdown is signalled —
//! handling a control op and then re-driving to quiescence so its effect (a
//! resume, a delivered message) is applied immediately.

use std::collections::HashMap;

use bevy_ecs::entity::Entity;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::oneshot;

use crate::components::{AgentMessage, AgentState, AgentStatus};
use crate::world::PipelineWorld;

/// A control operation addressed to the host, each carrying a oneshot channel the
/// host replies on. Agents are addressed by run id.
pub enum ControlOp {
    /// The status of a run, or `None` if there is no such run.
    Status {
        /// The run to query.
        run_id: String,
        /// Reply channel.
        reply: oneshot::Sender<Option<AgentStatus>>,
    },
    /// Pause a run. Reply is `false` if there is no such (live) run.
    Pause {
        /// The run to pause.
        run_id: String,
        /// Reply channel.
        reply: oneshot::Sender<bool>,
    },
    /// Resume a paused run. Reply is `false` if there is no such (live) run.
    Resume {
        /// The run to resume.
        run_id: String,
        /// Reply channel.
        reply: oneshot::Sender<bool>,
    },
    /// Cancel a run. Reply is `false` if there is no such (live) run.
    Cancel {
        /// The run to cancel.
        run_id: String,
        /// Reply channel.
        reply: oneshot::Sender<bool>,
    },
    /// List every known live run and its status.
    List {
        /// Reply channel.
        reply: oneshot::Sender<Vec<(String, AgentStatus)>>,
    },
    /// Deliver a message to a running agent (by agent id). Reply is `false` if the
    /// world's message channel is closed.
    Message {
        /// Target agent id.
        agent_id: String,
        /// Message body.
        content: String,
        /// Optional target region (defaults to the conversation region).
        target_region: Option<String>,
        /// Reply channel.
        reply: oneshot::Sender<bool>,
    },
}

/// Owns the world and the run-id map; drives the world and services control ops.
pub struct WorldHost {
    world: PipelineWorld,
    by_run_id: HashMap<String, Entity>,
}

impl WorldHost {
    /// Wrap a world.
    pub fn new(world: PipelineWorld) -> Self {
        Self {
            world,
            by_run_id: HashMap::new(),
        }
    }

    /// Mutable access to the underlying world (for the spawner to add agents).
    pub fn world_mut(&mut self) -> &mut PipelineWorld {
        &mut self.world
    }

    /// Record the run-id → entity mapping for a freshly-spawned agent.
    pub fn register(&mut self, run_id: impl Into<String>, entity: Entity) {
        self.by_run_id.insert(run_id.into(), entity);
    }

    /// Resolve a run id to a **live** entity (one that still exists in the world).
    fn live_entity(&self, run_id: &str) -> Option<Entity> {
        let entity = *self.by_run_id.get(run_id)?;
        self.world.world().get::<AgentState>(entity).map(|_| entity)
    }

    /// List every known live run and its status.
    fn list(&self) -> Vec<(String, AgentStatus)> {
        self.by_run_id
            .iter()
            .filter_map(|(run_id, &entity)| {
                self.world
                    .world()
                    .get::<AgentState>(entity)
                    .map(|s| (run_id.clone(), s.status.clone()))
            })
            .collect()
    }

    /// Apply one control op and reply on its channel. A dropped reply receiver is
    /// harmless (the requester went away).
    pub fn handle(&mut self, op: ControlOp) {
        match op {
            ControlOp::Status { run_id, reply } => {
                let status = self
                    .live_entity(&run_id)
                    .and_then(|e| self.world.agent_status(e));
                let _ = reply.send(status);
            }
            ControlOp::Pause { run_id, reply } => {
                let ok = self
                    .live_entity(&run_id)
                    .is_some_and(|e| self.world.pause(e));
                let _ = reply.send(ok);
            }
            ControlOp::Resume { run_id, reply } => {
                let ok = self
                    .live_entity(&run_id)
                    .is_some_and(|e| self.world.resume(e));
                let _ = reply.send(ok);
            }
            ControlOp::Cancel { run_id, reply } => {
                let ok = self
                    .live_entity(&run_id)
                    .is_some_and(|e| self.world.cancel(e));
                let _ = reply.send(ok);
            }
            ControlOp::List { reply } => {
                let _ = reply.send(self.list());
            }
            ControlOp::Message {
                agent_id,
                content,
                target_region,
                reply,
            } => {
                let ok = self
                    .world
                    .send_message(AgentMessage {
                        agent_id,
                        content,
                        target_region,
                        priority: 0,
                    })
                    .is_ok();
                let _ = reply.send(ok);
            }
        }
    }

    /// Run the host: drive the world to quiescence, then park until an async
    /// result wakes it, a control op arrives, or shutdown is signalled. Returns
    /// when shutdown fires or the control channel closes.
    pub async fn serve(&mut self, mut control_rx: UnboundedReceiver<ControlOp>) {
        let wake = self.world.wake_handle();
        let shutdown = self.world.shutdown_handle();
        loop {
            self.world.run_to_fixed_point();
            tokio::select! {
                _ = wake.notified() => {}
                _ = shutdown.notified() => return,
                op = control_rx.recv() => {
                    match op {
                        Some(op) => self.handle(op),
                        None => return, // all control senders dropped
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference_pool::InferencePoolConfig;
    use crate::pipeline::{
        AgentBlueprint, ReadyToInfer, StageCursor, StageInference, StageInferences, StageProgress,
        StageSetup, StageSetups, ToolService, VisitCounts,
    };
    use crate::tool_bridge::BoxedToolExec;
    use leviath_core::{Region, RegionKind};
    use leviath_providers::{
        FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, Provider,
        ProviderError, TokenUsage,
    };
    use std::sync::Arc;
    use std::sync::Mutex;
    use tokio::runtime::Handle;
    use tokio::sync::mpsc;

    struct Script {
        responses: Mutex<std::collections::VecDeque<InferenceResponse>>,
    }
    #[async_trait::async_trait]
    impl Provider for Script {
        async fn infer(
            &self,
            _req: InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| ProviderError::Other("exhausted".to_string()))
        }
        fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "script"
        }
        fn capabilities(&self, _m: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    struct NoTools;
    impl ToolService for NoTools {
        fn exec_for(&self, _e: Entity, calls: Vec<leviath_providers::ToolCall>) -> BoxedToolExec {
            Box::new(move || {
                Box::pin(async move { calls.into_iter().map(|c| (c.id, String::new())).collect() })
            })
        }
    }

    fn text(content: &str) -> InferenceResponse {
        InferenceResponse {
            content: content.to_string(),
            tool_calls: vec![],
            tokens_used: TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            finish_reason: FinishReason::Complete,
        }
    }

    fn host_with(responses: Vec<InferenceResponse>) -> WorldHost {
        let mut registry = crate::engine::ProviderRegistry::new();
        registry.register(
            "script".to_string(),
            Arc::new(Script {
                responses: Mutex::new(responses.into_iter().collect()),
            }),
        );
        let world = PipelineWorld::new(
            registry,
            Arc::new(NoTools),
            InferencePoolConfig::new(),
            std::env::temp_dir(),
            Handle::current(),
        );
        WorldHost::new(world)
    }

    fn blueprint() -> leviath_core::Blueprint {
        let layout = leviath_core::layout::ContextLayout::new(
            vec![leviath_core::layout::RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::Clearable,
                10_000,
            )],
            12_000,
        );
        let s = leviath_core::Stage::new(
            "s".to_string(),
            leviath_core::blueprint::ModelConfig::new("script".to_string(), "m".to_string()),
        );
        leviath_core::Blueprint::new("t".to_string(), "d".to_string(), vec![s], layout)
    }

    fn window() -> crate::components::ContextWindow {
        let mut w = crate::components::ContextWindow::new(10_000);
        w.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        ));
        w
    }

    fn agent_state(agent_id: &str) -> AgentState {
        AgentState {
            agent_id: agent_id.to_string(),
            current_stage: "s".to_string(),
            iteration: 0,
            status: AgentStatus::Active,
            spawned_children_ids: vec![],
            pending_wait: None,
            accepts_messages: true,
        }
    }

    fn si() -> StageInference {
        StageInference {
            provider_name: "script".to_string(),
            model: "m".to_string(),
            tools: vec![],
            tool_filter: None,
        }
    }

    fn setup() -> StageSetup {
        StageSetup {
            inference_config: crate::components::InferenceConfig {
                temperature: None,
                max_output_tokens: None,
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            system_prompt: None,
        }
    }

    /// Spawn a simple agent into the host and register it under `run_id`.
    fn spawn(host: &mut WorldHost, run_id: &str, agent_id: &str) -> Entity {
        let e = host.world_mut().spawn_agent((
            AgentBlueprint(blueprint()),
            StageCursor { index: 0 },
            agent_state(agent_id),
            crate::components::MessageInbox::default(),
            StageProgress::default(),
            StageInferences(vec![si()]),
            StageSetups(vec![setup()]),
            VisitCounts::default(),
            window(),
            si(),
            setup().inference_config,
            ReadyToInfer,
        ));
        host.register(run_id, e);
        e
    }

    async fn ask<T>(host: &mut WorldHost, make: impl FnOnce(oneshot::Sender<T>) -> ControlOp) -> T {
        let (tx, rx) = oneshot::channel();
        host.handle(make(tx));
        rx.await.unwrap()
    }

    #[tokio::test]
    async fn status_and_list_reflect_registered_runs() {
        let mut host = host_with(vec![]);
        spawn(&mut host, "run-a", "agent-a");

        let status = ask(&mut host, |reply| ControlOp::Status {
            run_id: "run-a".to_string(),
            reply,
        })
        .await;
        assert_eq!(status, Some(AgentStatus::Active));

        let list = ask(&mut host, |reply| ControlOp::List { reply }).await;
        assert_eq!(list, vec![("run-a".to_string(), AgentStatus::Active)]);

        // Unknown run.
        let none = ask(&mut host, |reply| ControlOp::Status {
            run_id: "ghost".to_string(),
            reply,
        })
        .await;
        assert_eq!(none, None);
    }

    #[tokio::test]
    async fn pause_resume_cancel_by_run_id() {
        let mut host = host_with(vec![]);
        spawn(&mut host, "run-a", "agent-a");

        assert!(
            ask(&mut host, |reply| ControlOp::Pause {
                run_id: "run-a".to_string(),
                reply
            })
            .await
        );
        assert_eq!(
            host.world.agent_status(host.by_run_id["run-a"]),
            Some(AgentStatus::Idle)
        );

        assert!(
            ask(&mut host, |reply| ControlOp::Resume {
                run_id: "run-a".to_string(),
                reply
            })
            .await
        );
        assert!(
            ask(&mut host, |reply| ControlOp::Cancel {
                run_id: "run-a".to_string(),
                reply
            })
            .await
        );
        assert_eq!(
            host.world.agent_status(host.by_run_id["run-a"]),
            Some(AgentStatus::Cancelled)
        );

        // Unknown run ⇒ false.
        assert!(
            !ask(&mut host, |reply| ControlOp::Pause {
                run_id: "ghost".to_string(),
                reply
            })
            .await
        );
        assert!(
            !ask(&mut host, |reply| ControlOp::Resume {
                run_id: "ghost".to_string(),
                reply
            })
            .await
        );
        assert!(
            !ask(&mut host, |reply| ControlOp::Cancel {
                run_id: "ghost".to_string(),
                reply
            })
            .await
        );
    }

    #[tokio::test]
    async fn message_op_is_delivered() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "agent-a");

        let ok = ask(&mut host, |reply| ControlOp::Message {
            agent_id: "agent-a".to_string(),
            content: "hi".to_string(),
            target_region: Some("conversation".to_string()),
            reply,
        })
        .await;
        assert!(ok);

        // One tick delivers the message into context.
        host.world_mut().tick();
        assert!(
            host.world
                .world()
                .get::<crate::components::ContextWindow>(e)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens
                > 0
        );
    }

    #[tokio::test]
    async fn serve_drives_agents_and_handles_ops_until_shutdown() {
        let mut host = host_with(vec![text("t1"), text("t2"), text("t3"), text("t4")]);
        let e = spawn(&mut host, "run-a", "agent-a");
        let shutdown = host.world_mut().shutdown_handle();
        let (op_tx, op_rx) = mpsc::unbounded_channel();

        let handle = tokio::spawn(async move {
            host.serve(op_rx).await;
            host
        });

        // Query status via the live serve loop.
        let (tx, rx) = oneshot::channel();
        op_tx
            .send(ControlOp::Status {
                run_id: "run-a".to_string(),
                reply: tx,
            })
            .unwrap();
        let _ = rx.await.unwrap();

        shutdown.notify_one();
        let host = handle.await.unwrap();
        // The agent ran to completion under the serve loop.
        assert_eq!(host.world.agent_status(e), Some(AgentStatus::Complete));
    }

    #[tokio::test]
    async fn serve_returns_when_control_channel_closes() {
        let mut host = host_with(vec![text("done")]);
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        drop(op_tx); // close immediately
        host.serve(op_rx).await; // must return, not hang
    }

    #[tokio::test]
    async fn mock_helpers_are_exercised() {
        // Keep the test mocks' non-driven methods measured (metadata, the
        // exhausted-infer error path, and the no-op tool exec).
        let p = Script {
            responses: Mutex::new(std::collections::VecDeque::new()),
        };
        assert_eq!(p.name(), "script");
        assert_eq!(p.count_tokens("t", "m"), 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        let _ = p.capabilities("m");
        let req = InferenceRequest {
            system: vec![],
            messages: vec![],
            model: "m".to_string(),
            max_tokens: 1,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
        };
        assert!(p.infer(req).await.is_err()); // exhausted

        let exec = NoTools.exec_for(
            Entity::from_raw(1),
            vec![leviath_providers::ToolCall {
                id: "c".to_string(),
                name: "n".to_string(),
                arguments: serde_json::Value::Null,
            }],
        );
        assert_eq!(exec().await, vec![("c".to_string(), String::new())]);
    }

    #[tokio::test]
    async fn list_skips_despawned_entity() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "agent-a");
        // Despawn the entity behind the world's back; the run-id map is now stale.
        host.world_mut().world_mut().despawn(e);

        let list = ask(&mut host, |reply| ControlOp::List { reply }).await;
        assert!(list.is_empty()); // stale mapping filtered out
        let status = ask(&mut host, |reply| ControlOp::Status {
            run_id: "run-a".to_string(),
            reply,
        })
        .await;
        assert_eq!(status, None);
    }
}
