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

use std::collections::{HashMap, HashSet};

use bevy_ecs::entity::Entity;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{broadcast, oneshot};

use crate::components::{
    AgentMessage, AgentState, AgentStatus, ContextWindow, ParentRef, SubAgentChildren,
};
use crate::interaction_hub::InteractionHub;
use crate::persistence::{RunMetadata, TokenTotals};
use crate::world::PipelineWorld;
use leviath_core::interaction::{InteractionRequest, InteractionResponse};
use serde::{Deserialize, Serialize};

/// The parameters for spawning an agent into the world. The runtime doesn't know
/// how to load blueprints or resolve tools — that policy lives in the
/// [`Spawner`] the daemon installs — so this just carries the raw request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SpawnArgs {
    /// The run id to give the new agent (its directory / control key).
    pub run_id: String,
    /// Path to the agent manifest directory or bundle.
    pub blueprint_path: String,
    /// The task prompt. Seeded into the region keyed `task` (see
    /// [`crate::context_setup::init_window_seeded`]); a matching `regions`
    /// entry, if present, overrides it.
    pub task: String,
    /// Literal seed content for named caller-input regions, keyed by the
    /// region's caller-input name. Merged over `task` at spawn. `#[serde(default)]`
    /// keeps older requests (which never sent this) deserializing to an empty map.
    #[serde(default)]
    pub regions: HashMap<String, String>,
    /// Optional model override (`provider/model` or `model`).
    #[serde(default)]
    pub model: Option<String>,
    /// Working directory for tool execution.
    pub workdir: String,
    /// Custom key/value metadata from the request.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    /// Webhook to POST on completion/error (surfaced in the run metadata).
    #[serde(default)]
    pub callback_url: Option<String>,
    /// Optional shared secret for HMAC-SHA256 signing the webhook body.
    #[serde(default)]
    pub callback_secret: Option<String>,
    /// Run this agent unattended (the `--yolo` launch override): approve every
    /// tool call, waive the taint gate, and auto-answer the agent's own prompts
    /// (`ask_user_*`, blueprint interaction points) rather than parking on the
    /// interaction hub for a person who isn't there.
    #[serde(default)]
    pub yolo: bool,
    /// Refuse this run's `seed = { command = ... }` regions (the
    /// `--no-seed-commands` launch override). Command seeds execute at spawn,
    /// before any approval prompt, so this is the per-run counterpart to the
    /// `[security] allow_seed_commands` config switch.
    #[serde(default)]
    pub no_seed_commands: bool,
    /// Tools to allow outright for this run (the `--allow` launch override).
    #[serde(default)]
    pub allow: Vec<String>,
    /// Override the blueprint's max sub-agent tree depth.
    #[serde(default)]
    pub max_depth: Option<usize>,
    /// The run id of this agent's parent, when it is a sub-agent / fan-out
    /// worker. Persisted in the run metadata so observers (dashboard, `serve`
    /// tree) can nest children under their parent. `None` for a top-level run.
    #[serde(default)]
    pub parent_run_id: Option<String>,
}

/// The daemon-installed function that turns [`SpawnArgs`] into a live agent:
/// loads the blueprint, resolves stages/tools, spawns into the world, and
/// returns the new entity (the host records the run-id mapping). Returns `Err`
/// with a human-readable message on failure.
pub type Spawner = Box<dyn FnMut(&mut PipelineWorld, &SpawnArgs) -> Result<Entity, String> + Send>;

/// The daemon-installed function that pages a previously-unloaded run back into
/// the world from its on-disk state: given a run id, it reloads the agent (its
/// blueprint, tool state, context, stage) and returns the new entity, or `None`
/// if there is no such resumable run on disk. Used for reload-on-demand — a
/// control/sub-agent op targeting a run that isn't currently in memory pages it
/// in first via the host's internal resolve-or-reload step. Installed with
/// [`WorldHost::set_reloader`].
pub type Reloader = Box<dyn FnMut(&mut PipelineWorld, &str) -> Option<Entity> + Send>;

/// The daemon-installed hook run just before a terminal agent's entity is
/// despawned (reaped). It receives the world and the entity while both are still
/// valid, so the daemon can release per-agent resources the runtime doesn't know
/// about — tearing down the agent's sandbox and dropping its tool state.
/// Installed with [`WorldHost::set_reaper`]; a no-op when none is set.
pub type Reaper = Box<dyn FnMut(&mut PipelineWorld, Entity) + Send>;

/// An async hook the host awaits *before* servicing a top-level `Spawn` control
/// op, so the daemon can do async preparation the sync spawner can't — e.g.
/// lazily connecting the blueprint's MCP servers into the shared pool (issue #97)
/// so they're warm by the time [`Spawner`] reads them. The returned future is
/// `'static` (it must clone anything it needs from the `SpawnArgs`). Installed
/// with [`WorldHost::set_spawn_preprocessor`]; when none is set, spawns proceed
/// straight to the spawner.
pub type SpawnPreprocessor = Box<
    dyn Fn(&SpawnArgs) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send,
>;

