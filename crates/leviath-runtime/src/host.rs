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

use std::collections::{HashMap, HashSet, VecDeque};
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
    ///
    /// An interaction point may opt out with `unattended = "ask"`, and then it
    /// parks anyway: the point exists precisely because auto-approving it is
    /// the wrong answer. Such a run waits for `[limits]
    /// interaction_timeout_secs` (default 3600) rather than forever, but from
    /// the outside an hour of `Waiting` is indistinguishable from a hang.
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
    /// The shape this caller wants the run's final output in, overriding what
    /// the blueprint declares.
    ///
    /// A request, not a contract: it changes what the model is asked to produce
    /// and what gets recorded, and nothing converts between shapes. Naming a
    /// format without also supplying a schema drops the blueprint's declared
    /// schema, since a check written for one shape says nothing about another
    /// (see [`leviath_core::resolve_output_spec`]).
    #[serde(default)]
    pub output: Option<leviath_core::output::OutputSpec>,
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
    /// Whether this run finished having modified nothing, when its blueprint
    /// gave it a way to. Only ever true for a run that has stopped.
    ///
    /// The flag itself is as old as issue #107, but nothing ever showed it: it
    /// went into `meta.json` and was read back only on restart, so a run that
    /// finished with no work to show for it looked exactly like one that
    /// succeeded. Defaulted for the same reason as `unattended` - an older
    /// daemon simply omits it.
    #[serde(default)]
    pub empty_output: bool,
    /// How much of this run's `[read_paths]` its config granted at spawn.
    /// `None` for a blueprint that declares none, which is nearly every agent.
    ///
    /// Worth a column of its own because an ungranted declaration is inert: the
    /// run is up, looks healthy, and will be refused the reads its author
    /// designed it around.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_paths: Option<leviath_core::run_meta::ReadPathGrantCounts>,
    /// Whether the run has submitted a final output.
    ///
    /// The flag only, never the content: this row is sent over the control
    /// socket on every `lev ps`, and an answer may be a quarter of a megabyte.
    /// `lev result <run-id>` fetches it.
    #[serde(default)]
    pub has_final_output: bool,
}

