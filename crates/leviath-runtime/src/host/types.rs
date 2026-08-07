//! The wire and callback types the host speaks: what a caller can ask for, and
//! what it gets back.
//!
//! Split out of the host itself because these are the crate's public
//! vocabulary. `ControlOp` is what every client sends and `RunListEntry` is what
//! `lev ps` renders, while the host is the thing that happens to interpret them.
//! They are re-exported from the parent, so every existing `host::ControlOp`
//! path is unchanged.

use std::collections::HashMap;

use bevy_ecs::entity::Entity;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::components::{AgentStatus, WaitReason};
use crate::world::PipelineWorld;
use leviath_core::interaction::{InteractionRequest, InteractionResponse};

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
/// [`super::WorldHost::set_reloader`].
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
/// writer. Installed with [`super::WorldHost::set_force_terminator`]; without one, a
/// cancel that misses in the world simply misses (the prior behavior).
pub type ForceTerminator = Box<dyn FnMut(&str) -> bool + Send>;

/// The daemon-installed hook run just before a terminal agent's entity is
/// despawned (reaped). It receives the world and the entity while both are still
/// valid, so the daemon can release per-agent resources the runtime doesn't know
/// about - tearing down the agent's sandbox and dropping its tool state.
/// Installed with [`super::WorldHost::set_reaper`]; a no-op when none is set.
pub type Reaper = Box<dyn FnMut(&mut PipelineWorld, Entity) + Send>;

/// An async hook the host awaits *before* servicing a top-level `Spawn` control
/// op, so the daemon can do async preparation the sync spawner can't - e.g.
/// lazily connecting the blueprint's MCP servers into the shared pool so
/// they're warm by the time [`Spawner`] reads them. The returned future is
/// `'static` (it must clone anything it needs from the `SpawnArgs`). Installed
/// with [`super::WorldHost::set_spawn_preprocessor`]; when none is set, spawns proceed
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
