//! The world host: the daemon-side wrapper that owns a single [`PipelineWorld`],
//! maps stable **run ids** to ECS entities, and interleaves external **control
//! operations** with driving the world - all on one task, so there is never any
//! locking around the world.
//!
//! Clients (a control socket, the TUI, the CLI) don't hold entities - those are
//! generational indices meaningful only inside the world. They address agents by
//! run id. The host keeps the `run_id → Entity` map and turns each
//! [`ControlOp`] into the corresponding [`PipelineWorld`] call, replying on the
//! op's oneshot channel.
//!
//! The serve loop drives the world to quiescence, then parks until either an
//! async result wakes it, a control op arrives, or shutdown is signalled -
//! handling a control op and then re-driving to quiescence so its effect (a
//! resume, a delivered message) is applied immediately.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use bevy_ecs::entity::Entity;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{broadcast, oneshot};

use crate::components::{
    AgentMessage, AgentState, AgentStatus, AwaitingInteraction, ContextWindow, ParentRef,
    SubAgentChildren, WaitReason,
};
use crate::interaction_hub::InteractionHub;
use crate::persistence::{RunMetadata, TokenTotals};
use crate::world::{LaneSnapshot, PipelineWorld};
use leviath_core::interaction::{InteractionRequest, InteractionResponse};
use serde::{Deserialize, Serialize};

/// The parameters for spawning an agent into the world. The runtime doesn't know
/// how to load blueprints or resolve tools - that policy lives in the
/// [`Spawner`] the daemon installs - so this just carries the raw request.
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

/// One row of a run listing ([`ControlRequest::List`]): a live run, its status,
/// and enough context to judge whether that status is a problem.
///
/// [`ControlRequest::List`]: crate::control_socket::ControlRequest::List
///
/// `lev ps` used to be a run id and a status word, which is why issue #184
/// happened: `waiting` on its own says nothing about whether a person is needed,
/// and there was no way to tell a run that had moved a second ago from one that
/// had been stopped for an hour. Everything here is read straight off the live
/// world, so it is the daemon's own view, not a re-read of `meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunListEntry {
    /// The run id (`lev ps`'s first column, and what `lev kill` takes).
    pub run_id: String,
    /// The agent's live status.
    pub status: AgentStatus,
    /// Why the status is [`AgentStatus::Waiting`]; `None` for every other
    /// status, and for a `Waiting` the host cannot attribute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_reason: Option<WaitReason>,
    /// The stage the agent is in.
    pub stage: String,
    /// Zero-based index of that stage, when the agent tracks one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_index: Option<usize>,
    /// How many stages the blueprint has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_stages: Option<usize>,
    /// Iterations completed in the current stage.
    pub iteration: usize,
    /// Cumulative tool calls across the run.
    pub tool_calls: usize,
    /// Unix seconds when this run last actually moved (see
    /// [`PersistWatermark`](crate::pipeline::PersistWatermark)). Distinct from
    /// `meta.json`'s `updated_at`, which also advances on a heartbeat and so
    /// cannot be used to tell a working run from a wedged one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_progress_at: Option<i64>,
    /// Whether this run is unattended (`--yolo`). An unattended run should never
    /// be sitting on a prompt; if it is, something dropped the flag.
    #[serde(default)]
    pub unattended: bool,
}

/// The daemon's own health, alongside the run listing.
///
/// A per-run view answers "what is this run doing"; this answers "is the daemon
/// getting anywhere at all". They are different questions, and issue #191 was
/// only visible in the second: every individual run looked fine, and the factory
/// as a whole had not moved in hours.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DaemonHealth {
    /// Loaded agents by status.
    pub agents: crate::world::AgentCounts,
    /// Inference-pool occupancy, one entry per model actually used.
    pub inference: Vec<crate::inference_pool::PoolOccupancy>,
    /// Tool batches holding lane capacity and running.
    pub tools_busy: usize,
    /// Tool batches waiting for lane capacity.
    pub tools_queued: usize,
    /// Tool batches parked on an unbounded wait, holding no capacity.
    pub tools_parked: usize,
    /// The tool lane's concurrency cap, including any relief granted.
    pub tools_workers: usize,
    /// Consecutive safety re-drives that found a lane at capacity and no run
    /// moving. Zero on a healthy daemon, and reset by any sign of progress.
    pub dead_cycles: u32,
    /// How many extra tool-lane permits the relief valve has handed out.
    pub relief_granted: usize,
    /// How often the daemon re-drives itself, so a client can turn
    /// `dead_cycles` into wall-clock time.
    pub redrive_secs: u64,
}

/// The daemon-installed function that turns [`SpawnArgs`] into a live agent:
/// loads the blueprint, resolves stages/tools, spawns into the world, and
/// returns the new entity (the host records the run-id mapping). Returns `Err`
/// with a human-readable message on failure.
pub type Spawner = Box<dyn FnMut(&mut PipelineWorld, &SpawnArgs) -> Result<Entity, String> + Send>;

/// The daemon-installed function that pages a previously-unloaded run back into
/// the world from its on-disk state: given a run id, it reloads the agent (its
/// blueprint, tool state, context, stage) and returns the new entity, or `None`
/// if there is no such resumable run on disk. Used for reload-on-demand - a
/// control/sub-agent op targeting a run that isn't currently in memory pages it
/// in first via the host's internal resolve-or-reload step. Installed with
/// [`WorldHost::set_reloader`].
pub type Reloader = Box<dyn FnMut(&mut PipelineWorld, &str) -> Option<Entity> + Send>;

/// The daemon-installed last resort for cancelling a run the world cannot hold:
/// given a run id, it forces that run's **on-disk** state to a terminal status
/// and reports whether a run directory existed to act on.
///
/// This is what makes a cancel unconditional. [`Reloader`] declines whenever a
/// run can't be rebuilt - its blueprint was moved or deleted, its metadata is
/// unreadable, it died mid-spawn before any agent existed - and before this seam
/// a cancel in that state replied `false` and wrote nothing, so `meta.json` kept
/// claiming `running`/`starting` forever and the run could never be got rid of.
/// The runtime has no notion of the on-disk layout, so the daemon supplies the
/// writer. Installed with [`WorldHost::set_force_terminator`]; without one, a
/// cancel that misses in the world simply misses (the prior behavior).
pub type ForceTerminator = Box<dyn FnMut(&str) -> bool + Send>;

/// The daemon-installed hook run just before a terminal agent's entity is
/// despawned (reaped). It receives the world and the entity while both are still
/// valid, so the daemon can release per-agent resources the runtime doesn't know
/// about - tearing down the agent's sandbox and dropping its tool state.
/// Installed with [`WorldHost::set_reaper`]; a no-op when none is set.
pub type Reaper = Box<dyn FnMut(&mut PipelineWorld, Entity) + Send>;

/// An async hook the host awaits *before* servicing a top-level `Spawn` control
/// op, so the daemon can do async preparation the sync spawner can't - e.g.
/// lazily connecting the blueprint's MCP servers into the shared pool so
/// they're warm by the time [`Spawner`] reads them. The returned future is
/// `'static` (it must clone anything it needs from the `SpawnArgs`). Installed
/// with [`WorldHost::set_spawn_preprocessor`]; when none is set, spawns proceed
/// straight to the spawner.
pub type SpawnPreprocessor = Box<
    dyn Fn(&SpawnArgs) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send,
>;