/// Everything one [`ControlOp::List`] answers with: the live runs, the runs that
/// finished recently enough to still be worth reporting, and the daemon's health.
///
/// A named struct rather than a tuple because the reply has now grown twice, and
/// each time every caller had to be re-read positionally to find out which half
/// was which.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunListing {
    /// One entry per run the daemon is hosting.
    pub runs: Vec<RunListEntry>,
    /// Runs the daemon has unloaded within its retention window, oldest first.
    /// Kept apart from `runs` so a caller asking "what is running" still gets
    /// only that.
    pub finished: Vec<RunListEntry>,
    /// How the daemon itself is doing.
    pub health: DaemonHealth,
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
    /// Providers currently out of service, and when each is probed again.
    ///
    /// Empty on a healthy daemon. `#[serde(default)]` so an older client still
    /// parses a newer daemon's response (issue #201).
    #[serde(default)]
    pub providers_down: Vec<crate::pipeline::ProviderCircuitState>,
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
    /// Report a run's current status and answer (`None` if the host has no such
    /// live run).
    Check {
        /// The run to query.
        run_id: String,
        /// Reply: what the run is doing and what it has handed back.
        reply: oneshot::Sender<Option<SubAgentReport>>,
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

/// What a parent learns when it checks on a child: what the child is doing, and
/// what it has handed back.
///
/// The two used to be one thing - the status alone - which is why
/// `wait_for_agent`, whose schema has always promised "return its final result",
/// returned `"Sub-agent 'x' finished with status: Complete"` and nothing else. A
/// parent had no way to receive a child's work except by agreeing on a file path
/// out of band.
#[derive(Debug, Clone, PartialEq)]
pub struct SubAgentReport {
    /// What the child is doing now.
    pub status: AgentStatus,
    /// What the child submitted, if anything. `None` for a child still working,
    /// one whose blueprint never asks for an output, or one that finished
    /// without giving it.
    pub final_output: Option<leviath_core::output::FinalOutput>,
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
    /// Report what a run handed back, if anything.
    ///
    /// The counterpart to [`Status`](Self::Status): that says whether a run is
    /// done, this says what it concluded.
    Result {
        /// The run to query.
        run_id: String,
        /// Reply: the submitted answer, or `None` for a run that gave none (or
        /// one the world no longer holds).
        reply: oneshot::Sender<Option<leviath_core::output::FinalOutput>>,
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
        reply: oneshot::Sender<RunListing>,
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
        /// What the run handed back, when it submitted anything.
        ///
        /// Carried on the event rather than left for the consumer to read off
        /// disk: this fires the moment the run goes terminal, and the persist
        /// tick that writes `meta.json` has not necessarily run yet. A webhook
        /// or websocket consumer reading the file would race it and report a
        /// finished run with no answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_output: Option<leviath_core::output::FinalOutput>,
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
    /// Extra tool-lane permits the relief valve has handed out and not yet
    /// reclaimed (see [`Self::decay_relief_if_healthy`]).
    relief_granted: usize,
    /// Consecutive re-drives that found the lane healthy (no dead cycles, no
    /// queue) while relief was outstanding - the decay countdown.
    healthy_cycles: u32,
    /// Dead cycles the daemon tolerates before widening the tool lane. `0`
    /// disables relief. See [`Self::set_dead_cycles_before_relief`].
    dead_cycles_before_relief: u32,
    /// Runs unloaded recently enough to still be worth reporting, oldest first,
    /// each paired with the unix second it was unloaded. See
    /// [`Self::record_finished`].
    finished: VecDeque<(i64, RunListEntry)>,
    /// How long an unloaded run stays in [`Self::finished`]. `0` keeps none.
    /// See [`Self::set_finished_retention_secs`].
    finished_retention_secs: u64,
    /// Paused runs the host has paged out of the world, by run id, each holding
    /// its last listing row. A parked run's full state is on disk; `Resume`,
    /// `Message` and `Cancel` all page it back through
    /// [`Self::resolve_or_reload`], and [`Self::list`] keeps reporting it so an
    /// operator's `lev ps` view does not change just because the daemon stopped
    /// spending memory on a run nobody is driving.
    parked: HashMap<String, RunListEntry>,
}

/// Consecutive healthy re-drives (no dead cycles, empty tool queue) before the
/// relief-decay valve reclaims one granted permit. Each re-drive is seconds
/// apart, so four of them is a comfortably-over margin - and each further
/// healthy cycle reclaims one more, so a full lane's worth drains in minutes.
const HEALTHY_CYCLES_BEFORE_DECAY: u32 = 4;

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

/// How long a run stays in the listing after the daemon unloads it.
///
/// A terminal agent is unloaded a pass or two after it finishes, and until now
/// it vanished from the listing at that moment. A run that died on its first
/// inference was therefore indistinguishable from one that had never been
/// spawned, which is what left the scheduler in issue #205 with nothing to go on
/// but a stopwatch: it could not tell a dead spawn from a slow one, so it
/// reverted the work and spawned again, for forty minutes.
///
/// Five minutes covers several polls of any scheduler that checks in about once
/// a minute, so a single missed or slow poll does not lose the evidence. It is
/// also what the rest of the daemon already means by "long enough that a hiccup
/// cannot cause it": the dashboard calls a run stale at 300 seconds, and
/// [`DEFAULT_DEAD_CYCLES_BEFORE_RELIEF`] at the 30-second re-drive works out to
/// the same five minutes.
///
/// Served from `[limits] finished_retention_secs`; `0` keeps nothing and
/// restores the old behaviour.
pub const DEFAULT_FINISHED_RETENTION_SECS: u64 = 300;

/// How many unloaded runs [`WorldHost::finished`] holds before the oldest are
/// dropped, whatever the retention window says.
///
/// Not configurable: it is a memory bound, not a tuning knob. A factory that
/// finishes runs faster than this fills the window keeps the most recent ones,
/// which are the ones anyone is still asking about. Set the window shorter to
/// control how much the listing shows; this only stops it growing without end.
const MAX_RETAINED_FINISHED: usize = 256;

impl WorldHost {
    /// Wrap a world with a fresh interaction hub.
    pub fn new(world: PipelineWorld) -> Self {
        Self::with_interactions(world, InteractionHub::new())
    }

    /// Wrap a world with a specific interaction hub - the daemon shares one hub
    /// between the tool service's per-agent backends and this host.
    pub fn with_interactions(mut world: PipelineWorld, interactions: InteractionHub) -> Self {
        // 256, not more: a tokio broadcast ring never shrinks, so every slot a
        // busy period fills stays allocated (holding its event's strings) for
        // the daemon's life. Consumers here are live relays, not replayers -
        // one that falls a full ring behind gets a Lagged skip either way.
        let (events, _) = broadcast::channel(256);
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
            parked: HashMap::new(),
            subagent_tx,
            subagent_rx,
            redrive: DEFAULT_REDRIVE_INTERVAL,
            dead_cycles: 0,
            last_progress: None,
            relief_granted: 0,
            healthy_cycles: 0,
            dead_cycles_before_relief: DEFAULT_DEAD_CYCLES_BEFORE_RELIEF,
            finished: VecDeque::new(),
            finished_retention_secs: DEFAULT_FINISHED_RETENTION_SECS,
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
        self.decay_relief_if_healthy(&snapshot);
        self.observe_lanes(&snapshot, relief);
    }

    /// The relief valve's give-back half: once the lane has been demonstrably
    /// healthy for [`HEALTHY_CYCLES_BEFORE_DECAY`] consecutive re-drives,
    /// reclaim one granted permit per further healthy cycle until the lane is
    /// back at its configured width.
    ///
    /// The guards are what keep this on the safe side of the wedge detection
    /// that granted the relief in the first place (issue #191): nothing is
    /// reclaimed while `dead_cycles` is non-zero (a wedge may be forming),
    /// nothing is reclaimed while the extra capacity is in use (`narrow` only
    /// takes *idle* permits), and the width can never drop below what the
    /// config asked for, because only permits this valve granted are counted.
    fn decay_relief_if_healthy(&mut self, snapshot: &LaneSnapshot) {
        if self.relief_granted == 0 {
            self.healthy_cycles = 0;
            return;
        }
        let healthy = self.dead_cycles == 0 && snapshot.tools_queued == 0;
        self.healthy_cycles = match healthy {
            true => self.healthy_cycles.saturating_add(1),
            false => 0,
        };
        if self.healthy_cycles < HEALTHY_CYCLES_BEFORE_DECAY {
            return;
        }
        let narrowed = self.world.narrow_tool_lane(1);
        if narrowed > 0 {
            self.relief_granted -= narrowed;
            tracing::info!(
                narrowed,
                relief_granted = self.relief_granted,
                "the jam is over; reclaiming relief capacity from the tool lane"
            );
        }
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

    /// How long a run stays in the listing after the daemon unloads it. `0`
    /// keeps none, which is how the listing behaved before issue #205. Served
    /// from `[limits] finished_retention_secs`.
    pub fn set_finished_retention_secs(&mut self, secs: u64) {
        self.finished_retention_secs = secs;
    }

    /// Keep `entry` in the listing as a run that finished at `at`.
    ///
    /// One row per run: an id already held is replaced rather than duplicated,
    /// so however often a run is unloaded it is reported once.
    ///
    /// `last_progress_at` is filled in from `at` when the run never persisted a
    /// snapshot. That is not a guess. A run that died on its first inference has
    /// no watermark to read, and the listing would show its age as `-` - which
    /// is the one thing an operator or a scheduler most wants to know about a
    /// run that failed instantly. For a run being unloaded, the unload is the
    /// last thing that happened to it.
    fn record_finished(&mut self, mut entry: RunListEntry, at: i64) {
        if self.finished_retention_secs == 0 {
            return;
        }
        entry.last_progress_at.get_or_insert(at);
        self.finished
            .retain(|(_, held)| held.run_id != entry.run_id);
        self.finished.push_back((at, entry));
        while self.finished.len() > MAX_RETAINED_FINISHED {
            self.finished.pop_front();
        }
    }

    /// Drop unloaded runs that have outlived the retention window.
    ///
    /// `now` is passed in rather than read here so a test can age the buffer
    /// without sleeping through the window, the same reason `lev ps`'s
    /// `format_runs` takes it. Called once per [`Self::emit_events`], which the
    /// serve loop runs before it handles any control op, so a listing never has
    /// to prune on the way out.
    fn prune_finished(&mut self, now: i64) {
        let window = self.finished_retention_secs as i64;
        while let Some(&(at, _)) = self.finished.front() {
            if now.saturating_sub(at) <= window {
                break;
            }
            self.finished.pop_front();
        }
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
        // Sampled on the same tick, and unconditionally: a collector needs the
        // empty sample to see that a provider came *back*, not just that it
        // went away (issue #201).
        let down: Vec<leviath_core::telemetry::ProviderHealth> = self
            .world
            .open_circuits()
            .into_iter()
            .map(|c| leviath_core::telemetry::ProviderHealth {
                provider: c.provider,
                reason: c.reason.label().to_string(),
                consecutive_failures: c.consecutive_failures,
                retry_in_secs: c.retry_in_secs,
            })
            .collect();
        self.world
            .world()
            .resource::<crate::telemetry::Telemetry>()
            .0
            .observe_providers(&down);
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
            providers_down: self.world.open_circuits(),
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
        // The listing row travels with each one: it is built here, while the
        // entity is untouched, rather than in the reap loop below, where the
        // daemon's reap hook has already had the world and is free to have taken
        // the components it reads.
        let mut to_reap: Vec<(String, Entity, RunListEntry)> = Vec::new();
        let mut to_park: Vec<(String, Entity, RunListEntry)> = Vec::new();
        let now = chrono::Utc::now().timestamp();
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
                    // Read off the live entity, not off disk: this fires the
                    // moment the run goes terminal, and the persist tick that
                    // writes `meta.json` has not necessarily run yet.
                    final_output: self
                        .world
                        .world()
                        .get::<crate::persistence::FinalOutput>(entity)
                        .map(|o| o.0.clone()),
                });
            }
            // Unload a terminal agent once its terminal state has been emitted (a
            // prior pass already saw it terminal, so the event went out and the
            // persistence lane captured it) and no live parent still needs it.
            if cur.terminal && was_terminal && self.no_live_parent(entity) {
                let entry = self.entry_for(&run_id, entity, state);
                to_reap.push((run_id.clone(), entity, entry));
            }
            // Page a paused run out of the world once its paused state is on
            // its way to disk. Unlike `Waiting` (see the NOTE below), `Paused`
            // carries no live continuation - it is the one non-terminal state
            // whose whole meaning is "nothing is driving this" - and Resume,
            // Message and Cancel all page an unloaded run back in through
            // `resolve_or_reload`, exactly as a daemon restart would. Scoped
            // to standalone roots: a run with tree links or an open prompt
            // keeps the restart-equivalence question open and stays resident.
            if self.parkable(entity, &state.status) {
                let entry = self.entry_for(&run_id, entity, state);
                to_park.push((run_id.clone(), entity, entry));
            }
            // NOTE: non-terminal `Waiting` agents are intentionally NOT unloaded.
            // Every `Waiting` state carries a live, unpersisted continuation - a
            // blocked `ask` future (`AwaitingInteraction`), running fan-out workers
            // (`FanOutWaiting`), or pending children (`WaitingForChildren`) - so
            // flushing one to disk and paging it back cannot resume it (in-flight
            // interactions aren't persisted; the blocked future is gone). Only
            // terminal agents (fully on disk) are reaped, and paused ones parked.

            self.emitted.insert(run_id, cur);
        }

        // Reap: run the daemon's reap hook (sandbox teardown + tool-state drop)
        // while the entity is still valid, then despawn it and erase its host-map
        // entries. Iterating a snapshot of `by_run_id` above means removing here
        // is safe. The reaper is moved out for the loop to avoid borrowing `self`
        // twice, then restored.
        let mut reaper = self.reaper.take();
        let reaped_any = !to_reap.is_empty();
        for (run_id, entity, entry) in to_reap {
            if let Some(reaper) = reaper.as_mut() {
                reaper(&mut self.world, entity);
            }
            self.world.world_mut().despawn(entity);
            self.by_run_id.remove(&run_id);
            self.emitted.remove(&run_id);
            // The run leaves memory but not the listing: for a while yet it can
            // still say how it ended, which is the whole of issue #205.
            self.record_finished(entry, now);
        }
        // Park paused runs: same teardown as a reap (the reap hook drops the
        // agent's tool state and sandbox, which a page-in rebuilds the way a
        // daemon restart does), but the listing row moves to `parked` rather
        // than `finished` - the run is not over, it is just not resident.
        for (run_id, entity, entry) in to_park {
            if let Some(reaper) = reaper.as_mut() {
                reaper(&mut self.world, entity);
            }
            self.world.world_mut().despawn(entity);
            self.by_run_id.remove(&run_id);
            self.emitted.remove(&run_id);
            self.parked.insert(run_id, entry);
        }
        self.reaper = reaper;
        self.prune_finished(now);
        // Reaped runs answer no further prompts: drop their request ids from
        // the emitted-interaction set, which otherwise grows for the daemon's
        // life (the set is keyed by request id, so prune by what is still
        // pending - the same shape `cancel_tree` uses).
        if reaped_any {
            let still_open: std::collections::HashSet<String> = self
                .interactions
                .pending()
                .into_iter()
                .map(|(_, req)| req.id)
                .collect();
            self.emitted_interactions
                .retain(|id| still_open.contains(id));
        }

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

    /// Whether a paused agent is safe to page out of the world.
    ///
    /// Conservative on purpose - this is the restart-equivalence question, and
    /// only shapes where the answer is a settled "yes" qualify:
    /// - status is `Paused`, and the *persisted* status is too (the watermark
    ///   proves the paused snapshot was dispatched, so disk can rebuild it);
    /// - it is a standalone root: no parent that might address it by entity,
    ///   no children whose links a page-in would have to rebuild;
    /// - no open interaction and no fan-out in flight (a pause that landed
    ///   mid-prompt or mid-split keeps its live machinery).
    fn parkable(&self, entity: Entity, status: &AgentStatus) -> bool {
        if !matches!(status, AgentStatus::Paused) {
            return false;
        }
        // No reloader, no parking: a host that cannot page a run back in
        // (an embedded world, a bare test host) must keep it resident, or
        // "paused" silently becomes "gone".
        if self.reloader.is_none() {
            return false;
        }
        let world = self.world.world();
        let paused_persisted = world
            .get::<crate::pipeline::PersistWatermark>(entity)
            .and_then(|w| w.persisted_status())
            == Some(leviath_core::run_meta::RunStatus::Paused);
        paused_persisted
            && world.get::<crate::components::ParentRef>(entity).is_none()
            && world.get::<SubAgentChildren>(entity).is_none()
            && world.get::<crate::fanout::FanOutWaiting>(entity).is_none()
            && world
                .get::<crate::interaction_points::AwaitingInteractionPoint>(entity)
                .is_none()
            && world.get::<AwaitingInteraction>(entity).is_none()
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
        // Live again: its listing row comes off the entity, not the parked map.
        self.parked.remove(run_id);
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
        let run_id = run_id.into();
        self.parked.remove(&run_id);
        self.by_run_id.insert(run_id, entity);
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
                let report = self.live_entity(&run_id).and_then(|e| {
                    self.world.agent_status(e).map(|status| SubAgentReport {
                        status,
                        final_output: self
                            .world
                            .world()
                            .get::<crate::persistence::FinalOutput>(e)
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

    /// One listing row for a run, read off the live world.
    ///
    /// Shared by [`Self::list`] and by the unload path in [`Self::emit_events`],
    /// so a run's last row is built exactly the way every row before it was.
    /// Takes the state rather than looking it up because the unload path already
    /// holds one, and a `None` it could never return would be a branch nothing
    /// can reach.
    fn entry_for(&self, run_id: &str, entity: Entity, state: &AgentState) -> RunListEntry {
        let world = self.world.world();
        let metadata = world.get::<RunMetadata>(entity);
        let has_output = world
            .get::<crate::persistence::FinalOutput>(entity)
            .is_some();
        RunListEntry {
            run_id: run_id.to_string(),
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
            empty_output: world
                .get::<crate::persistence::RunOutcomeFlags>(entity)
                .is_some_and(|f| {
                    // `produced_output` lives on the component only after a
                    // persist tick fills it, so it is answered from the live
                    // entity here. Without this, a researcher that submitted a
                    // perfectly good answer still read `complete (no output)`
                    // in `lev ps` while `meta.json` said otherwise - the exact
                    // drift between the two surfaces that one shared
                    // `is_empty_output` exists to prevent.
                    let mut flags = f.0.clone();
                    flags.produced_output = has_output;
                    crate::persistence::is_empty_output(&state.status, &flags)
                }),
            read_paths: metadata.and_then(|m| m.read_paths),
            has_final_output: has_output,
        }
    }

    /// List every known live run with the context an operator needs to read its
    /// status: why it is waiting, where it is, and when it last moved.
    fn list(&self) -> Vec<RunListEntry> {
        let world = self.world.world();
        self.by_run_id
            .iter()
            .filter_map(|(run_id, &entity)| {
                let state = world.get::<AgentState>(entity)?;
                Some(self.entry_for(run_id, entity, state))
            })
            // Parked (paused, paged-out) runs are still the daemon's runs; an
            // operator must not lose sight of one just because it left memory.
            .chain(self.parked.values().cloned())
            .collect()
    }

    /// The runs unloaded recently enough to still be reported, oldest first.
    ///
    /// Kept apart from [`Self::list`] rather than folded into it because
    /// "running now" and "finished a moment ago" are different questions, and
    /// two callers already depend on the first one: `lev daemon status` counts
    /// the hosted agents, and the dashboard uses the listing to decide which
    /// runs the daemon still holds.
    fn finished(&self) -> Vec<RunListEntry> {
        self.finished
            .iter()
            .map(|(_, entry)| entry.clone())
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
            ControlOp::Result { run_id, reply } => {
                // Live entities only. An unloaded run's answer is on disk in
                // `meta.json`, which is what `lev result` reads; keeping a copy
                // of every finished run's answer in memory would defeat the
                // point of bounding the finished buffer.
                let output = self
                    .live_entity(&run_id)
                    .and_then(|e| self.world.world().get::<crate::persistence::FinalOutput>(e))
                    .map(|o| o.0.clone());
                let _ = reply.send(output);
            }
            ControlOp::Status { run_id, reply } => {
                // A run the daemon has unloaded still has an answer for a
                // while, so a caller that asks a moment too late learns how the
                // run ended instead of being told there is no such run.
                let status = self
                    .live_entity(&run_id)
                    .and_then(|e| self.world.agent_status(e))
                    .or_else(|| self.parked.get(&run_id).map(|e| e.status.clone()))
                    .or_else(|| {
                        self.finished
                            .iter()
                            .find(|(_, e)| e.run_id == run_id)
                            .map(|(_, e)| e.status.clone())
                    });
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
                let _ = reply.send(RunListing {
                    runs: self.list(),
                    finished: self.finished(),
                    health: self.health(),
                });
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
#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