/// A world-access request from an agent's tool lane. The sub-agent tools
/// (`spawn_agent`/`check_agent`/`send_to_agent`/`kill_agent`) need the world and
/// the [`Spawner`], which only the host holds — the tool lane runs async, off the
/// world. Each carries a oneshot reply, so the (sequential) tool lane blocks on
/// the host applying it, mirroring the interaction hub.
pub enum SubAgentOp {
    /// Spawn a child agent from `args`, linked as a child of `parent_run_id`.
    /// Rejected if the child would exceed `max_depth`. Reply is the child run id.
    Spawn {
        /// The child's spawn parameters (blueprint path, task, etc.). Boxed
        /// because it is much larger than the other variants' payloads.
        args: Box<SpawnArgs>,
        /// The run id of the agent doing the spawning.
        parent_run_id: String,
        /// Maximum allowed sub-agent tree depth (root = 0).
        max_depth: usize,
        /// Reply: the child's run id, or an error message.
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Report a run's current status (`None` if the host has no such live run).
    Check {
        /// The run to query.
        run_id: String,
        /// Reply: the run's status.
        reply: oneshot::Sender<Option<AgentStatus>>,
    },
    /// Deliver a message into a running agent's inbox. Reply is whether a live
    /// agent accepted it.
    Send {
        /// The target run.
        run_id: String,
        /// The message body.
        content: String,
        /// Reply: whether the message was accepted.
        reply: oneshot::Sender<bool>,
    },
    /// Cancel a run and its whole sub-tree. Reply is whether any agent was found.
    Kill {
        /// The run to cancel (with its descendants).
        run_id: String,
        /// Reply: whether anything was cancelled.
        reply: oneshot::Sender<bool>,
    },
}

/// A control operation addressed to the host, each carrying a oneshot channel the
/// host replies on. Agents are addressed by run id.
pub enum ControlOp {
    /// Spawn a new agent. Reply is the run id on success, or an error message.
    Spawn {
        /// The spawn request. Boxed because it is much larger than the other
        /// variants' payloads.
        args: Box<SpawnArgs>,
        /// Reply channel.
        reply: oneshot::Sender<Result<String, String>>,
    },
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
    /// List every open interaction awaiting an answer, as `(agent_id, request)`.
    ListInteractions {
        /// Reply channel.
        reply: oneshot::Sender<Vec<(String, InteractionRequest)>>,
    },
    /// Answer an open interaction. Reply is `false` if no such request is open.
    AnswerInteraction {
        /// The answer (its `request_id` selects the interaction).
        response: InteractionResponse,
        /// Reply channel.
        reply: oneshot::Sender<bool>,
    },
    /// Cancel an open interaction (its asker wakes with a neutral response).
    /// Reply is `false` if no such request is open.
    CancelInteraction {
        /// The interaction id to cancel.
        request_id: String,
        /// Reply channel.
        reply: oneshot::Sender<bool>,
    },
    /// Shut the daemon down: signal the world's shutdown so the serve loop
    /// returns. Reply is sent (`true`) before the shutdown is triggered.
    Shutdown {
        /// Reply channel.
        reply: oneshot::Sender<bool>,
    },
}

/// A change in the world, broadcast to subscribers (the HTTP/WS gateway) so they
/// get pushed updates instead of polling. Emitted by the host as it drives the
/// world; streamed over the control transport via `ControlRequest::Subscribe`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WorldEvent {
    /// A run first appeared in the world.
    Spawned {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The blueprint / agent name.
        blueprint: String,
    },
    /// A run's status, stage, iteration, or tool-call count changed.
    Status {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// Short status label (`active`, `waiting`, `complete`, …).
        status: String,
        /// The current stage name.
        stage: String,
        /// The current iteration.
        iteration: usize,
        /// Cumulative tool calls.
        tool_calls: usize,
        /// Whether the current stage accepts messages.
        accepts_messages: bool,
    },
    /// A run's token totals changed.
    Tokens {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// Cumulative prompt tokens.
        prompt_tokens: usize,
        /// Cumulative completion tokens.
        completion_tokens: usize,
        /// Cumulative cached tokens.
        cached_tokens: usize,
        /// Cumulative cache-write tokens.
        cache_write_tokens: usize,
    },
    /// A run's context-window token usage changed.
    Context {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// Current context tokens.
        total_tokens: usize,
        /// Max context tokens.
        max_tokens: usize,
    },
    /// A run raised a new interaction awaiting an answer.
    Interaction {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The interaction request.
        request: InteractionRequest,
    },
    /// A run reached a terminal status.
    Completed {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The terminal status label.
        status: String,
    },
    /// A run produced a per-agent log/output line (readable assistant output or
    /// an operational `[Tokens: …]` / `[tool] …` / `[error] …` line).
    Log {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The log line text.
        line: String,
    },
}

/// A world resource holding a clone of the host's [`WorldEvent`] broadcast
/// sender, so ECS systems (e.g. the persistence drain) can push events — notably
/// per-agent [`WorldEvent::Log`] lines — into the same stream the control
/// transport serves. Absent in worlds that don't stream (test / `lev run`), where
/// systems that depend on it become no-ops.
#[derive(bevy_ecs::system::Resource, Clone)]
pub struct WorldEventSink(pub broadcast::Sender<WorldEvent>);

/// A short, stable status label for [`WorldEvent`].
fn status_str(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Active => "active",
        AgentStatus::Waiting => "waiting",
        AgentStatus::Complete => "complete",
        AgentStatus::Error { .. } => "error",
        AgentStatus::Cancelled => "cancelled",
    }
}

/// The last-emitted snapshot of an agent, for change detection.
#[derive(Clone)]
struct Emitted {
    status: &'static str,
    stage: String,
    iteration: usize,
    tool_calls: usize,
    accepts_messages: bool,
    prompt_tokens: usize,
    completion_tokens: usize,
    cached_tokens: usize,
    cache_write_tokens: usize,
    context_tokens: usize,
    terminal: bool,
}

/// Owns the world and the run-id map; drives the world and services control ops.
pub struct WorldHost {
    world: PipelineWorld,
    by_run_id: HashMap<String, Entity>,
    interactions: InteractionHub,
    spawner: Option<Spawner>,
    spawn_preprocessor: Option<SpawnPreprocessor>,
    reloader: Option<Reloader>,
    reaper: Option<Reaper>,
    events: broadcast::Sender<WorldEvent>,
    emitted: HashMap<String, Emitted>,
    emitted_interactions: HashSet<String>,
    /// Sub-agent world-access requests from tool lanes. The host holds a `tx`
    /// clone so the receiver never closes (its `recv` never yields `None`).
    subagent_tx: UnboundedSender<SubAgentOp>,
    subagent_rx: UnboundedReceiver<SubAgentOp>,
}

impl WorldHost {
    /// Wrap a world with a fresh interaction hub.
    pub fn new(world: PipelineWorld) -> Self {
        Self::with_interactions(world, InteractionHub::new())
    }

    /// Wrap a world with a specific interaction hub — the daemon shares one hub
    /// between the tool service's per-agent backends and this host.
    pub fn with_interactions(mut world: PipelineWorld, interactions: InteractionHub) -> Self {
        let (events, _) = broadcast::channel(1024);
        // Let ECS systems (the persistence drain) push events — per-agent log
        // lines — into the same stream the control transport serves.
        world
            .world_mut()
            .insert_resource(WorldEventSink(events.clone()));
        let (subagent_tx, subagent_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            world,
            by_run_id: HashMap::new(),
            interactions,
            spawner: None,
            spawn_preprocessor: None,
            reloader: None,
            reaper: None,
            events,
            emitted: HashMap::new(),
            emitted_interactions: HashSet::new(),
            subagent_tx,
            subagent_rx,
        }
    }

    /// A sender for [`SubAgentOp`]s. The daemon hands a clone to each agent's tool
    /// state so the sub-agent tools can reach the world through the host.
    pub fn subagent_sender(&self) -> UnboundedSender<SubAgentOp> {
        self.subagent_tx.clone()
    }

    /// Subscribe to [`WorldEvent`]s. The HTTP/WS gateway uses this (via the
    /// control transport's `Subscribe`) to push updates instead of polling.
    pub fn subscribe(&self) -> broadcast::Receiver<WorldEvent> {
        self.events.subscribe()
    }

    /// The world-event sender, handed to the control transport so a `Subscribe`
    /// connection can stream events.
    pub fn event_sender(&self) -> broadcast::Sender<WorldEvent> {
        self.events.clone()
    }