/// A world-access request from an agent's tool lane. The sub-agent tools
/// (`spawn_agent`/`check_agent`/`send_to_agent`/`kill_agent`) need the world and
/// the [`Spawner`], which only the host holds - the tool lane runs async, off the
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
        /// The run doing the sending. The target must be it or one of its
        /// descendants - see `WorldHost::is_within_tree`.
        caller_run_id: String,
        /// The message body.
        content: String,
        /// Context region to deliver into (`None` = the "conversation"
        /// default). The `send_to_agent` tool advertised this from the start
        /// but the op had no field to carry it, so it was silently dropped.
        target_region: Option<String>,
        /// Reply: whether the message was accepted.
        reply: oneshot::Sender<bool>,
    },
    /// Cancel a run and its whole sub-tree. Reply is whether any agent was found.
    Kill {
        /// The run to cancel (with its descendants).
        run_id: String,
        /// The run doing the cancelling. The target must be it or one of its
        /// descendants - see `WorldHost::is_within_tree`.
        caller_run_id: String,
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
    /// List every known live run and its status, with the daemon's own health.
    List {
        /// Reply channel.
        reply: oneshot::Sender<(Vec<RunListEntry>, DaemonHealth)>,
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

/// A change in the world, broadcast to subscribers (the HTTP/WS gateway and
/// in-process embedders) so they get pushed updates instead of polling. The
/// coarse per-run variants (`Spawned`/`Status`/`Tokens`/`Context`/`Completed`)
/// are emitted by the host's change-detection pass as it drives the world;
/// `StageTransition`/`ToolCallStarted`/`ToolCallFinished`/`Log` are pushed at
/// the source by pipeline systems through [`WorldEventSink`]. Streamed over the
/// control transport via `ControlRequest::Subscribe`.
///
/// Marked non-exhaustive: new variants are additive, so consumers outside this
/// crate must keep a catch-all arm.
#[non_exhaustive]
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
    /// A run moved from one stage to another. Emitted by the transition systems
    /// at the moment the new stage is entered (the initial stage at spawn is
    /// covered by [`WorldEvent::Spawned`], not by this).
    StageTransition {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The stage being left.
        from: String,
        /// The stage being entered.
        to: String,
        /// How many times the destination stage has been entered, this entry
        /// included.
        iteration: usize,
    },
    /// A tool call was handed to the async tool lane for execution. Inline
    /// calls (context tools, refusals, gate blocks) resolve without touching
    /// the lane and don't produce this event.
    ToolCallStarted {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The provider-assigned tool call id.
        call_id: String,
        /// The tool name.
        tool: String,
    },
    /// A lane-executed tool call returned. Paired with
    /// [`WorldEvent::ToolCallStarted`] by `call_id`.
    ToolCallFinished {
        /// The run id.
        run_id: String,
        /// The agent id.
        agent_id: String,
        /// The provider-assigned tool call id.
        call_id: String,
        /// The tool name.
        tool: String,
        /// Whether the call took effect (`false` for `[error]`/`[blocked]`/
        /// `[unavailable]` results).
        ok: bool,
        /// The result, flattened to one line and truncated.
        summary: String,
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

impl WorldEvent {
    /// The run id this event belongs to. Every variant carries one; this saves
    /// consumers an exhaustive match (which, with the enum non-exhaustive,
    /// they could not write anyway).
    pub fn run_id(&self) -> &str {
        match self {
            WorldEvent::Spawned { run_id, .. }
            | WorldEvent::Status { run_id, .. }
            | WorldEvent::Tokens { run_id, .. }
            | WorldEvent::Context { run_id, .. }
            | WorldEvent::Interaction { run_id, .. }
            | WorldEvent::Completed { run_id, .. }
            | WorldEvent::StageTransition { run_id, .. }
            | WorldEvent::ToolCallStarted { run_id, .. }
            | WorldEvent::ToolCallFinished { run_id, .. }
            | WorldEvent::Log { run_id, .. } => run_id,
        }
    }
}

/// A world resource holding a clone of the host's [`WorldEvent`] broadcast
/// sender, so ECS systems (e.g. the persistence drain) can push events - notably
/// per-agent [`WorldEvent::Log`] lines - into the same stream the control
/// transport serves. Absent in worlds that don't stream (test / `lev run`), where
/// systems that depend on it become no-ops.
// `Resource` moved from `bevy_ecs::system` to `bevy_ecs::resource` in 0.19.
#[derive(bevy_ecs::resource::Resource, Clone)]
pub struct WorldEventSink(pub broadcast::Sender<WorldEvent>);

/// A short, stable status label for [`WorldEvent`]. Part of the daemon's wire
/// contract (the REST WebSocket forwards it verbatim), so it comes from the one
/// table on [`AgentStatus`] rather than a copy that could drift from it.
fn status_str(status: &AgentStatus) -> &'static str {
    status.label()
}

/// The last-emitted snapshot of an agent, for change detection.
#[derive(Clone, Hash)]
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
    force_terminator: Option<ForceTerminator>,
    reaper: Option<Reaper>,
    events: broadcast::Sender<WorldEvent>,
    emitted: HashMap<String, Emitted>,
    emitted_interactions: HashSet<String>,
    /// Sub-agent world-access requests from tool lanes. The host holds a `tx`
    /// clone so the receiver never closes (its `recv` never yields `None`).
    subagent_tx: UnboundedSender<SubAgentOp>,
    subagent_rx: UnboundedReceiver<SubAgentOp>,
    /// How often [`Self::serve`] re-drives the world even though nothing woke
    /// it. See [`Self::set_redrive_interval`].
    redrive: Duration,
    /// Consecutive re-drives that found the lanes full and nothing moved. See
    /// [`Self::observe_redrive`].
    dead_cycles: u32,
    /// The progress fingerprint as of the previous re-drive, or `None` before
    /// the first one.
    last_progress: Option<u64>,
    /// Extra tool-lane permits the relief valve has handed out over this
    /// daemon's life.
    relief_granted: usize,
    /// Dead cycles the daemon tolerates before widening the tool lane. `0`
    /// disables relief. See [`Self::set_dead_cycles_before_relief`].
    dead_cycles_before_relief: u32,
}

/// How often the serve loop re-drives the world on its own.
///
/// The loop is event-driven, so a missed wake anywhere parks it indefinitely -
/// the daemon looks alive while nothing progresses, which is what issue #189
/// reported as hours of frozen agents. This bounds any such wedge to one
/// interval instead of "until something unrelated happens", and gives the lane
/// heartbeat a place to run.
///
/// Deliberately not configurable: it is a correctness backstop, not a tuning
/// knob. A no-op re-drive is one tick over a handful of systems plus an event
/// diff, so at this cadence it costs nothing measurable.
const DEFAULT_REDRIVE_INTERVAL: Duration = Duration::from_secs(30);

/// How many consecutive dead cycles trigger the tool-lane relief valve.
///
/// At the 30-second re-drive that is five minutes of a full lane going nowhere -
/// long enough that ordinary backpressure never reaches it, short enough that a
/// genuinely wedged daemon is not left overnight. Served from
/// `[limits] dead_cycles_before_relief`; `0` disables relief.
pub const DEFAULT_DEAD_CYCLES_BEFORE_RELIEF: u32 = 10;

impl WorldHost {
    /// Wrap a world with a fresh interaction hub.
    pub fn new(world: PipelineWorld) -> Self {
        Self::with_interactions(world, InteractionHub::new())
    }

    /// Wrap a world with a specific interaction hub - the daemon shares one hub
    /// between the tool service's per-agent backends and this host.
    pub fn with_interactions(mut world: PipelineWorld, interactions: InteractionHub) -> Self {
        let (events, _) = broadcast::channel(1024);
        // Let ECS systems (the persistence drain) push events - per-agent log
        // lines - into the same stream the control transport serves.
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
            force_terminator: None,
            reaper: None,
            events,
            emitted: HashMap::new(),
            emitted_interactions: HashSet::new(),
            subagent_tx,
            subagent_rx,
            redrive: DEFAULT_REDRIVE_INTERVAL,
            dead_cycles: 0,
            last_progress: None,
            relief_granted: 0,
            dead_cycles_before_relief: DEFAULT_DEAD_CYCLES_BEFORE_RELIEF,
        }
    }

    /// Take stock once per safety re-drive: has anything moved, and are the lanes
    /// full? Updates the dead-cycle count and reports.
    ///
    /// A *dead cycle* is a whole re-drive interval in which some lane was at
    /// capacity with work queued behind it and no run observably moved. Both
    /// halves matter. Pressure on its own is just a busy daemon. Stillness on its
    /// own is an idle one, or one agent in a long inference with nobody waiting.
    /// Together they are the shape issue #191 reported: work to do, no capacity to
    /// do it with, and no sign of that ever changing.
    fn observe_redrive(&mut self) {
        let snapshot = self.world.lane_snapshot();
        let progress = self.progress_fingerprint();
        let went_nowhere = snapshot.is_under_pressure() && self.last_progress == Some(progress);
        self.last_progress = Some(progress);
        self.dead_cycles = match went_nowhere {
            true => self.dead_cycles.saturating_add(1),
            false => 0,
        };
        self.log_lane_pressure(&snapshot);
        let relief = self.relieve_if_wedged(&snapshot);
        self.observe_lanes(&snapshot, relief);
    }

    /// Widen the tool lane if the daemon has been going nowhere long enough, and
    /// report how much capacity was added.
    ///
    /// Deliberately additive. The tempting reading of "force-reclaim stuck
    /// slots" is to kill whatever is holding them, and that is the wrong move
    /// here: a run parked on an `ask_user` is doing exactly what it should, and
    /// an operator who mistook `waiting` for `stuck` and started killing healthy
    /// runs is the story behind issue #184. Handing out more capacity unwedges a
    /// jammed lane without having to be right about which run deserves to die.
    ///
    /// Only the tool lane is widened. A full inference pool is a deliberate cap
    /// on requests in flight to a provider, and forcing extra ones past it would
    /// trade a wedge for a rate limit.
    ///
    /// Capped at one extra lane's worth over the daemon's life. If that is not
    /// enough, the problem is not capacity and more of it will not help.
    fn relieve_if_wedged(&mut self, snapshot: &LaneSnapshot) -> usize {
        let threshold = self.dead_cycles_before_relief;
        if threshold == 0 || self.dead_cycles < threshold || !snapshot.tools_saturated {
            return 0;
        }
        // The snapshot's width already includes everything granted so far, so
        // back it out to get the lane's configured width - the budget.
        let configured = snapshot.tools_workers.saturating_sub(self.relief_granted);
        let remaining = configured.saturating_sub(self.relief_granted);
        let granted = self
            .world
            .relieve_tool_lane(remaining.min(snapshot.tools_queued));
        self.relief_granted += granted;
        tracing::error!(
            dead_cycles = self.dead_cycles,
            granted,
            relief_granted = self.relief_granted,
            tools_queued = snapshot.tools_queued,
            tools_parked = snapshot.tools_parked,
            "the tool lane has not drained in {} cycles; widening it by {granted}",
            self.dead_cycles
        );
        // Give the widened lane a fresh interval to show whether it helped,
        // rather than granting again on the very next re-drive.
        self.dead_cycles = 0;
        granted
    }

    /// How many dead cycles the daemon tolerates before widening the tool lane.
    /// `0` disables relief; detection and reporting are unaffected. Served from
    /// `[limits] dead_cycles_before_relief`.
    pub fn set_dead_cycles_before_relief(&mut self, cycles: u32) {
        self.dead_cycles_before_relief = cycles;
    }

    /// Hand one daemon-wide health sample to the telemetry sink.
    ///
    /// `relief` is the capacity granted on this sample, which is a per-sample
    /// figure rather than a running total: the sink accumulates it.
    fn observe_lanes(&self, snapshot: &LaneSnapshot, relief: usize) {
        // Every `PipelineWorld::new` installs the sink resource (a no-op one
        // unless a host replaced it), so this is a hard invariant rather than a
        // branch - the same reasoning as `set_exact_token_counting`.
        self.world
            .world()
            .resource::<crate::telemetry::Telemetry>()
            .0
            .observe_lanes(leviath_core::telemetry::LaneHealth {
                agents_active: snapshot.agents.active,
                agents_waiting: snapshot.agents.waiting,
                tools_busy: snapshot.tools_busy,
                tools_queued: snapshot.tools_queued,
                tools_parked: snapshot.tools_parked,
                tools_workers: snapshot.tools_workers,
                dead_cycles: self.dead_cycles,
                relief_granted: relief,
            });
    }

    /// The daemon's own health: lane occupancy plus the dead-cycle count.
    ///
    /// Served alongside every run listing, because "is this run stuck" and "is
    /// the daemon stuck" are answered by different numbers and an operator
    /// looking at one wants the other in the same breath.
    pub fn health(&self) -> DaemonHealth {
        let snapshot = self.world.lane_snapshot();
        DaemonHealth {
            agents: snapshot.agents,
            inference: snapshot.inference,
            tools_busy: snapshot.tools_busy,
            tools_queued: snapshot.tools_queued,
            tools_parked: snapshot.tools_parked,
            tools_workers: snapshot.tools_workers,
            dead_cycles: self.dead_cycles,
            relief_granted: self.relief_granted,
            redrive_secs: self.redrive.as_secs(),
        }
    }

    /// A number that changes exactly when some run observably moves.
    ///
    /// Derived from the per-run snapshots `emit_events` already keeps to decide
    /// what to broadcast, so an unchanged fingerprint means "nothing happened
    /// that anyone watching would have been told about" - not merely "no event
    /// was sent", which would also be true of a daemon nobody is subscribed to.
    ///
    /// Summed rather than fed through one hasher because a `HashMap` has no
    /// iteration order to depend on. Every field it covers is either monotonic or
    /// hashed, so two different worlds colliding takes a deliberate effort.
    fn progress_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut total = self.emitted.len() as u64;
        for entry in &self.emitted {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            entry.hash(&mut hasher);
            total = total.wrapping_add(hasher.finish());
        }
        total
    }

    /// Report what the lanes are holding.
    ///
    /// The daemon otherwise logs nothing per tick, by design - observation goes
    /// through the telemetry sink. But a wedged daemon emits no telemetry either,
    /// precisely because nothing is happening, so "frozen for hours" left no
    /// trace at all (issue #189). This is the one periodic line that can answer
    /// "is anything running, and what is it queued behind?".
    ///
    /// Quiet by default: `warn` once the daemon has been going nowhere, `info`
    /// while a lane is merely at capacity, `debug` otherwise, so an idle daemon
    /// says nothing above `debug`.
    fn log_lane_pressure(&self, snapshot: &LaneSnapshot) {
        let agents = snapshot.agents.to_string();
        let inference = snapshot.inference_summary();
        if self.dead_cycles > 0 {
            tracing::warn!(
                dead_cycles = self.dead_cycles,
                agents = %agents,
                inference = %inference,
                tools_busy = snapshot.tools_busy,
                tools_workers = snapshot.tools_workers,
                tools_queued = snapshot.tools_queued,
                tools_parked = snapshot.tools_parked,
                "no progress while the lanes are full"
            );
        } else if snapshot.is_under_pressure() {
            tracing::info!(
                agents = %agents,
                inference = %inference,
                tools_busy = snapshot.tools_busy,
                tools_workers = snapshot.tools_workers,
                tools_queued = snapshot.tools_queued,
                tools_parked = snapshot.tools_parked,
                "lane heartbeat: at capacity with work queued"
            );
        } else {
            tracing::debug!(
                agents = %agents,
                inference = %inference,
                tools_busy = snapshot.tools_busy,
                tools_workers = snapshot.tools_workers,
                tools_queued = snapshot.tools_queued,
                tools_parked = snapshot.tools_parked,
                "lane heartbeat"
            );
        }
    }

    /// Override how often [`Self::serve`] re-drives the world with no wake.
    ///
    /// Exists so tests don't have to wait out the 30-second default; the daemon
    /// uses it as-is.
    pub fn set_redrive_interval(&mut self, every: Duration) {
        self.redrive = every;
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
        self.adopt_unregistered_runs();
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
            // Every `Waiting` state carries a live, unpersisted continuation - a
            // blocked `ask` future (`AwaitingInteraction`), running fan-out workers
            // (`FanOutWaiting`), or pending children (`WaitingForChildren`) - so
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

    /// Register any agent that exists in the world but is missing from the run-id
    /// map, so the host's view is the world's view.
    ///
    /// Not every agent arrives through a `Spawn` control op: fan-out workers are
    /// built straight into the world by the fan-out spawner, which has no handle
    /// on the host to register them. An unregistered agent is invisible to `list`
    /// (so `lev ps` never showed a worker), never reaped (its sandbox and tool
    /// state leak), and - worst - un-cancellable, because a cancel by its run id
    /// misses the map, falls through to the reloader, and pages a **second** live
    /// entity in from that run's on-disk state while the original keeps running.
    /// Adopting them here is idempotent and keeps a stale mapping from winning:
    /// a registered id whose entity has been despawned is re-pointed.
    fn adopt_unregistered_runs(&mut self) {
        let live: Vec<(String, Entity)> = self
            .world
            .world_mut()
            .query::<(Entity, &RunMetadata)>()
            .iter(self.world.world())
            .map(|(entity, md)| (md.run_id.clone(), entity))
            .collect();
        for (run_id, entity) in live {
            if self.live_entity(&run_id) != Some(entity) {
                self.by_run_id.insert(run_id, entity);
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

    /// Install the [`ForceTerminator`] used to terminate a run on disk when the
    /// world cannot hold it. Without one, a cancel that misses in the world and
    /// can't be reloaded just misses.
    pub fn set_force_terminator(&mut self, force_terminator: ForceTerminator) {
        self.force_terminator = Some(force_terminator);
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
    fn is_within_tree(&mut self, run_id: &str, ancestor: &str) -> bool {
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
        let mut stack = vec![root];
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
            // Read the agent id before cancelling - the entity stays valid until
            // it is reaped, but reading first keeps this independent of that.
            let agent_id = self
                .world
                .world()
                .get::<AgentState>(e)
                .map(|s| s.agent_id.clone());
            cancelled |= self.world.cancel(e);
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

    /// Why `entity` is [`AgentStatus::Waiting`], read off the markers the engine
    /// already maintains. `None` when the agent is not waiting, or when it is
    /// waiting for a reason nothing has claimed.
    ///
    /// Order matters. A taint-gate block and a stage checkpoint each open a hub
    /// request of their own, so both also carry [`AwaitingInteraction`]; asking
    /// the specific markers first is what keeps them from all reporting as a
    /// generic prompt.
    pub fn wait_reason(&self, entity: Entity) -> Option<WaitReason> {
        let world = self.world.world();
        let state = world.get::<AgentState>(entity)?;
        if state.status != AgentStatus::Waiting {
            return None;
        }
        if world
            .get::<crate::gate_prompt::AwaitingGatePrompt>(entity)
            .is_some()
        {
            return Some(WaitReason::TaintGate);
        }
        if world
            .get::<crate::interaction_points::AwaitingInteractionPoint>(entity)
            .is_some()
        {
            return Some(WaitReason::InteractionPoint);
        }
        if let Some(fanout) = world.get::<crate::fanout::FanOutWaiting>(entity) {
            return Some(WaitReason::FanOutWorkers {
                outstanding: fanout.outstanding(),
            });
        }
        if world
            .get::<crate::pipeline::WaitingForChildren>(entity)
            .is_some()
        {
            let outstanding = world
                .get::<SubAgentChildren>(entity)
                .map(|c| {
                    c.children
                        .iter()
                        .filter(|&&child| {
                            world
                                .get::<AgentState>(child)
                                .is_some_and(|s| !crate::pipeline::is_terminal_status(&s.status))
                        })
                        .count()
                })
                .unwrap_or(0);
            return Some(WaitReason::Children { outstanding });
        }
        if world.get::<AwaitingInteraction>(entity).is_some() {
            // The hub is keyed by agent id, and one agent can only be parked on
            // one prompt at a time, so the first match is the one blocking it.
            let kind = self
                .interactions
                .pending()
                .into_iter()
                .find(|(agent_id, _)| *agent_id == state.agent_id)
                .map(|(_, req)| req.kind);
            return Some(match kind {
                Some(leviath_core::interaction::InteractionKind::ToolApproval) => {
                    WaitReason::ToolApproval
                }
                _ => WaitReason::UserPrompt,
            });
        }
        None
    }

    /// List every known live run with the context an operator needs to read its
    /// status: why it is waiting, where it is, and when it last moved.
    fn list(&self) -> Vec<RunListEntry> {
        let world = self.world.world();
        self.by_run_id
            .iter()
            .filter_map(|(run_id, &entity)| {
                let state = world.get::<AgentState>(entity)?;
                let metadata = world.get::<RunMetadata>(entity);
                Some(RunListEntry {
                    run_id: run_id.clone(),
                    status: state.status.clone(),
                    wait_reason: self.wait_reason(entity),
                    stage: state.current_stage.clone(),
                    stage_index: world
                        .get::<crate::pipeline::StageCursor>(entity)
                        .map(|c| c.index),
                    num_stages: metadata.map(|m| m.num_stages),
                    iteration: state.iteration,
                    tool_calls: world.get::<TokenTotals>(entity).map_or(0, |t| t.tool_calls),
                    last_progress_at: world
                        .get::<crate::pipeline::PersistWatermark>(entity)
                        .and_then(|w| w.last_progress_at()),
                    unattended: metadata.is_some_and(|m| m.unattended),
                })
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
                    // partially-built entity - the run just never registers.
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
                // A failed spawn must leave a trace daemon-side: the error goes
                // back over the socket to a client that may have already exited,
                // and nothing is written to disk, so without this log line the
                // failure is invisible (issue #107).
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
                // Cancel is unconditional: it either takes effect in the world
                // (root plus every descendant) or, when the run can't be held
                // there at all, is forced onto its on-disk state. It reports
                // `false` only when there is genuinely no such run anywhere -
                // otherwise a run whose blueprint had moved stayed `running` on
                // disk forever with no way to get rid of it.
                let ok = self.cancel_tree(&run_id)
                    || self
                        .force_terminator
                        .as_mut()
                        .is_some_and(|terminate| terminate(&run_id));
                let _ = reply.send(ok);
            }
            ControlOp::List { reply } => {
                let _ = reply.send((self.list(), self.health()));
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
    /// when shutdown fires or the control channel closes - and before returning,
    /// **flushes all queued persistence to disk** ([`Self::flush_and_stop`]) so a
    /// clean daemon shutdown never loses a dirty agent's final snapshot.
    pub async fn serve(&mut self, mut control_rx: UnboundedReceiver<ControlOp>) {
        let wake = self.world.wake_handle();
        let shutdown = self.world.shutdown_handle();
        // `interval_at` rather than `interval`: the latter's first tick is
        // immediately ready, which would spin one pointless pass at startup.
        // `Delay` keeps a slow drive from queueing a burst of catch-up ticks.
        let mut redrive =
            tokio::time::interval_at(tokio::time::Instant::now() + self.redrive, self.redrive);
        redrive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        'serve: loop {
            self.world.run_to_fixed_point();
            self.emit_events();
            tokio::select! {
                _ = wake.notified() => {}
                _ = shutdown.notified() => break 'serve,
                // The backstop. Everything else here is edge-triggered, so a
                // release or completion that forgets to wake us would otherwise
                // park the daemon indefinitely with work left to do. Re-driving
                // on a timer bounds that to one interval, and is where the lane
                // heartbeat reports what the loop is actually waiting on.
                _ = redrive.tick() => self.observe_redrive(),
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
        fn exec_for(
            &self,
            _e: Entity,
            calls: Vec<leviath_providers::ToolCall>,
            _progress: crate::pipeline::ToolProgress,
        ) -> BoxedToolExec {
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
            None,
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

    /// A [`ForceTerminator`] that records each run id it was asked to terminate
    /// and reports success for everything but `"never-existed"`. Shared by the
    /// tests that expect it to fire and the ones that expect it not to, so its
    /// body is exercised rather than existing only to go unused.
    fn recording_terminator(seen: Arc<Mutex<Vec<String>>>) -> ForceTerminator {
        Box::new(move |run_id| {
            seen.lock().unwrap().push(run_id.to_string());
            run_id != "never-existed"
        })
    }

    /// A [`Reloader`] that pages any run id in as a fresh agent.
    fn paging_reloader() -> Reloader {
        Box::new(|world, run_id| Some(world.spawn_agent((agent_state(run_id),))))
    }

    async fn ask<T>(host: &mut WorldHost, make: impl FnOnce(oneshot::Sender<T>) -> ControlOp) -> T {
        let (tx, rx) = oneshot::channel();
        host.handle(make(tx));
        rx.await.unwrap()
    }

    /// A provider whose call never returns while `hang` is set - the stalled
    /// request that holds its pool permit until something cancels the job.
    ///
    /// The non-hanging arm is not decoration: a body that only ever diverges has
    /// no reachable return, so the answering path is what keeps this honest (and
    /// measurable) - the same shape `inference_bridge`'s `Scripted` uses.
    struct Hangs {
        hang: bool,
    }
    #[async_trait::async_trait]
    impl Provider for Hangs {
        async fn infer(
            &self,
            _req: InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            if self.hang {
                std::future::pending().await
            } else {
                Err(ProviderError::Other("not hanging".to_string()))
            }
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "hangs"
        }
        fn capabilities(&self, _m: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    /// The stalling provider's own surface. `infer` never returns by design, so
    /// it is reached under a timeout; the rest are plain accessors the dispatch
    /// path reads.
    #[tokio::test]
    async fn the_hanging_provider_answers_everything_except_a_hanging_infer() {
        fn request() -> InferenceRequest {
            InferenceRequest {
                system: vec![],
                messages: vec![],
                model: "m".to_string(),
                max_tokens: 1,
                temperature: 0.0,
                tools: vec![],
                extra: serde_json::Value::Null,
                request_timeout_secs: None,
            }
        }
        let p = Hangs { hang: true };
        assert_eq!(p.name(), "hangs");
        assert_eq!(p.count_tokens("t", "m").await, 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        let _ = p.capabilities("m");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), p.infer(request()))
                .await
                .is_err(),
            "hanging: the whole point is that the call never lands"
        );
        // ...and the answering arm, so the call has a reachable way out.
        assert!(Hangs { hang: false }.infer(request()).await.is_err());
    }

    /// A host whose only provider hangs, with model `m` capped at `limit`
    /// concurrent inferences - so the second agent to want a slot is starved
    /// until the first one gives its permit back.
    fn host_with_full_pool(limit: usize) -> WorldHost {
        let mut registry = crate::providers::ProviderRegistry::new();
        registry.register("script".to_string(), Arc::new(Hangs { hang: true }));
        let mut pools = InferencePoolConfig::new();
        pools.set_limit("m", limit);
        WorldHost::new(PipelineWorld::new(
            registry,
            Arc::new(NoTools),
            pools,
            1,
            None,
            Handle::current(),
        ))
    }

    /// How long [`serve_until_inferring`] waits at each park before calling the loop
    /// wedged. A wake that is coming lands as soon as the freeing task is
    /// polled, so this is only ever spent proving the *absence* of one.
    const PARK: std::time::Duration = std::time::Duration::from_millis(250);

    /// Drive `host` exactly the way [`WorldHost::serve`] does - run the world to
    /// quiescence, then park until something wakes it - and report whether
    /// `entity` got dispatched within `rounds` parks. `false` means the loop
    /// parked with nothing left to wake it, which is the daemon wedging.
    ///
    /// Takes the entity rather than a predicate closure on purpose: a generic
    /// parameter would give each call site its own instantiation, and no single
    /// one of them exercises both the "it happened" and "we wedged" exits.
    async fn serve_until_inferring(
        host: &mut WorldHost,
        rounds: usize,
        park: std::time::Duration,
        entity: Entity,
    ) -> bool {
        let wake = host.world_mut().wake_handle();
        for _ in 0..rounds {
            host.world_mut().run_to_fixed_point();
            if is_inferring(host, entity) {
                return true;
            }
            if tokio::time::timeout(park, wake.notified()).await.is_err() {
                break; // parked with no wake pending - nothing will re-drive us
            }
        }
        false
    }

    /// Whether `entity` has been handed a pool permit and dispatched.
    fn is_inferring(host: &mut WorldHost, entity: Entity) -> bool {
        host.world_mut()
            .world()
            .get::<crate::pipeline::AwaitingInference>(entity)
            .is_some()
    }

    /// Regression for #189 ("slots=0 for hours, in_progress frozen").
    ///
    /// Releasing an inference permit has to wake the tick loop, because
    /// `dispatch_inference` leaves a slot-starved agent `ReadyToInfer` to be
    /// "retried on a later tick" - and the loop is event-driven, so a later tick
    /// only happens when something wakes it. A cancelled job frees its permit
    /// from a detached task, *after* the tick chain has already run to
    /// quiescence over the cancel. If that release is silent, the freed slot is
    /// invisible: capacity sits idle while every agent queued behind it stays
    /// parked, for as long as it takes some unrelated event to wake the loop.
    #[tokio::test]
    async fn releasing_a_cancelled_runs_permit_wakes_the_starved_agent_behind_it() {
        let mut host = host_with_full_pool(1);

        // Dispatch the holder first and on its own, so which agent wins the
        // single permit is decided here rather than by the parallel dispatch.
        let holder = spawn(&mut host, "run-a", "agent-a");
        host.world_mut().run_to_fixed_point();
        assert!(is_inferring(&mut host, holder), "the holder takes the slot");

        let starved = spawn(&mut host, "run-b", "agent-b");
        host.world_mut().run_to_fixed_point();
        assert!(
            !is_inferring(&mut host, starved),
            "the second agent is starved on the full pool"
        );
        // And it stays starved for as long as the slot is genuinely held - the
        // cap is real, not an artifact of the wake. Several rounds, because the
        // first park consumes the wake the spawn itself stored; the loop has to
        // reach a park with nothing pending before "wedged" means anything.
        assert!(
            !serve_until_inferring(&mut host, 3, PARK, starved).await,
            "no slot, no dispatch"
        );

        // Cancel the holder the way `lev cancel` does. The tick chain aborts its
        // in-flight work; the permit itself comes back later, on the job's task.
        assert!(
            ask(&mut host, |reply| ControlOp::Cancel {
                run_id: "run-a".to_string(),
                reply,
            })
            .await
        );

        assert!(
            serve_until_inferring(&mut host, 8, PARK, starved).await,
            "the freed slot must wake the loop so the starved agent can take it; \
             without that wake the daemon parks with capacity it cannot see"
        );
    }

    /// The backstop, on its own terms: `serve` must make progress from a timer
    /// alone, with nothing ever waking it. Whatever else goes silent - a release
    /// that forgets to notify, a lane that reports nothing - the daemon still
    /// re-examines the world instead of parking indefinitely.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_redrives_the_world_on_its_own_timer_with_no_wake() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Counts ticks from inside the schedule, so the assertion is about the
        // loop actually running - not about some state that a single startup
        // pass could equally have produced.
        static TICKS: AtomicUsize = AtomicUsize::new(0);
        TICKS.store(0, Ordering::SeqCst);
        fn count_ticks() {
            TICKS.fetch_add(1, Ordering::SeqCst);
        }

        let mut host = host_with(vec![]);
        host.world_mut().add_test_system(count_ticks);
        host.set_redrive_interval(std::time::Duration::from_millis(20));
        let shutdown = host.world_mut().shutdown_handle();

        let (op_tx, op_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            host.serve(op_rx).await;
        });

        // Nothing is ever sent on `op_tx`, nothing is spawned, and no wake is
        // signalled: an empty world quiesces immediately, so every tick past the
        // first handful is one the timer produced.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let ticks = TICKS.load(Ordering::SeqCst);
        shutdown.notify_one();
        drop(op_tx);
        handle.await.unwrap();

        assert!(
            ticks > 3,
            "the timer must keep driving the world with nothing waking it; saw {ticks} ticks"
        );
    }

    /// A two-stage linear blueprint (`one` -> `two`), for the stage-boundary
    /// tests. No transitions declared: `resolve_transition_sync` falls through to
    /// the next stage in order, which is the ordinary case.
    fn two_stage_blueprint() -> leviath_core::Blueprint {
        let layout = leviath_core::layout::ContextLayout::new(
            vec![leviath_core::layout::RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::Clearable,
                10_000,
            )],
            12_000,
        );
        let model =
            leviath_core::blueprint::ModelConfig::new("script".to_string(), "m".to_string());
        // Both stages end by running out of iterations, which is how a stage that
        // keeps calling tools finishes. That boundary is the one the driver used
        // to miss: `enforce_max_iterations` and `resolve_transition` both run in
        // the same tick, so the agent leaves `ReadyToInfer` and comes back to it
        // with every marker count exactly as it was.
        let mut one = leviath_core::Stage::new("one".to_string(), model.clone());
        one.max_iterations = Some(1);
        let mut two = leviath_core::Stage::new("two".to_string(), model);
        two.max_iterations = Some(1);
        let stages = vec![one, two];
        leviath_core::Blueprint::new("t".to_string(), "d".to_string(), stages, layout)
    }

    /// Spawn an agent that starts at stage `one` of [`two_stage_blueprint`].
    fn spawn_two_stage(host: &mut WorldHost, run_id: &str, agent_id: &str) -> Entity {
        let mut state = agent_state(agent_id);
        state.current_stage = "one".to_string();
        let e = host.world_mut().spawn_agent((
            AgentBlueprint(two_stage_blueprint()),
            StageCursor { index: 0 },
            state,
            crate::components::MessageInbox::default(),
            StageProgress::default(),
            StageInferences(vec![si(), si()]),
            StageSetups(vec![setup(), setup()]),
            VisitCounts::default(),
            window(),
            si(),
            setup().inference_config,
            ReadyToInfer,
        ));
        host.register(run_id, e);
        e
    }

    /// A response that asks for one tool call - what a working stage returns
    /// right up to the iteration that ends it.
    fn tool_call(id: &str) -> InferenceResponse {
        InferenceResponse {
            tool_calls: vec![leviath_providers::ToolCall {
                id: id.to_string(),
                name: "noop".to_string(),
                arguments: serde_json::Value::Null,
                thought_signature: None,
            }],
            ..text("working")
        }
    }

    /// Regression for #197 ("entering the next stage waits for the re-drive
    /// tick").
    ///
    /// `serve` is event-driven; its 30s re-drive is a correctness backstop for a
    /// wake that never came, not the mechanism ordinary work runs on. A stage
    /// boundary that only makes progress on the timer puts up to 30s of dead time
    /// on every transition - a five-stage run loses minutes to nothing.
    ///
    /// The re-drive is set out of reach here, so the run can only finish through
    /// the wake path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stage_boundary_is_crossed_without_waiting_for_the_redrive() {
        let mut host = host_with(vec![tool_call("c1"), tool_call("c2")]);
        host.set_redrive_interval(std::time::Duration::from_secs(3600));
        spawn_two_stage(&mut host, "run-a", "agent-a");

        let mut events = host.subscribe();
        let shutdown = host.world_mut().shutdown_handle();
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(async move { host.serve(op_rx).await });

        // Watch the event stream rather than the world: `serve` owns the host for
        // as long as it runs. Everything before `Completed` (spawn, status,
        // tokens) streams past on the way.
        let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event = events
                    .recv()
                    .await
                    .expect("the event stream must outlive the run");
                if let WorldEvent::Completed { status, .. } = event {
                    break status;
                }
            }
        })
        .await;

        shutdown.notify_one();
        drop(op_tx);
        handle.await.unwrap();

        assert_eq!(
            completed.expect("the run must reach stage two and finish on wakes alone"),
            "complete"
        );
    }

    /// The heartbeat's two levels. Under pressure it is worth an `info` line;
    /// idle it must not be, or a healthy daemon spams the log forever.
    #[tokio::test]
    async fn the_lane_heartbeat_distinguishes_pressure_from_idle() {
        leviath_testkit::with_tracing(|| async {
            // Idle: no agents, no pools touched, nothing queued.
            let mut host = host_with_full_pool(1);
            let idle = host.world_mut().lane_snapshot();
            assert!(!idle.is_under_pressure(), "an empty world is not pressured");
            assert_eq!(idle.inference_summary(), "none");
            host.log_lane_pressure(&idle); // the `debug` arm

            // Two agents, one slot: one infers, one is queued behind a full pool.
            spawn(&mut host, "run-a", "agent-a");
            spawn(&mut host, "run-b", "agent-b");
            host.world_mut().run_to_fixed_point();

            let busy = host.world_mut().lane_snapshot();
            assert_eq!(busy.agents.active, 2);
            assert_eq!(busy.inference_summary(), "m=1/1");
            assert!(
                busy.is_under_pressure(),
                "a full pool with active agents is exactly the state worth reporting"
            );
            host.log_lane_pressure(&busy); // the `info` arm
        })
        .await;
    }

    /// A daemon with work queued and nothing moving is what issue #191 reported,
    /// and until now it looked identical to a busy one. Each re-drive that finds
    /// the lanes full and the world unchanged is one dead cycle.
    #[tokio::test]
    async fn re_drives_that_go_nowhere_under_pressure_count_as_dead_cycles() {
        leviath_testkit::with_tracing(|| async {
            // Two agents, one inference slot, and a provider that never answers:
            // one is stuck mid-call, the other is queued behind a full pool.
            let mut host = host_with_full_pool(1);
            spawn(&mut host, "run-a", "agent-a");
            spawn(&mut host, "run-b", "agent-b");
            host.world_mut().run_to_fixed_point();
            host.emit_events();

            // The first re-drive has nothing to compare against.
            host.observe_redrive();
            assert_eq!(host.dead_cycles, 0, "the first cycle sets the baseline");

            host.observe_redrive();
            assert_eq!(host.dead_cycles, 1, "a whole interval, nothing moved");
            host.observe_redrive();
            assert_eq!(host.dead_cycles, 2, "and another - this is the `warn` arm");
        })
        .await;
    }

    /// Any sign of life clears the count. A daemon that moves once every few
    /// minutes is slow, not wedged, and must not accumulate towards relief.
    #[tokio::test]
    async fn a_run_that_moves_clears_the_dead_cycle_count() {
        let mut host = host_with_full_pool(1);
        let entity = spawn(&mut host, "run-a", "agent-a");
        spawn(&mut host, "run-b", "agent-b");
        host.world_mut().run_to_fixed_point();
        host.emit_events();
        host.observe_redrive();
        host.observe_redrive();
        assert_eq!(host.dead_cycles, 1, "wedged to begin with");

        // One run advances an iteration, which is exactly what the fingerprint
        // is built to notice.
        host.world_mut()
            .world_mut()
            .get_mut::<AgentState>(entity)
            .expect("the agent is loaded")
            .iteration += 1;
        host.emit_events();

        host.observe_redrive();
        assert_eq!(host.dead_cycles, 0, "something moved");
    }

    /// Fill the world's tool lane and queue one batch behind it, returning a
    /// handle that releases the blocking batch.
    ///
    /// Uses the world's real lane rather than poking the counters, because the
    /// point of relief is that the queued batch actually runs afterwards.
    async fn wedge_the_tool_lane(host: &mut WorldHost) -> crate::cancel::CancelToken {
        let snapshot = host.world_mut().lane_snapshot();
        let stage = host
            .world_mut()
            .world()
            .resource::<crate::pipeline::ToolStage>()
            .clone();
        // A cancel token rather than a `Notify`: it latches, so a batch that has
        // not started yet still sees the release rather than waiting for a
        // wake-up that already happened.
        let release = crate::cancel::CancelToken::new();
        let submit = |exec: crate::tool_bridge::BoxedToolExec| {
            stage.stats.enqueued();
            stage
                .jobs
                .send(crate::tool_bridge::ToolJob {
                    // The lane never looks at the entity; these batches belong to
                    // no agent.
                    entity: Entity::from_raw_u32(9_001).expect("a small index is a valid id"),
                    exec,
                    cancel: crate::cancel::CancelToken::new(),
                })
                .expect("the lane is serving");
        };
        // Every batch here blocks until `release` fires. That is deliberate: a
        // batch that can finish on its own makes the lane's occupancy a moving
        // target, and the counts these tests assert on stop being deterministic.
        // `release_the_lane` lets them all go at the end.
        let blocker = || {
            let held = release.clone();
            submit(Box::new(move || {
                Box::pin(async move {
                    held.cancelled().await;
                    Vec::new()
                })
            }));
        };
        // Take whatever capacity is still free, so the lane is genuinely full
        // rather than merely busy.
        for _ in 0..snapshot.tools_workers.saturating_sub(snapshot.tools_busy) {
            blocker();
        }
        // Wait for them to actually be holding it before queueing anything
        // behind them: batches race each other for a permit, so one submitted
        // alongside could get in first.
        await_full_lane(host).await;
        blocker(); // and one behind them, which can only run once there is room
        await_saturation(host).await;
        release
    }

    /// Block until every unit of the world's tool-lane capacity is held.
    async fn await_full_lane(host: &mut WorldHost) {
        await_lane(host, "the lane filled up", |snapshot| {
            snapshot.tools_busy >= snapshot.tools_workers
        })
        .await;
    }

    /// Block until the world's tool lane reports itself saturated.
    async fn await_saturation(host: &mut WorldHost) {
        await_lane(host, "the lane saturated", |snapshot| {
            snapshot.tools_saturated
        })
        .await;
    }

    /// Block until the world's tool lane drains its queue.
    async fn await_drained_queue(host: &mut WorldHost) {
        await_lane(host, "the queued batch got in", |snapshot| {
            snapshot.tools_queued == 0
        })
        .await;
    }

    /// Poll the lane until `done`, or fail with `context`. Bounded so a wedge in
    /// the code under test fails the run instead of hanging it.
    async fn await_lane(
        host: &mut WorldHost,
        context: &str,
        done: fn(&crate::world::LaneSnapshot) -> bool,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while !done(&host.world_mut().lane_snapshot()) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect(context);
    }

    /// Let every wedged batch finish and wait for the lane to empty, so the
    /// batches are exercised end to end rather than abandoned mid-await.
    ///
    /// Takes a slice rather than one token so a test that wedged the lane twice
    /// releases both before waiting; releasing one and waiting would wait for
    /// batches still held by the other.
    async fn release_the_lane(host: &mut WorldHost, releases: &[crate::cancel::CancelToken]) {
        for release in releases {
            release.cancel();
        }
        await_lane(host, "the lane emptied", |snapshot| {
            snapshot.tools_busy == 0 && snapshot.tools_queued == 0
        })
        .await;
    }

    /// The relief valve: a tool lane that has not drained in long enough gets
    /// wider, so whatever is queued behind the jam can run.
    ///
    /// Additive on purpose. Killing whatever holds the lane is the tempting
    /// reading of "reclaim stuck slots", and it is wrong: a run parked on an
    /// `ask_user` is behaving correctly, and an operator killing healthy
    /// `waiting` runs is the story behind issue #184.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lane_that_never_drains_is_widened_rather_than_emptied() {
        leviath_testkit::with_tracing(|| async {
            let mut host = host_with_full_pool(1);
            host.set_dead_cycles_before_relief(2);
            let release = wedge_the_tool_lane(&mut host).await;

            host.observe_redrive(); // baseline
            host.observe_redrive(); // 1
            assert_eq!(host.relief_granted, 0, "still inside the grace period");
            host.observe_redrive(); // 2 → relief
            assert_eq!(host.relief_granted, 1, "the lane got wider");
            assert_eq!(
                host.dead_cycles, 0,
                "the streak restarts so relief is not granted again immediately"
            );
            assert_eq!(host.health().tools_workers, 2);

            // Which is the whole point: the batch that was queued behind the
            // jam gets a permit, while the batch already holding one keeps it.
            await_drained_queue(&mut host).await;
            assert_eq!(host.world_mut().lane_snapshot().tools_busy, 2);
            release_the_lane(&mut host, &[release]).await;
        })
        .await;
    }

    /// Relief is capped at one extra lane's worth over the daemon's life. If
    /// that much did not help, the problem is not capacity.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relief_stops_after_one_extra_lane_s_worth() {
        leviath_testkit::with_tracing(|| async {
            let mut host = host_with_full_pool(1);
            host.set_dead_cycles_before_relief(1);
            let release = wedge_the_tool_lane(&mut host).await;

            host.observe_redrive();
            host.observe_redrive();
            assert_eq!(host.relief_granted, 1);

            // Wedge it again at the wider width and keep pushing: the budget is
            // spent, so nothing more is handed out.
            let release_two = wedge_the_tool_lane(&mut host).await;
            for _ in 0..4 {
                host.observe_redrive();
            }
            assert_eq!(host.relief_granted, 1, "the budget was already spent");
            release_the_lane(&mut host, &[release, release_two]).await;
        })
        .await;
    }

    /// Relief is off when the operator says so, and detection carries on
    /// regardless - the streak is still counted and still reported.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relief_can_be_turned_off_without_turning_off_detection() {
        leviath_testkit::with_tracing(|| async {
            let mut host = host_with_full_pool(1);
            host.set_dead_cycles_before_relief(0);
            let release = wedge_the_tool_lane(&mut host).await;

            for _ in 0..4 {
                host.observe_redrive();
            }
            assert_eq!(host.relief_granted, 0, "relief is disabled");
            assert_eq!(host.dead_cycles, 3, "but the streak is still counted");
            release_the_lane(&mut host, &[release]).await;
        })
        .await;
    }

    /// Every re-drive hands the sink a daemon-wide sample, including the quiet
    /// ones. A wedged daemon produces no per-run telemetry at all, which is
    /// exactly why the health sample cannot be conditional on something having
    /// happened.
    #[tokio::test]
    async fn each_re_drive_reports_lane_health_to_the_telemetry_sink() {
        let sink = Arc::new(leviath_core::telemetry::MemorySink::default());
        let mut host = host_with_full_pool(1);
        host.world_mut()
            .world_mut()
            .insert_resource(crate::telemetry::Telemetry(sink.clone()));
        spawn(&mut host, "run-a", "agent-a");
        spawn(&mut host, "run-b", "agent-b");
        host.world_mut().run_to_fixed_point();
        host.emit_events();

        host.observe_redrive();
        host.observe_redrive();

        let samples = sink.lane_samples();
        assert_eq!(samples.len(), 2, "one per re-drive");
        assert_eq!(samples[0].dead_cycles, 0);
        assert_eq!(samples[1].dead_cycles, 1, "the streak is carried through");
        assert_eq!(samples[1].agents_active, 2);
    }

    /// Stillness on its own is not a dead cycle. An idle daemon has nothing
    /// queued and nothing to do, and counting it would fire relief at every quiet
    /// spell.
    #[tokio::test]
    async fn an_idle_daemon_never_counts_a_dead_cycle() {
        let mut host = host_with_full_pool(1);
        host.emit_events();
        for _ in 0..3 {
            host.observe_redrive();
        }
        assert_eq!(host.dead_cycles, 0, "no pressure, no dead cycles");
    }

    /// Terminal agents are counted apart from live ones, so "nothing is running"
    /// can't be read as "everything is running" just because finished runs are
    /// still loaded.
    #[tokio::test]
    async fn the_lane_snapshot_counts_agents_by_status() {
        let mut host = host_with(vec![]);
        let active = spawn(&mut host, "run-active", "a");
        let paused = spawn(&mut host, "run-paused", "b");
        let waiting = spawn(&mut host, "run-waiting", "c");
        let done = spawn(&mut host, "run-done", "d");
        let idle = spawn(&mut host, "run-idle", "e");
        host.world_mut().set_status(paused, AgentStatus::Paused);
        host.world_mut().set_status(waiting, AgentStatus::Waiting);
        host.world_mut().set_status(done, AgentStatus::Complete);
        host.world_mut().set_status(idle, AgentStatus::Idle);

        let counts = host.world_mut().lane_snapshot().agents;
        assert_eq!(counts.active, 1);
        assert_eq!(counts.paused, 1);
        assert_eq!(counts.waiting, 1);
        assert_eq!(counts.terminal, 1);
        assert_eq!(counts.idle, 1);
        assert_eq!(
            counts.to_string(),
            "active=1 waiting=1 paused=1 idle=1 terminal=1"
        );
        // The other two terminal statuses land in the same bucket.
        host.world_mut().set_status(active, AgentStatus::Cancelled);
        host.world_mut().set_status(
            paused,
            AgentStatus::Error {
                message: "boom".to_string(),
            },
        );
        assert_eq!(host.world_mut().lane_snapshot().agents.terminal, 3);
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

        let (list, _health) = ask(&mut host, |reply| ControlOp::List { reply }).await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].run_id, "run-a");
        assert_eq!(list[0].status, AgentStatus::Active);
        // An active run is not waiting on anything, so there is nothing to explain.
        assert_eq!(list[0].wait_reason, None);

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
            Some(AgentStatus::Paused)
        );

        // Pausing an already-paused run refuses rather than reporting success.
        assert!(
            !ask(&mut host, |reply| ControlOp::Pause {
                run_id: "run-a".to_string(),
                reply
            })
            .await
        );

        assert!(
            ask(&mut host, |reply| ControlOp::Resume {
                run_id: "run-a".to_string(),
                reply
            })
            .await
        );
        assert_eq!(
            host.world.agent_status(host.by_run_id["run-a"]),
            Some(AgentStatus::Active)
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
        // not unwind the daemon's serve task - the run just fails to start.
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

    /// `send_to_agent` and `kill_agent` took any run id at all, so an agent
    /// could reach into an unrelated run - cancel it, inject text, or hand it
    /// data that arrives `Public` regardless of the sender's taint. That last
    /// one is a laundering channel straight through taint tracking.
    /// The converse of the refusal: a run the caller *did* spawn is reachable,
    /// so scoping did not simply block everything. This also walks the
    /// `SubAgentChildren` link rather than matching the caller itself.
    #[tokio::test]
    async fn subagent_ops_reach_a_run_the_caller_spawned() {
        let mut host = host_with(vec![]);
        let parent = spawn(&mut host, "parent", "parent");
        let child = spawn(&mut host, "child", "child");
        host.world_mut()
            .world_mut()
            .entity_mut(parent)
            .insert(SubAgentChildren {
                children: vec![child],
                max_child_depth: 3,
            });

        let delivered = ask_sub(&mut host, |reply| SubAgentOp::Send {
            run_id: "child".to_string(),
            caller_run_id: "parent".to_string(),
            content: "carry on".to_string(),
            target_region: None,
            reply,
        })
        .await;
        assert!(delivered, "a run we spawned is ours to message");
    }

    #[tokio::test]
    async fn subagent_ops_refuse_a_run_outside_the_callers_tree() {
        let mut host = host_with(vec![]);
        spawn(&mut host, "run-a", "run-a");
        spawn(&mut host, "outsider", "outsider");

        let delivered = ask_sub(&mut host, |reply| SubAgentOp::Send {
            run_id: "outsider".to_string(),
            caller_run_id: "run-a".to_string(),
            content: "take this".to_string(),
            target_region: None,
            reply,
        })
        .await;
        assert!(!delivered, "a run we did not spawn is not ours to message");

        let killed = ask_sub(&mut host, |reply| SubAgentOp::Kill {
            run_id: "outsider".to_string(),
            caller_run_id: "run-a".to_string(),
            reply,
        })
        .await;
        assert!(!killed, "nor ours to cancel");

        // A run id that resolves to nothing at all is likewise not ours - the
        // walk never starts, rather than defaulting to reachable.
        let phantom = ask_sub(&mut host, |reply| SubAgentOp::Send {
            run_id: "no-such-run".to_string(),
            caller_run_id: "run-a".to_string(),
            content: "hello?".to_string(),
            target_region: None,
            reply,
        })
        .await;
        assert!(!phantom, "an unknown run id is in nobody's tree");
    }

    #[tokio::test]
    async fn subagent_send_delivers_to_inbox() {
        let mut host = host_with(vec![]);
        spawn(&mut host, "run-a", "run-a");
        let ok = ask_sub(&mut host, |reply| SubAgentOp::Send {
            run_id: "run-a".to_string(),
            caller_run_id: "run-a".to_string(),
            content: "hello child".to_string(),
            target_region: None,
            reply,
        })
        .await;
        assert!(ok);
    }

    /// The op's `target_region` reaches the named region, not just the
    /// default conversation. The `send_to_agent` tool advertised this
    /// parameter from the start, but the op had no field to carry it, so it
    /// was silently dropped on this path.
    #[tokio::test]
    async fn subagent_send_delivers_into_the_target_region() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "run-a");
        host.world
            .world_mut()
            .get_mut::<crate::components::ContextWindow>(e)
            .unwrap()
            .add_region(Region::new(
                "notes".to_string(),
                RegionKind::Clearable,
                5000,
            ));

        let ok = ask_sub(&mut host, |reply| SubAgentOp::Send {
            run_id: "run-a".to_string(),
            caller_run_id: "run-a".to_string(),
            content: "filed under notes".to_string(),
            target_region: Some("notes".to_string()),
            reply,
        })
        .await;
        assert!(ok);

        host.world.tick(); // intake → inbox → window
        let window = host
            .world
            .world()
            .get::<crate::components::ContextWindow>(e)
            .unwrap();
        assert!(window.get_region("notes").unwrap().current_tokens > 0);
        assert_eq!(window.get_region("conversation").unwrap().current_tokens, 0);
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
            caller_run_id: "parent".to_string(),
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
            caller_run_id: "ghost".to_string(),
            reply,
        })
        .await;
        assert!(!miss);
    }

    /// A user-facing cancel must reach the sub-agent tree, not just the root -
    /// otherwise the children keep running with nobody to report to. Before this,
    /// only the model-facing `kill_agent` tool cascaded.
    #[tokio::test]
    async fn cancel_cascades_to_the_whole_tree() {
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

        assert!(
            ask(&mut host, |reply| ControlOp::Cancel {
                run_id: "parent".to_string(),
                reply
            })
            .await
        );
        assert_eq!(
            host.world.agent_status(host.by_run_id["child"]),
            Some(AgentStatus::Cancelled),
            "cancelling the parent cancels its children"
        );
    }

    /// A child that was already reaped is skipped rather than tripping the
    /// cancel: `SubAgentChildren` still names it, but the entity is gone, so
    /// there is no agent id to close interactions for.
    #[tokio::test]
    async fn cancel_tolerates_a_child_that_has_already_been_reaped() {
        let mut host = host_with(vec![]);
        let parent = spawn(&mut host, "parent", "parent");
        let ghost = host.world_mut().spawn_agent((agent_state("ghost"),));
        host.world_mut()
            .world_mut()
            .entity_mut(parent)
            .insert(SubAgentChildren {
                children: vec![ghost],
                max_child_depth: 3,
            });
        host.world_mut().world_mut().despawn(ghost);

        assert!(
            ask(&mut host, |reply| ControlOp::Cancel {
                run_id: "parent".to_string(),
                reply
            })
            .await,
            "the parent is still cancelled"
        );
        assert_eq!(
            host.world.agent_status(parent),
            Some(AgentStatus::Cancelled)
        );
    }

    /// Cancelling a run closes its open prompts. The blocked `ask` waits off the
    /// lane, so it no longer starves anyone, but a prompt left open for a run
    /// that no longer exists is still surfaced to whoever is meant to answer it.
    #[tokio::test]
    async fn cancel_closes_the_runs_open_interactions() {
        let mut host = host_with(vec![]);
        let hub = host.interactions();
        spawn(&mut host, "run-a", "agent-a");

        let backend = hub.backend_for("agent-a");
        let asking = tokio::spawn(async move {
            backend
                .ask(InteractionRequest::free_text("q", "ask", "stage", true))
                .await
        });
        // Wait for the ask to register, then let the host emit it - so the
        // emitted-interaction set is non-empty and the cancel has something to
        // prune, rather than pruning an empty set.
        while hub.pending().is_empty() {
            tokio::task::yield_now().await;
        }
        host.emit_events();
        assert!(
            !host.emitted_interactions.is_empty(),
            "the open request was emitted"
        );

        ask(&mut host, |reply| ControlOp::Cancel {
            run_id: "run-a".to_string(),
            reply,
        })
        .await;

        // The blocked future is released rather than parked forever. Bounded,
        // because the regression this guards *is* an unbounded wait: without the
        // per-agent cancel this await simply never returns, and a test that hangs
        // rather than fails is worse than no test.
        tokio::time::timeout(std::time::Duration::from_secs(5), asking)
            .await
            .expect("cancelling the run releases its blocked ask")
            .expect("the ask task did not panic");
        // ...and the request stops being advertised to `lev respond` / the
        // dashboard for a run that is going away.
        assert!(hub.pending().is_empty(), "no orphaned prompt is left open");
        assert!(
            host.emitted_interactions.is_empty(),
            "and it is pruned from the emitted set, not re-announced forever"
        );
    }

    /// The floor under every kill: a run the reloader can't rebuild must still be
    /// terminated, via the daemon's on-disk force-terminator. Replying `false` and
    /// writing nothing is what made such a run permanent.
    #[tokio::test]
    async fn cancel_falls_back_to_the_force_terminator_when_the_world_cannot_hold_the_run() {
        let mut host = host_with(vec![]);
        // A reloader that always declines - the deleted-blueprint case.
        host.set_reloader(Box::new(|_world, _run_id| None));
        let terminated = Arc::new(Mutex::new(Vec::new()));
        host.set_force_terminator(recording_terminator(terminated.clone()));

        assert!(
            ask(&mut host, |reply| ControlOp::Cancel {
                run_id: "unreloadable".to_string(),
                reply
            })
            .await,
            "a run that can't be reloaded is still terminated"
        );
        assert!(
            !ask(&mut host, |reply| ControlOp::Cancel {
                run_id: "never-existed".to_string(),
                reply
            })
            .await,
            "`false` is reserved for a run that exists nowhere"
        );
        assert_eq!(
            *terminated.lock().unwrap(),
            vec!["unreloadable".to_string(), "never-existed".to_string()]
        );
    }

    /// A live run is cancelled in the world; the on-disk fallback is not consulted
    /// (the persistence lane records the status change).
    #[tokio::test]
    async fn cancel_does_not_force_terminate_a_run_it_could_cancel() {
        let mut host = host_with(vec![]);
        spawn(&mut host, "run-a", "agent-a");
        let terminated = Arc::new(Mutex::new(Vec::new()));
        host.set_force_terminator(recording_terminator(terminated.clone()));

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
        assert!(
            terminated.lock().unwrap().is_empty(),
            "the disk fallback stayed unused"
        );
    }

    /// Agents that enter the world outside a `Spawn` op (fan-out workers, built
    /// directly by the fan-out spawner) are adopted into the run-id map, so they
    /// are listed, reaped and - the point here - cancellable by id. Left
    /// unregistered, a cancel missed the map and paged a *second* copy of the run
    /// in from disk while the original kept going.
    #[tokio::test]
    async fn unregistered_world_agents_are_adopted_and_become_cancellable() {
        let mut host = host_with(vec![]);
        let entity = host.world_mut().spawn_agent((
            agent_state("worker"),
            RunMetadata {
                run_id: "worker-run".to_string(),
                agent_name: "w".to_string(),
                agent_path: String::new(),
                task: String::new(),
                model: None,
                workdir: String::new(),
                num_stages: 1,
                started_at: 0,
                parent_run_id: None,
                metadata: Default::default(),
                callback_url: None,
                callback_secret: None,
                title: None,
                unattended: false,
            },
        ));
        assert!(
            !host.by_run_id.contains_key("worker-run"),
            "not registered by the spawn itself"
        );

        host.emit_events();

        assert_eq!(host.live_entity("worker-run"), Some(entity), "adopted");
        // A reloader that would mint a duplicate if the map were still missing it.
        host.set_reloader(paging_reloader());
        assert!(
            ask(&mut host, |reply| ControlOp::Cancel {
                run_id: "worker-run".to_string(),
                reply
            })
            .await
        );
        assert_eq!(
            host.world.agent_status(entity),
            Some(AgentStatus::Cancelled),
            "the original entity is cancelled, not a reloaded copy"
        );
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
        spawn(&mut host, "run-a", "agent-a");
        let shutdown = host.world_mut().shutdown_handle();
        // Watch the event stream rather than the entity: a run that finishes is
        // reaped out of the world once it has been seen terminal, so the
        // broadcast is the durable record that it ran to completion.
        let mut events = host.subscribe();
        let (op_tx, op_rx) = mpsc::unbounded_channel();

        let handle = tokio::spawn(async move {
            host.serve(op_rx).await;
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

        // The agent ran to completion under the serve loop.
        let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(WorldEvent::Completed { run_id, status, .. }) = events.recv().await {
                    return (run_id, status);
                }
            }
        })
        .await
        .expect("the serve loop must drive the agent to a terminal status");
        assert_eq!(completed, ("run-a".to_string(), "complete".to_string()));

        shutdown.notify_one();
        handle.await.unwrap();
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
        // An inert parent: no `ReadyToInfer`, so it never infers, never errors on
        // the empty response script, and stays live for the child to attach to.
        let parent = host.world_mut().spawn_agent((agent_state("parent"),));
        host.register("parent", parent);
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
        assert_eq!(status_str(&AgentStatus::Paused), "paused");
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
                unattended: false,
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

        // Parked on a human prompt (`AwaitingInteraction`) - the reported bug:
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

        // Many serve passes - none of them may reap a Waiting agent.
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
        host.set_reloader(paging_reloader());
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
            Entity::from_raw_u32(1).expect("a small literal index is always a valid entity id"),
            vec![leviath_providers::ToolCall {
                id: "c".to_string(),
                name: "n".to_string(),
                arguments: serde_json::Value::Null,
                thought_signature: None,
            }],
            crate::pipeline::noop_progress(),
        );
        assert_eq!(exec().await, vec![("c".to_string(), String::new())]);
    }

    #[tokio::test]
    async fn list_skips_despawned_entity() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "agent-a");
        // Despawn the entity behind the world's back; the run-id map is now stale.
        host.world_mut().world_mut().despawn(e);

        let (list, _health) = ask(&mut host, |reply| ControlOp::List { reply }).await;
        assert!(list.is_empty()); // stale mapping filtered out
        let status = ask(&mut host, |reply| ControlOp::Status {
            run_id: "run-a".to_string(),
            reply,
        })
        .await;
        assert_eq!(status, None);
    }

    // ─── Wait reasons (issue #184) ───────────────────────────────────────────

    /// Park `entity` at `Waiting` with `marker` attached, the way the engine
    /// would, and ask the host to explain it.
    fn waiting_because(
        host: &mut WorldHost,
        entity: Entity,
        attach: impl FnOnce(&mut bevy_ecs::world::EntityWorldMut),
    ) -> Option<WaitReason> {
        {
            let world = host.world_mut().world_mut();
            world
                .get_mut::<AgentState>(entity)
                .expect("spawned agent has state")
                .status = AgentStatus::Waiting;
            let mut e = world.entity_mut(entity);
            attach(&mut e);
        }
        host.wait_reason(entity)
    }

    /// A run that is not waiting has nothing to explain, whatever markers it
    /// happens to be carrying.
    #[tokio::test]
    async fn wait_reason_is_none_unless_the_agent_is_waiting() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "run-a");
        host.world_mut()
            .world_mut()
            .entity_mut(e)
            .insert(crate::pipeline::WaitingForChildren);
        assert_eq!(host.wait_reason(e), None);
    }

    /// An entity the world no longer holds cannot be explained either.
    #[tokio::test]
    async fn wait_reason_is_none_for_an_unknown_entity() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "run-a");
        host.world_mut().world_mut().despawn(e);
        assert_eq!(host.wait_reason(e), None);
    }

    /// `Waiting` with nothing claiming it: report nothing rather than guess.
    #[tokio::test]
    async fn wait_reason_is_none_when_nothing_claims_the_wait() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "run-a");
        assert_eq!(waiting_because(&mut host, e, |_| {}), None);
    }

    #[tokio::test]
    async fn wait_reason_reports_a_taint_gate() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "run-a");
        let reason = waiting_because(&mut host, e, |entity| {
            entity.insert(crate::gate_prompt::AwaitingGatePrompt(1));
        });
        assert_eq!(reason, Some(WaitReason::TaintGate));
    }

    #[tokio::test]
    async fn wait_reason_reports_an_interaction_point() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "run-a");
        let reason = waiting_because(&mut host, e, |entity| {
            entity.insert(crate::interaction_points::AwaitingInteractionPoint);
        });
        assert_eq!(reason, Some(WaitReason::InteractionPoint));
    }

    /// A stage holding for sub-agents counts only the children that have not
    /// finished - the whole point is telling the operator how much is left.
    #[tokio::test]
    async fn wait_reason_counts_unfinished_children() {
        let mut host = host_with(vec![]);
        let parent = spawn(&mut host, "run-a", "run-a");
        let running = spawn(&mut host, "run-b", "run-b");
        let done = spawn(&mut host, "run-c", "run-c");
        {
            let world = host.world_mut().world_mut();
            world
                .get_mut::<AgentState>(done)
                .expect("child has state")
                .status = AgentStatus::Complete;
        }
        let reason = waiting_because(&mut host, parent, |entity| {
            entity.insert((
                crate::pipeline::WaitingForChildren,
                SubAgentChildren {
                    children: vec![running, done],
                    max_child_depth: 3,
                },
            ));
        });
        assert_eq!(reason, Some(WaitReason::Children { outstanding: 1 }));
    }

    /// The marker can outlive the child list (a reload that lost them); report
    /// the wait rather than dropping it.
    #[tokio::test]
    async fn wait_reason_reports_children_with_none_recorded() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "run-a");
        let reason = waiting_because(&mut host, e, |entity| {
            entity.insert(crate::pipeline::WaitingForChildren);
        });
        assert_eq!(reason, Some(WaitReason::Children { outstanding: 0 }));
    }

    /// Open a real hub request for `agent_id` and leave it pending, returning
    /// the task holding it (dropping the host cancels it).
    fn open_prompt(
        host: &WorldHost,
        agent_id: &str,
        request: InteractionRequest,
    ) -> tokio::task::JoinHandle<InteractionResponse> {
        let backend = host.interactions().backend_for(agent_id.to_string());
        tokio::spawn(async move {
            use crate::dynamic_interaction::InteractionBackend;
            backend.ask(request).await
        })
    }

    /// Let the spawned `ask` reach its first poll, so its request is registered
    /// before the assertion looks for it. `submit` inserts before it awaits, so
    /// yielding is enough - no sleeping, and no timeout branch to leave uncovered.
    async fn await_pending(host: &WorldHost, agent_id: &str) {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            host.interactions()
                .pending()
                .iter()
                .any(|(id, _)| id == agent_id),
            "the hub registered a request for {agent_id}"
        );
    }

    #[tokio::test]
    async fn wait_reason_distinguishes_a_tool_approval_from_a_question() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "run-a");

        let approval = open_prompt(
            &host,
            "run-a",
            InteractionRequest::tool_approval("req-1", "shell", serde_json::json!({}), "implement"),
        );
        await_pending(&host, "run-a").await;
        let reason = waiting_because(&mut host, e, |entity| {
            entity.insert(AwaitingInteraction);
        });
        assert_eq!(reason, Some(WaitReason::ToolApproval));
        // Release the prompt (rather than abandoning it) so the awaiting task
        // finishes instead of leaking into the next case.
        assert_eq!(host.interactions().cancel_for_agent("run-a"), 1);
        approval.await.expect("the asking task finishes");

        let question = open_prompt(
            &host,
            "run-a",
            InteractionRequest::free_text("req-2", "which one?", "implement", true),
        );
        await_pending(&host, "run-a").await;
        assert_eq!(host.wait_reason(e), Some(WaitReason::UserPrompt));
        assert_eq!(host.interactions().cancel_for_agent("run-a"), 1);
        question.await.expect("the asking task finishes");
    }

    /// The marker without a matching hub entry (the request cleared in the same
    /// tick) still reads as a prompt rather than as nothing.
    #[tokio::test]
    async fn wait_reason_falls_back_to_user_prompt_without_a_hub_entry() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "run-a");
        let reason = waiting_because(&mut host, e, |entity| {
            entity.insert(AwaitingInteraction);
        });
        assert_eq!(reason, Some(WaitReason::UserPrompt));
    }

    /// A gate prompt opens a hub request of its own, so the gate-blocked agent
    /// carries `AwaitingInteraction` too. The specific marker has to win, or
    /// every gate would report as a generic prompt.
    #[tokio::test]
    async fn a_gate_outranks_the_generic_interaction_marker() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "run-a");
        let reason = waiting_because(&mut host, e, |entity| {
            entity.insert((
                AwaitingInteraction,
                crate::gate_prompt::AwaitingGatePrompt(1),
            ));
        });
        assert_eq!(reason, Some(WaitReason::TaintGate));
    }

    /// A fan-out parent reports how many workers are left, so "waiting" reads as
    /// progress against a denominator rather than an unexplained stall.
    #[tokio::test]
    async fn wait_reason_counts_outstanding_fan_out_workers() {
        let mut host = host_with(vec![]);
        let parent = spawn(&mut host, "run-a", "run-a");
        let worker = spawn(&mut host, "run-b", "run-b");
        {
            let world = host.world_mut().world_mut();
            world
                .get_mut::<AgentState>(parent)
                .expect("parent has state")
                .status = AgentStatus::Waiting;
            // One worker in flight and two items not yet started ⇒ three left.
            crate::fanout::restore_fan_out_waiting(
                world,
                parent,
                crate::fanout::FanOutState {
                    config: leviath_core::blueprint::FanOutConfig {
                        worker_agent: None,
                        worker_stage: Some("work".to_string()),
                        worker_query: None,
                        merge_stage: None,
                        max_workers: 2,
                        on_worker_failure: Default::default(),
                        split_prompt: String::new(),
                    },
                    max_workers: 2,
                    pending: vec![
                        crate::fanout::WorkItem::default(),
                        crate::fanout::WorkItem::default(),
                    ],
                    active: vec![("item-1".to_string(), "run-b".to_string())],
                    summaries: Vec::new(),
                    failures: Vec::new(),
                },
                &|run_id| (run_id == "run-b").then_some(worker),
            );
        }
        assert_eq!(
            host.wait_reason(parent),
            Some(WaitReason::FanOutWorkers { outstanding: 3 })
        );
    }

    /// With run metadata attached, the listing reports the blueprint's shape and
    /// whether the run is unattended - an unattended run sitting on a prompt is
    /// the shape of a bug.
    #[tokio::test]
    async fn list_reports_blueprint_shape_and_unattended() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "run-a");
        host.world_mut().world_mut().entity_mut(e).insert((
            RunMetadata {
                run_id: "run-a".to_string(),
                agent_name: "coder".to_string(),
                agent_path: "/tmp/agent".to_string(),
                task: "t".to_string(),
                model: None,
                workdir: "/tmp".to_string(),
                num_stages: 3,
                started_at: 0,
                parent_run_id: None,
                metadata: HashMap::new(),
                callback_url: None,
                callback_secret: None,
                title: None,
                unattended: true,
            },
            TokenTotals {
                tool_calls: 9,
                ..Default::default()
            },
            {
                let mut watermark = crate::pipeline::PersistWatermark::default();
                watermark.backdate(1_700);
                watermark
            },
        ));
        let (list, _health) = ask(&mut host, |reply| ControlOp::List { reply }).await;
        assert_eq!(list[0].num_stages, Some(3));
        assert_eq!(list[0].tool_calls, 9);
        assert!(list[0].unattended);
        assert_eq!(list[0].last_progress_at, Some(1_700));
    }

    /// The listing carries the reason and the progress context, not just a word.
    #[tokio::test]
    async fn list_explains_a_waiting_run() {
        let mut host = host_with(vec![]);
        let e = spawn(&mut host, "run-a", "run-a");
        waiting_because(&mut host, e, |entity| {
            entity.insert(crate::pipeline::WaitingForChildren);
        });
        let (list, _health) = ask(&mut host, |reply| ControlOp::List { reply }).await;
        assert_eq!(list.len(), 1);
        assert_eq!(
            list[0].wait_reason,
            Some(WaitReason::Children { outstanding: 0 })
        );
        assert_eq!(list[0].stage_index, Some(0));
        // No RunMetadata on this fixture, so there is nothing to claim about the
        // blueprint's shape or how it was launched.
        assert_eq!(list[0].num_stages, None);
        assert!(!list[0].unattended);
    }

    #[test]
    fn every_world_event_variant_carries_its_run_id() {
        let rid = "run-x".to_string();
        let aid = "agent-x".to_string();
        let events = vec![
            WorldEvent::Spawned {
                run_id: rid.clone(),
                agent_id: aid.clone(),
                blueprint: "b".to_string(),
            },
            WorldEvent::Status {
                run_id: rid.clone(),
                agent_id: aid.clone(),
                status: "active".to_string(),
                stage: "s".to_string(),
                iteration: 1,
                tool_calls: 0,
                accepts_messages: false,
            },
            WorldEvent::Tokens {
                run_id: rid.clone(),
                agent_id: aid.clone(),
                prompt_tokens: 1,
                completion_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            WorldEvent::Context {
                run_id: rid.clone(),
                agent_id: aid.clone(),
                total_tokens: 3,
                max_tokens: 4,
            },
            WorldEvent::Interaction {
                run_id: rid.clone(),
                agent_id: aid.clone(),
                request: InteractionRequest::free_text("i", "p", "s", true),
            },
            WorldEvent::Completed {
                run_id: rid.clone(),
                agent_id: aid.clone(),
                status: "complete".to_string(),
            },
            WorldEvent::StageTransition {
                run_id: rid.clone(),
                agent_id: aid.clone(),
                from: "a".to_string(),
                to: "b".to_string(),
                iteration: 1,
            },
            WorldEvent::ToolCallStarted {
                run_id: rid.clone(),
                agent_id: aid.clone(),
                call_id: "c".to_string(),
                tool: "t".to_string(),
            },
            WorldEvent::ToolCallFinished {
                run_id: rid.clone(),
                agent_id: aid.clone(),
                call_id: "c".to_string(),
                tool: "t".to_string(),
                ok: true,
                summary: "s".to_string(),
            },
            WorldEvent::Log {
                run_id: rid.clone(),
                agent_id: aid.clone(),
                line: "l".to_string(),
            },
        ];
        for ev in events {
            assert_eq!(ev.run_id(), "run-x");
        }
    }
}