    /// Diff every registered run against its last-emitted snapshot and broadcast
    /// what changed (status/tokens/context/completion) plus any new interaction.
    /// Called after each drive to quiescence, so subscribers see every change.
    fn emit_events(&mut self) {
        let pairs: Vec<(String, Entity)> = self
            .by_run_id
            .iter()
            .map(|(k, &v)| (k.clone(), v))
            .collect();
        // Terminal agents to unload from memory this pass (their disk state is
        // preserved and still viewable). Collected during the loop, reaped after.
        let mut to_reap: Vec<(String, Entity)> = Vec::new();
        for (run_id, entity) in pairs {
            let Some(state) = self.world.world().get::<AgentState>(entity) else {
                continue; // reaped between registration and now
            };
            let agent_id = state.agent_id.clone();
            let status = status_str(&state.status);
            let terminal = matches!(
                state.status,
                AgentStatus::Complete | AgentStatus::Error { .. } | AgentStatus::Cancelled
            );
            let cur = {
                let totals = self
                    .world
                    .world()
                    .get::<TokenTotals>(entity)
                    .copied()
                    .unwrap_or_default();
                let (context_tokens, _) = self
                    .world
                    .world()
                    .get::<ContextWindow>(entity)
                    .map(|w| (w.current_tokens, w.max_tokens))
                    .unwrap_or((0, 0));
                Emitted {
                    status,
                    stage: state.current_stage.clone(),
                    iteration: state.iteration,
                    tool_calls: totals.tool_calls,
                    accepts_messages: state.accepts_messages,
                    prompt_tokens: totals.prompt_tokens,
                    completion_tokens: totals.completion_tokens,
                    cached_tokens: totals.cached_tokens,
                    cache_write_tokens: totals.cache_write_tokens,
                    context_tokens,
                    terminal,
                }
            };
            let max_tokens = self
                .world
                .world()
                .get::<ContextWindow>(entity)
                .map(|w| w.max_tokens)
                .unwrap_or(0);
            let prev = self.emitted.get(&run_id).cloned();

            if prev.is_none() {
                let blueprint = self
                    .world
                    .world()
                    .get::<RunMetadata>(entity)
                    .map(|m| m.agent_name.clone())
                    .unwrap_or_default();
                let _ = self.events.send(WorldEvent::Spawned {
                    run_id: run_id.clone(),
                    agent_id: agent_id.clone(),
                    blueprint,
                });
            }

            let status_key = |e: &Emitted| {
                (
                    e.status,
                    e.stage.clone(),
                    e.iteration,
                    e.tool_calls,
                    e.accepts_messages,
                )
            };
            if prev.as_ref().map(status_key) != Some(status_key(&cur)) {
                let _ = self.events.send(WorldEvent::Status {
                    run_id: run_id.clone(),
                    agent_id: agent_id.clone(),
                    status: status.to_string(),
                    stage: cur.stage.clone(),
                    iteration: cur.iteration,
                    tool_calls: cur.tool_calls,
                    accepts_messages: cur.accepts_messages,
                });
            }

            let token_key = |e: &Emitted| {
                (
                    e.prompt_tokens,
                    e.completion_tokens,
                    e.cached_tokens,
                    e.cache_write_tokens,
                )
            };
            if prev.as_ref().map(token_key) != Some(token_key(&cur)) {
                let _ = self.events.send(WorldEvent::Tokens {
                    run_id: run_id.clone(),
                    agent_id: agent_id.clone(),
                    prompt_tokens: cur.prompt_tokens,
                    completion_tokens: cur.completion_tokens,
                    cached_tokens: cur.cached_tokens,
                    cache_write_tokens: cur.cache_write_tokens,
                });
            }

            if prev.as_ref().map(|e| e.context_tokens) != Some(cur.context_tokens) {
                let _ = self.events.send(WorldEvent::Context {
                    run_id: run_id.clone(),
                    agent_id: agent_id.clone(),
                    total_tokens: cur.context_tokens,
                    max_tokens,
                });
            }

            let was_terminal = prev.as_ref().map(|e| e.terminal) == Some(true);
            if cur.terminal && !was_terminal {
                let _ = self.events.send(WorldEvent::Completed {
                    run_id: run_id.clone(),
                    agent_id: agent_id.clone(),
                    status: status.to_string(),
                });
            }
            // Unload a terminal agent once its terminal state has been emitted (a
            // prior pass already saw it terminal, so the event went out and the
            // persistence lane captured it) and no live parent still needs it.
            if cur.terminal && was_terminal && self.no_live_parent(entity) {
                to_reap.push((run_id.clone(), entity));
            }
            // NOTE: non-terminal `Waiting` agents are intentionally NOT unloaded.
            // Every `Waiting` state carries a live, unpersisted continuation — a
            // blocked `ask` future (`AwaitingInteraction`), running fan-out workers
            // (`FanOutWaiting`), or pending children (`WaitingForChildren`) — so
            // flushing one to disk and paging it back cannot resume it (in-flight
            // interactions aren't persisted; the blocked future is gone). Only
            // terminal agents, whose full state is on disk, are safe to reap.

            self.emitted.insert(run_id, cur);
        }

        // Reap: run the daemon's reap hook (sandbox teardown + tool-state drop)
        // while the entity is still valid, then despawn it and erase its host-map
        // entries. Iterating a snapshot of `by_run_id` above means removing here
        // is safe. The reaper is moved out for the loop to avoid borrowing `self`
        // twice, then restored.
        let mut reaper = self.reaper.take();
        for (run_id, entity) in to_reap {
            if let Some(reaper) = reaper.as_mut() {
                reaper(&mut self.world, entity);
            }
            self.world.world_mut().despawn(entity);
            self.by_run_id.remove(&run_id);
            self.emitted.remove(&run_id);
        }
        self.reaper = reaper;

        for (agent_id, request) in self.interactions.pending() {
            if self.emitted_interactions.insert(request.id.clone()) {
                let _ = self.events.send(WorldEvent::Interaction {
                    run_id: agent_id.clone(),
                    agent_id,
                    request,
                });
            }
        }
    }

    /// Whether a terminal agent is safe to unload: it has no **live** parent that
    /// might still be waiting on it. True for a root (no `ParentRef`), or when its
    /// parent has been despawned or is itself terminal; false while a non-terminal
    /// parent could still be gating on this child.
    fn no_live_parent(&self, entity: Entity) -> bool {
        let world = self.world.world();
        match world.get::<crate::components::ParentRef>(entity) {
            None => true,
            Some(parent_ref) => match world.get::<AgentState>(parent_ref.parent_entity) {
                None => true,
                Some(state) => matches!(
                    state.status,
                    AgentStatus::Complete | AgentStatus::Error { .. } | AgentStatus::Cancelled
                ),
            },
        }
    }

    /// Install the spawner used to service `Spawn` control ops. Without one, a
    /// `Spawn` op replies with an error.
    pub fn set_spawner(&mut self, spawner: Spawner) {
        self.spawner = Some(spawner);
    }

    /// Install the async hook awaited before each top-level `Spawn` (see
    /// [`SpawnPreprocessor`]).
    pub fn set_spawn_preprocessor(&mut self, pp: SpawnPreprocessor) {
        self.spawn_preprocessor = Some(pp);
    }

    /// Install the reloader used to page an unloaded run back in on demand.
    /// Without one, an op targeting a run that isn't in memory just misses.
    pub fn set_reloader(&mut self, reloader: Reloader) {
        self.reloader = Some(reloader);
    }

    /// Install the reap hook run just before each terminal agent is despawned,
    /// so the daemon can tear down that agent's sandbox and drop its tool state.
    /// Without one, reaping just despawns the entity (the prior behavior).
    pub fn set_reaper(&mut self, reaper: Reaper) {
        self.reaper = Some(reaper);
    }

    /// Resolve a run id to a live entity, paging it in from disk if it has been
    /// unloaded (and a reloader is installed). Returns `None` if the run is
    /// neither live nor resumable from disk. Newly-reloaded runs are registered.
    fn resolve_or_reload(&mut self, run_id: &str) -> Option<Entity> {
        if let Some(entity) = self.live_entity(run_id) {
            return Some(entity);
        }
        let entity = (self.reloader.as_mut()?)(&mut self.world, run_id)?;
        self.by_run_id.insert(run_id.to_string(), entity);
        Some(entity)
    }

    /// A clone of the interaction hub, for building per-agent backends.
    pub fn interactions(&self) -> InteractionHub {
        self.interactions.clone()
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

    /// Service one [`SubAgentOp`] from a tool lane, replying on its oneshot.
    fn handle_subagent(&mut self, op: SubAgentOp) {
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
                let status = self
                    .live_entity(&run_id)
                    .and_then(|e| self.world.agent_status(e));
                let _ = reply.send(status);
            }
            SubAgentOp::Send {
                run_id,
                content,
                reply,
            } => {
                // Page the target in if it was unloaded, so delivery finds it.
                self.resolve_or_reload(&run_id);
                let ok = self
                    .world
                    .send_message(AgentMessage {
                        agent_id: run_id,
                        content,
                        target_region: None,
                        priority: 0,
                    })
                    .is_ok();
                let _ = reply.send(ok);
            }
            SubAgentOp::Kill { run_id, reply } => {
                let _ = reply.send(self.cancel_tree(&run_id));
            }
        }
    }

    /// Spawn a child agent under `parent_run_id`, linking `ParentRef` /
    /// `SubAgentChildren` and registering its run id. `Err` if the parent is not
    /// live, the depth limit is reached, or the spawner rejects it.
    fn spawn_child(
        &mut self,
        mut args: SpawnArgs,
        parent_run_id: &str,
        max_depth: usize,
    ) -> Result<String, String> {
        // Record the parentage so the child's run metadata nests it in the tree.
        args.parent_run_id = Some(parent_run_id.to_string());
        let parent = self
            .live_entity(parent_run_id)
            .ok_or_else(|| format!("parent run '{parent_run_id}' is not live"))?;
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
        crate::context_transform::apply_context_transforms(world, parent, child);
        self.by_run_id.insert(run_id.clone(), child);
        Ok(run_id)
    }

    /// Cancel a run and every descendant. Returns whether the run existed (paging
    /// it in from disk first if it had been unloaded).
    fn cancel_tree(&mut self, run_id: &str) -> bool {
        let Some(root) = self.resolve_or_reload(run_id) else {
            return false;
        };
        // Collect the subtree (parent before children), then cancel each.
        let mut subtree = Vec::new();
        let mut stack = vec![root];
        while let Some(e) = stack.pop() {
            subtree.push(e);
            if let Some(kids) = self.world.world().get::<SubAgentChildren>(e) {
                stack.extend(kids.children.iter().copied());
            }
        }
        let mut cancelled = false;
        for e in subtree {
            cancelled |= self.world.cancel(e);
        }
        cancelled
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
            ControlOp::Spawn { args, reply } => {
                let result = match self.spawner.as_mut() {
                    // Spawning runs outside the pipeline schedule, so it isn't
                    // covered by `run_isolated`'s panic guard: a panic while
                    // parsing a blueprint or building a sandbox would otherwise
                    // unwind the whole serve task and take the daemon with it.
                    // As with `run_isolated`, the world may be left holding a
                    // partially-built entity — the run just never registers.
                    Some(spawner) => {
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            spawner(&mut self.world, &args)
                        })) {
                            Ok(Ok(entity)) => {
                                self.by_run_id.insert(args.run_id.clone(), entity);
                                Ok(args.run_id.clone())
                            }
                            Ok(Err(e)) => Err(e),
                            Err(_) => Err("agent spawn panicked".to_string()),
                        }
                    }
                    None => Err("this daemon cannot spawn agents".to_string()),
                };
                // A failed spawn used to be invisible daemon-side: the error went
                // back over the socket to a client that has already exited, and
                // nothing was written to disk (issue #107).
                if let Err(error) = &result {
                    tracing::error!(
                        run_id = %args.run_id,
                        blueprint = %args.blueprint_path,
                        workdir = %args.workdir,
                        error = %error,
                        "agent spawn failed"
                    );
                }
                let _ = reply.send(result);
            }
            ControlOp::Status { run_id, reply } => {
                let status = self
                    .live_entity(&run_id)
                    .and_then(|e| self.world.agent_status(e));
                let _ = reply.send(status);
            }
            ControlOp::Pause { run_id, reply } => {
                let ok = self
                    .resolve_or_reload(&run_id)
                    .is_some_and(|e| self.world.pause(e));
                let _ = reply.send(ok);
            }
            ControlOp::Resume { run_id, reply } => {
                let ok = self
                    .resolve_or_reload(&run_id)
                    .is_some_and(|e| self.world.resume(e));
                let _ = reply.send(ok);
            }
            ControlOp::Cancel { run_id, reply } => {
                let ok = self
                    .resolve_or_reload(&run_id)
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
                // Page the target in if it was unloaded, so delivery finds it.
                self.resolve_or_reload(&agent_id);
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
            ControlOp::ListInteractions { reply } => {
                let _ = reply.send(self.interactions.pending());
            }
            ControlOp::AnswerInteraction { response, reply } => {
                let _ = reply.send(self.interactions.answer(response));
            }
            ControlOp::CancelInteraction { request_id, reply } => {
                let _ = reply.send(self.interactions.cancel(&request_id));
            }
            ControlOp::Shutdown { reply } => {
                // Reply first (best effort), then trigger the world's shutdown so
                // the serve loop's next `select!` returns.
                let _ = reply.send(true);
                self.world.shutdown();
            }
        }
    }

    /// Flush all queued persistence and stop the hosted world, guaranteeing every
    /// dirty agent's final snapshot reaches disk (see
    /// [`PipelineWorld::flush_and_stop`]). Invoked automatically when [`Self::serve`]
    /// returns; also exposed directly for callers that drive the world themselves.
    pub async fn flush_and_stop(&mut self) {
        self.world.flush_and_stop().await;
    }

    /// Run the host: drive the world to quiescence, then park until an async
    /// result wakes it, a control op arrives, or shutdown is signalled. Returns
    /// when shutdown fires or the control channel closes — and before returning,
    /// **flushes all queued persistence to disk** ([`Self::flush_and_stop`]) so a
    /// clean daemon shutdown never loses a dirty agent's final snapshot.
    pub async fn serve(&mut self, mut control_rx: UnboundedReceiver<ControlOp>) {
        let wake = self.world.wake_handle();
        let shutdown = self.world.shutdown_handle();
        'serve: loop {
            self.world.run_to_fixed_point();
            self.emit_events();
            tokio::select! {
                _ = wake.notified() => {}
                _ = shutdown.notified() => break 'serve,
                op = control_rx.recv() => {
                    match op {
                        // Await the spawn preprocessor (e.g. lazy MCP connect) before
                        // the sync spawner runs, so the pool is warm. The returned
                        // future is `'static`, so no borrow of `self`/`op` outlives it.
                        Some(op) => {
                            let pre = match &op {
                                ControlOp::Spawn { args, .. } => {
                                    self.spawn_preprocessor.as_ref().map(|pp| pp(args))
                                }
                                _ => None,
                            };
                            if let Some(fut) = pre {
                                fut.await;
                            }
                            self.handle(op);
                        }
                        None => break 'serve, // all control senders dropped
                    }
                }
                // The host holds a `subagent_tx`, so this only yields `Some`.
                Some(sub) = self.subagent_rx.recv() => {
                    // Warm a spawning sub-agent's MCP servers first, same as a
                    // top-level Spawn (both run in this async loop).
                    let pre = match &sub {
                        SubAgentOp::Spawn { args, .. } => {
                            self.spawn_preprocessor.as_ref().map(|pp| pp(args))
                        }
                        _ => None,
                    };
                    if let Some(fut) = pre {
                        fut.await;
                    }
                    self.handle_subagent(sub);
                }
            }
        }
        // Shutting down: drain the persistence lane before the world is dropped.
        self.flush_and_stop().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dynamic_interaction::InteractionBackend;
    use crate::inference_pool::InferencePoolConfig;
    use crate::pipeline::{
        AgentBlueprint, ReadyToInfer, StageCursor, StageInference, StageInferences, StageProgress,
        StageSetup, StageSetups, ToolService, VisitCounts, WaitingForChildren,
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
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
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
        let mut registry = crate::providers::ProviderRegistry::new();
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
            1,
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
                extra_params: Default::default(),
                batch_tool_hint: false,
                request_timeout_secs: None,
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
    async fn spawn_op_uses_installed_spawner_and_registers() {
        let mut host = host_with(vec![]);
        host.set_spawner(Box::new(|world, args| {
            Ok(world.spawn_agent((agent_state(&args.run_id),)))
        }));

        let result = ask(&mut host, |reply| ControlOp::Spawn {
            args: Box::new(SpawnArgs {
                run_id: "r1".to_string(),
                ..Default::default()
            }),
            reply,
        })
        .await;
        assert_eq!(result, Ok("r1".to_string()));

        // The run is now registered, so Status resolves it.
        let status = ask(&mut host, |reply| ControlOp::Status {
            run_id: "r1".to_string(),
            reply,
        })
        .await;
        assert_eq!(status, Some(AgentStatus::Active));
    }

    #[tokio::test]
    async fn spawn_op_propagates_spawner_error() {
        let mut host = host_with(vec![]);
        host.set_spawner(Box::new(|_world, _args| Err("bad blueprint".to_string())));
        let result = ask(&mut host, |reply| ControlOp::Spawn {
            args: Box::new(SpawnArgs::default()),
            reply,
        })
        .await;
        assert_eq!(result, Err("bad blueprint".to_string()));
    }

    #[tokio::test]
    async fn spawn_op_contains_a_panicking_spawner() {
        // A panic while building an agent (bad manifest, sandbox blow-up) must
        // not unwind the daemon's serve task — the run just fails to start.
        let mut host = host_with(vec![]);
        host.set_spawner(Box::new(|_world, _args| panic!("simulated spawn panic")));
        let (tx, rx) = oneshot::channel();
        crate::test_support::with_silenced_panics(|| {
            host.handle(ControlOp::Spawn {
                args: Box::new(SpawnArgs::default()),
                reply: tx,
            });
        });
        assert_eq!(rx.await.unwrap(), Err("agent spawn panicked".to_string()));
        // The host is still usable afterwards, and the run never registered.
        let status = ask(&mut host, |reply| ControlOp::Status {
            run_id: SpawnArgs::default().run_id,
            reply,
        })
        .await;
        assert!(status.is_none());
    }

    #[tokio::test]
    async fn spawn_op_errors_without_a_spawner() {
        let mut host = host_with(vec![]);
        let result = ask(&mut host, |reply| ControlOp::Spawn {
            args: Box::new(SpawnArgs::default()),
            reply,
        })
        .await;
        assert!(result.unwrap_err().contains("cannot spawn"));
    }

    // ─── sub-agent bridge ──────────────────────────────────────────────────

    async fn ask_sub<T>(
        host: &mut WorldHost,
        make: impl FnOnce(oneshot::Sender<T>) -> SubAgentOp,
    ) -> T {
        let (tx, rx) = oneshot::channel();
        host.handle_subagent(make(tx));
        rx.await.unwrap()
    }

    /// A spawner that adds a bare child agent and returns it.
    fn child_spawner() -> Spawner {
        Box::new(|world, args| Ok(world.spawn_agent((agent_state(&args.run_id),))))
    }

    #[tokio::test]
    async fn subagent_spawn_links_child_and_registers() {
        let mut host = host_with(vec![]);
        host.set_spawner(child_spawner());
        let parent = spawn(&mut host, "parent", "parent");

        let result = ask_sub(&mut host, |reply| SubAgentOp::Spawn {
            args: Box::new(SpawnArgs {
                run_id: "child".to_string(),
                ..Default::default()
            }),
            parent_run_id: "parent".to_string(),
            max_depth: 3,
            reply,
        })
        .await;
        assert_eq!(result, Ok("child".to_string()));

        let child = host.by_run_id["child"];
        // The child links back to the parent at depth 1.
        let pref = host.world.world().get::<ParentRef>(child).unwrap();
        assert_eq!(pref.parent_entity, parent);
        assert_eq!(pref.depth, 1);
        // The parent tracks the child.
        let kids = host.world.world().get::<SubAgentChildren>(parent).unwrap();
        assert_eq!(kids.children, vec![child]);
    }

    #[tokio::test]
    async fn subagent_spawn_appends_to_existing_children() {
        let mut host = host_with(vec![]);
        host.set_spawner(child_spawner());
        spawn(&mut host, "parent", "parent");
        for id in ["c1", "c2"] {
            let r = ask_sub(&mut host, |reply| SubAgentOp::Spawn {
                args: Box::new(SpawnArgs {
                    run_id: id.to_string(),
                    ..Default::default()
                }),
                parent_run_id: "parent".to_string(),
                max_depth: 3,
                reply,
            })
            .await;
            assert!(r.is_ok());
        }
        let parent = host.by_run_id["parent"];
        let kids = host.world.world().get::<SubAgentChildren>(parent).unwrap();
        assert_eq!(kids.children.len(), 2);
    }

    #[tokio::test]
    async fn subagent_spawn_rejects_beyond_max_depth() {
        let mut host = host_with(vec![]);
        host.set_spawner(child_spawner());
        spawn(&mut host, "parent", "parent");
        let result = ask_sub(&mut host, |reply| SubAgentOp::Spawn {
            args: Box::new(SpawnArgs {
                run_id: "child".to_string(),
                ..Default::default()
            }),
            parent_run_id: "parent".to_string(),
            max_depth: 0, // child would be depth 1 > 0
            reply,
        })
        .await;
        assert!(result.unwrap_err().contains("depth limit"));
        assert!(!host.by_run_id.contains_key("child"));
    }

    #[tokio::test]
    async fn subagent_spawn_unknown_parent_and_no_spawner_and_spawner_error() {
        // Unknown parent.
        let mut host = host_with(vec![]);
        host.set_spawner(child_spawner());
        let r = ask_sub(&mut host, |reply| SubAgentOp::Spawn {
            args: Box::new(SpawnArgs::default()),
            parent_run_id: "ghost".to_string(),
            max_depth: 3,
            reply,
        })
        .await;
        assert!(r.unwrap_err().contains("not live"));

        // No spawner installed.
        let mut host2 = host_with(vec![]);
        spawn(&mut host2, "parent", "parent");
        let r = ask_sub(&mut host2, |reply| SubAgentOp::Spawn {
            args: Box::new(SpawnArgs::default()),
            parent_run_id: "parent".to_string(),
            max_depth: 3,
            reply,
        })
        .await;
        assert!(r.unwrap_err().contains("cannot spawn"));

        // Spawner rejects.
        let mut host3 = host_with(vec![]);
        host3.set_spawner(Box::new(|_w, _a| Err("bad blueprint".to_string())));
        spawn(&mut host3, "parent", "parent");
        let r = ask_sub(&mut host3, |reply| SubAgentOp::Spawn {
            args: Box::new(SpawnArgs::default()),
            parent_run_id: "parent".to_string(),
            max_depth: 3,
            reply,
        })
        .await;
        assert_eq!(r, Err("bad blueprint".to_string()));
    }

    #[tokio::test]
    async fn subagent_check_reports_status_or_none() {
        let mut host = host_with(vec![]);
        spawn(&mut host, "run-a", "run-a");
        let status = ask_sub(&mut host, |reply| SubAgentOp::Check {
            run_id: "run-a".to_string(),
            reply,
        })
        .await;
        assert_eq!(status, Some(AgentStatus::Active));

        let none = ask_sub(&mut host, |reply| SubAgentOp::Check {
            run_id: "ghost".to_string(),
            reply,
        })
        .await;
        assert_eq!(none, None);
    }

    #[tokio::test]
    async fn subagent_send_delivers_to_inbox() {
        let mut host = host_with(vec![]);
        spawn(&mut host, "run-a", "run-a");
        let ok = ask_sub(&mut host, |reply| SubAgentOp::Send {
            run_id: "run-a".to_string(),
            content: "hello child".to_string(),
            reply,
        })
        .await;
        assert!(ok);
    }

    #[tokio::test]
    async fn subagent_kill_cancels_the_whole_tree() {
        let mut host = host_with(vec![]);
        host.set_spawner(child_spawner());
        spawn(&mut host, "parent", "parent");
        ask_sub(&mut host, |reply| SubAgentOp::Spawn {
            args: Box::new(SpawnArgs {
                run_id: "child".to_string(),
                ..Default::default()
            }),
            parent_run_id: "parent".to_string(),
            max_depth: 3,
            reply,
        })
        .await
        .unwrap();

        let ok = ask_sub(&mut host, |reply| SubAgentOp::Kill {
            run_id: "parent".to_string(),
            reply,
        })
        .await;
        assert!(ok);
        assert_eq!(
            host.world.agent_status(host.by_run_id["parent"]),
            Some(AgentStatus::Cancelled)
        );
        assert_eq!(
            host.world.agent_status(host.by_run_id["child"]),
            Some(AgentStatus::Cancelled)
        );

        // Killing an unknown run is a no-op.
        let miss = ask_sub(&mut host, |reply| SubAgentOp::Kill {
            run_id: "ghost".to_string(),
            reply,
        })
        .await;
        assert!(!miss);
    }

    #[tokio::test]
    async fn interaction_ops_list_answer_and_cancel() {
        let mut host = host_with(vec![]);
        let hub = host.interactions();
        let backend = hub.backend_for("agent-a");

        // An agent's ask is registered on the hub.
        let asking = tokio::spawn(async move {
            backend
                .ask(leviath_core::interaction::InteractionRequest::free_text(
                    "q1", "prompt?", "stage", true,
                ))
                .await
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        // ListInteractions surfaces it.
        let list = ask(&mut host, |reply| ControlOp::ListInteractions { reply }).await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "agent-a");

        // AnswerInteraction fulfils it.
        let ok = ask(&mut host, |reply| ControlOp::AnswerInteraction {
            response: leviath_core::interaction::InteractionResponse::text("q1", "hi"),
            reply,
        })
        .await;
        assert!(ok);
        assert_eq!(asking.await.unwrap().value.as_deref(), Some("hi"));

        // CancelInteraction on an unknown id ⇒ false.
        let cancelled = ask(&mut host, |reply| ControlOp::CancelInteraction {
            request_id: "gone".to_string(),
            reply,
        })
        .await;
        assert!(!cancelled);
    }

    #[tokio::test]
    async fn cancel_interaction_op_wakes_asker() {
        let mut host = host_with(vec![]);
        let backend = host.interactions().backend_for("agent-a");
        let asking = tokio::spawn(async move {
            backend
                .ask(leviath_core::interaction::InteractionRequest::free_text(
                    "q2", "p", "s", true,
                ))
                .await
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        let ok = ask(&mut host, |reply| ControlOp::CancelInteraction {
            request_id: "q2".to_string(),
            reply,
        })
        .await;
        assert!(ok);
        assert_eq!(asking.await.unwrap().request_id, "q2");
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
    async fn serve_awaits_spawn_preprocessor_before_spawning() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut host = host_with(vec![]);
        let ran = Arc::new(AtomicBool::new(false));
        let ran_pp = ran.clone();
        host.set_spawn_preprocessor(Box::new(move |_args| {
            let ran = ran_pp.clone();
            Box::pin(async move {
                ran.store(true, Ordering::SeqCst);
            })
        }));
        let ran_spawn = ran.clone();
        host.set_spawner(Box::new(move |world, args| {
            // The preprocessor must have completed before the spawner runs.
            assert!(ran_spawn.load(Ordering::SeqCst));
            Ok(world.spawn_agent((agent_state(&args.run_id),)))
        }));
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            host.serve(op_rx).await;
        });
        let (tx, rx) = oneshot::channel();
        op_tx
            .send(ControlOp::Spawn {
                args: Box::new(SpawnArgs {
                    run_id: "rp".to_string(),
                    ..Default::default()
                }),
                reply: tx,
            })
            .unwrap();
        let result = rx.await.unwrap();
        drop(op_tx); // close the channel so serve() returns
        handle.await.unwrap();
        assert_eq!(result, Ok("rp".to_string()));
        assert!(ran.load(Ordering::SeqCst), "preprocessor ran");
    }

    #[tokio::test]
    async fn serve_awaits_preprocessor_for_subagent_spawn() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut host = host_with(vec![]);
        host.set_spawner(child_spawner());
        let _parent = spawn(&mut host, "parent", "parent");
        // Count preprocessor invocations: it must fire for the sub-agent Spawn,
        // and NOT for the non-Spawn Check op (the `_ => None` arm).
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_pp = calls.clone();
        host.set_spawn_preprocessor(Box::new(move |_args| {
            let calls = calls_pp.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
            })
        }));
        let sub_tx = host.subagent_sender();
        let shutdown = host.world_mut().shutdown_handle();
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            host.serve(op_rx).await;
        });

        // A non-Spawn sub-agent op does not invoke the preprocessor.
        let (ctx, crx) = oneshot::channel();
        sub_tx
            .send(SubAgentOp::Check {
                run_id: "parent".to_string(),
                reply: ctx,
            })
            .unwrap();
        let _ = crx.await.unwrap();

        // A sub-agent Spawn does.
        let (stx, srx) = oneshot::channel();
        sub_tx
            .send(SubAgentOp::Spawn {
                args: Box::new(SpawnArgs {
                    run_id: "child".to_string(),
                    ..Default::default()
                }),
                parent_run_id: "parent".to_string(),
                max_depth: 3,
                reply: stx,
            })
            .unwrap();
        assert_eq!(srx.await.unwrap(), Ok("child".to_string()));

        shutdown.notify_one();
        drop(op_tx);
        handle.await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "only the Spawn preprocessed"
        );
    }

    #[tokio::test]
    async fn serve_spawns_without_a_preprocessor() {
        // A Spawn op through serve() with no preprocessor installed exercises the
        // `None` arm of the preprocessor branch.
        let mut host = host_with(vec![]);
        host.set_spawner(Box::new(|world, args| {
            Ok(world.spawn_agent((agent_state(&args.run_id),)))
        }));
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            host.serve(op_rx).await;
        });
        let (tx, rx) = oneshot::channel();
        op_tx
            .send(ControlOp::Spawn {
                args: Box::new(SpawnArgs {
                    run_id: "np".to_string(),
                    ..Default::default()
                }),
                reply: tx,
            })
            .unwrap();
        let result = rx.await.unwrap();
        drop(op_tx);
        handle.await.unwrap();
        assert_eq!(result, Ok("np".to_string()));
    }

    #[tokio::test]
    async fn shutdown_op_stops_the_serve_loop() {
        let mut host = host_with(vec![]);
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(async move { host.serve(op_rx).await });

        let (tx, rx) = oneshot::channel();
        op_tx.send(ControlOp::Shutdown { reply: tx }).unwrap();
        assert!(rx.await.unwrap());
        // The serve loop returns once the world's shutdown is signalled.
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn flush_and_stop_delegates_to_the_world() {
        // The host's flush-and-stop drains the world's persistence lane; calling it
        // (even with no agents) returns cleanly and is idempotent.
        let mut host = host_with(vec![]);
        host.flush_and_stop().await;
        host.flush_and_stop().await; // second call is a no-op
    }

    #[tokio::test]
    async fn serve_loop_services_subagent_ops_via_the_sender() {
        let mut host = host_with(vec![]);
        spawn(&mut host, "run-a", "run-a");
        let sub_tx = host.subagent_sender();
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(async move { host.serve(op_rx).await });

        // A Check submitted on the sub-agent channel is serviced by the serve loop.
        let (tx, rx) = oneshot::channel();
        sub_tx
            .send(SubAgentOp::Check {
                run_id: "run-a".to_string(),
                reply: tx,
            })
            .unwrap();
        assert!(rx.await.unwrap().is_some());

        let (stx, srx) = oneshot::channel();
        op_tx.send(ControlOp::Shutdown { reply: stx }).unwrap();
        assert!(srx.await.unwrap());
        handle.await.unwrap();
    }

    #[test]
    fn status_str_covers_all_variants() {
        assert_eq!(status_str(&AgentStatus::Idle), "idle");
        assert_eq!(status_str(&AgentStatus::Active), "active");
        assert_eq!(status_str(&AgentStatus::Waiting), "waiting");
        assert_eq!(status_str(&AgentStatus::Complete), "complete");
        assert_eq!(
            status_str(&AgentStatus::Error {
                message: "x".to_string()
            }),
            "error"
        );
        assert_eq!(status_str(&AgentStatus::Cancelled), "cancelled");
    }

    #[tokio::test]
    async fn emit_events_broadcasts_agent_changes() {
        let mut host = host_with(vec![text("done")]);
        let mut rx = host.subscribe();
        let entity = spawn(&mut host, "run-a", "agent-a");
        // Attach run metadata so the `Spawned` event carries the blueprint name.
        host.world_mut()
            .world_mut()
            .entity_mut(entity)
            .insert(RunMetadata {
                run_id: "run-a".to_string(),
                agent_name: "coder".to_string(),
                agent_path: "/a".to_string(),
                task: "t".to_string(),
                model: None,
                workdir: "/w".to_string(),
                num_stages: 1,
                started_at: 0,
                parent_run_id: None,
                metadata: std::collections::HashMap::new(),
                callback_url: None,
                callback_secret: None,
                title: None,
            });

        // First emission after spawn: Spawned + Status + Tokens + Context.
        host.emit_events();
        let first: Vec<WorldEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            first
                .iter()
                .any(|e| matches!(e, WorldEvent::Spawned { .. }))
        );
        assert!(first.iter().any(|e| matches!(e, WorldEvent::Status { .. })));
        assert!(first.iter().any(|e| matches!(e, WorldEvent::Tokens { .. })));
        assert!(
            first
                .iter()
                .any(|e| matches!(e, WorldEvent::Context { .. }))
        );

        // A second emission with nothing changed emits nothing (skip branches).
        host.emit_events();
        assert!(rx.try_recv().is_err());

        // Drive to completion, then emit: a terminal `Completed` fires.
        host.world_mut().run_until_idle(20).await;
        host.emit_events();
        let done: Vec<WorldEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            done.iter()
                .any(|e| matches!(e, WorldEvent::Completed { .. }))
        );

        // Once terminal and unchanged, a further emission fires nothing.
        host.emit_events();
        assert!(
            std::iter::from_fn(|| rx.try_recv().ok())
                .collect::<Vec<_>>()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn emit_events_unloads_terminal_agents_when_safe() {
        let mut host = host_with(vec![]);

        // A terminal root: emitted on the first pass, unloaded on the second.
        let root = {
            let mut s = agent_state("root");
            s.status = AgentStatus::Complete;
            host.world.world_mut().spawn(s).id()
        };
        host.register("root", root);
        host.emit_events();
        assert!(
            host.live_entity("root").is_some(),
            "not reaped on the first terminal pass (event must go out first)"
        );
        host.emit_events();
        assert!(host.live_entity("root").is_none(), "reaped after emit");
        assert!(
            host.world.world().get::<AgentState>(root).is_none(),
            "entity despawned"
        );

        // A terminal child under a LIVE (Active) parent is deferred.
        let parent = host.world.world_mut().spawn(agent_state("parent")).id();
        host.register("parent", parent);
        let child = {
            let mut s = agent_state("child");
            s.status = AgentStatus::Complete;
            host.world
                .world_mut()
                .spawn((
                    s,
                    ParentRef {
                        parent_entity: parent,
                        parent_agent_id: "parent".to_string(),
                        depth: 1,
                    },
                ))
                .id()
        };
        host.register("child", child);
        host.emit_events();
        host.emit_events();
        assert!(
            host.live_entity("child").is_some(),
            "not reaped while its parent is live"
        );

        // Once the parent is terminal, the child becomes reapable.
        host.world
            .world_mut()
            .get_mut::<AgentState>(parent)
            .unwrap()
            .status = AgentStatus::Complete;
        host.emit_events();
        host.emit_events();
        assert!(
            host.live_entity("child").is_none(),
            "reaped once its parent is terminal"
        );

        // A terminal child whose parent entity was despawned is also reapable.
        let ghost = host.world.world_mut().spawn_empty().id();
        host.world.world_mut().despawn(ghost);
        let orphan = {
            let mut s = agent_state("orphan");
            s.status = AgentStatus::Complete;
            host.world
                .world_mut()
                .spawn((
                    s,
                    ParentRef {
                        parent_entity: ghost,
                        parent_agent_id: "gone".to_string(),
                        depth: 1,
                    },
                ))
                .id()
        };
        host.register("orphan", orphan);
        host.emit_events();
        host.emit_events();
        assert!(
            host.live_entity("orphan").is_none(),
            "reaped: parent entity despawned"
        );
    }

    #[tokio::test]
    async fn emit_events_does_not_reap_non_terminal_agents() {
        let mut host = host_with(vec![]);
        let active = host.world.world_mut().spawn(agent_state("active")).id();
        host.register("active", active);
        host.emit_events();
        host.emit_events();
        assert!(host.live_entity("active").is_some());
    }

    #[tokio::test]
    async fn reaper_runs_once_per_agent_before_despawn() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut host = host_with(vec![]);

        // The reap hook records that it saw a still-live entity, proving it runs
        // before despawn. A `static` counter dodges the `'static` closure bound.
        static SEEN_LIVE: AtomicUsize = AtomicUsize::new(0);
        SEEN_LIVE.store(0, Ordering::SeqCst);
        host.set_reaper(Box::new(|world, entity| {
            // Branch-free (`live as usize`) so the whole closure body is covered
            // by a single firing; the assertion below confirms `live` was true.
            let live = world.world().get::<AgentState>(entity).is_some();
            SEEN_LIVE.fetch_add(live as usize, Ordering::SeqCst);
        }));

        let root = {
            let mut s = agent_state("root");
            s.status = AgentStatus::Complete;
            host.world.world_mut().spawn(s).id()
        };
        host.register("root", root);
        host.emit_events(); // first pass: emit terminal event, not yet reaped
        assert_eq!(SEEN_LIVE.load(Ordering::SeqCst), 0);
        host.emit_events(); // second pass: reaper fires, then despawn
        assert!(host.live_entity("root").is_none(), "reaped after emit");
        assert_eq!(
            SEEN_LIVE.load(Ordering::SeqCst),
            1,
            "reaper ran exactly once, while the entity was still live"
        );
    }

    /// Spawn a `Waiting` agent (optionally with an extra marker component) and
    /// register it under `run_id`.
    fn register_waiting(host: &mut WorldHost, run_id: &str) -> Entity {
        let mut s = agent_state(run_id);
        s.status = AgentStatus::Waiting;
        let e = host.world.world_mut().spawn(s).id();
        host.register(run_id, e);
        e
    }

    /// Regression: a `Waiting` agent must NEVER be unloaded. Every `Waiting`
    /// state carries a live, unpersisted continuation, so flushing it to disk
    /// strands the run. The worst case is an agent parked on a human approval
    /// (`AwaitingInteraction`): unloading it means the answer has no entity to
    /// wake and the run hangs in "waiting" forever.
    #[tokio::test]
    async fn emit_events_never_unloads_waiting_agents() {
        use crate::components::AwaitingInteraction;

        let mut host = host_with(vec![]);

        // Parked on a human prompt (`AwaitingInteraction`) — the reported bug:
        // the blocked `ask` future is unpersisted, so unloading strands the run.
        let asking = register_waiting(&mut host, "asking");
        host.world
            .world_mut()
            .entity_mut(asking)
            .insert(AwaitingInteraction);
        // Gated on children, and a plain parked agent.
        let gated = register_waiting(&mut host, "gated");
        host.world
            .world_mut()
            .entity_mut(gated)
            .insert(WaitingForChildren);
        register_waiting(&mut host, "parked");

        // Many serve passes — none of them may reap a Waiting agent.
        for _ in 0..5 {
            host.emit_events();
        }
        for run_id in ["asking", "gated", "parked"] {
            assert!(
                host.live_entity(run_id).is_some(),
                "a Waiting agent was unloaded and can no longer be resumed"
            );
        }
    }

    #[tokio::test]
    async fn resolve_or_reload_pages_in_and_registers() {
        let mut host = host_with(vec![]);
        // No reloader installed → a miss stays a miss.
        assert!(host.resolve_or_reload("ghost").is_none());

        // A reloader that declines (run not resumable from disk) → still a miss,
        // and nothing gets registered.
        host.set_reloader(Box::new(|_world, _run_id| None));
        assert!(host.resolve_or_reload("gone").is_none());
        assert!(
            host.live_entity("gone").is_none(),
            "a declined reload registers nothing"
        );

        // With a reloader that resolves → an unloaded run is paged in and registered.
        host.set_reloader(Box::new(|world, run_id| {
            Some(world.spawn_agent((agent_state(run_id),)))
        }));
        let paged = host.resolve_or_reload("paged").expect("reloaded");
        assert_eq!(
            host.live_entity("paged"),
            Some(paged),
            "registered after reload"
        );

        // A live run is returned without invoking the reloader (no re-spawn).
        assert_eq!(host.resolve_or_reload("paged"), Some(paged));
    }

    #[tokio::test]
    async fn cancel_pages_in_an_unloaded_run() {
        let mut host = host_with(vec![]);
        host.set_reloader(Box::new(|world, run_id| {
            Some(world.spawn_agent((agent_state(run_id),)))
        }));
        // Cancelling a run that isn't in memory pages it in, then cancels it.
        let cancelled = ask(&mut host, |reply| ControlOp::Cancel {
            run_id: "unloaded".to_string(),
            reply,
        })
        .await;
        assert!(cancelled, "reloaded then cancelled");
        assert_eq!(
            host.world
                .agent_status(host.live_entity("unloaded").unwrap()),
            Some(AgentStatus::Cancelled)
        );
    }

    #[tokio::test]
    async fn emit_events_broadcasts_new_interactions_once() {
        let mut host = host_with(vec![]);
        let mut rx = host.subscribe();
        let backend = host.interactions().backend_for("agent-a");
        let asking = tokio::spawn(async move {
            backend
                .ask(leviath_core::interaction::InteractionRequest::free_text(
                    "q1", "p", "s", true,
                ))
                .await
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        host.emit_events();
        let evs: Vec<WorldEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            evs.iter()
                .any(|e| matches!(e, WorldEvent::Interaction { .. }))
        );
        // A second emission does not re-broadcast the same interaction.
        host.emit_events();
        assert!(rx.try_recv().is_err());

        // Answer it so the asking task finishes cleanly.
        assert!(
            host.interactions()
                .answer(leviath_core::interaction::InteractionResponse::text(
                    "q1", "ok"
                ))
        );
        let _ = asking.await;
    }

    #[tokio::test]
    async fn event_sender_feeds_subscribers() {
        let host = host_with(vec![]);
        let mut rx = host.subscribe();
        let event = WorldEvent::Completed {
            run_id: "r".to_string(),
            agent_id: "a".to_string(),
            status: "complete".to_string(),
        };
        host.event_sender().send(event.clone()).unwrap();
        assert_eq!(rx.try_recv().unwrap(), event);
    }

    #[tokio::test]
    async fn emit_events_skips_despawned_agents() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "agent-a");
        host.world_mut().world_mut().despawn(e);
        // The stale run-id mapping is skipped; must not panic.
        host.emit_events();
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
        assert_eq!(p.count_tokens("t", "m").await, 1);
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
            request_timeout_secs: None,
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
