//! The wire and callback types the host speaks: what a caller can ask for, and
//! what it gets back.
//!
//! Split out of the host itself because these are the crate's public
//! vocabulary. `ControlOp` is what every client sends and `RunListEntry` is what
//! `lev ps` renders, while the host is the thing that happens to interpret them.
//! They are re-exported from the parent, so every existing `host::ControlOp`
//! path is unchanged.

use bevy_ecs::entity::Entity;

use crate::spec::env::Caller;
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::RunId;
use crate::spec::request::SpawnRequest;
use crate::spec::run_spec::RunSpec;
use crate::spec::summary::SpawnSummary;
use crate::world::AgentId;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::components::{AgentStatus, WaitReason};
use crate::world::PipelineWorld;
use leviath_core::interaction::{InteractionRequest, InteractionResponse};

/// One row of a run listing (`ControlRequest::List`): a live run, its status,
/// and enough context to judge whether that status is a problem.
///
/// The request type is named rather than linked because it lives behind the
/// `control-socket` feature while this type does not, and an intra-doc link
/// into a module that may not be compiled fails the docs build.
///
/// A run id and a status word are not enough: `waiting` on its own says nothing
/// about whether a person is needed, and gives no way to tell a run that moved a
/// second ago from one stopped for an hour. Everything here is read straight off
/// the live world, so it is the daemon's own view, not a re-read of `meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunListEntry {
    /// The run id (`lev ps`'s first column, and what `lev kill` takes).
    pub run_id: String,
    /// The generated one-line title, once there is one.
    ///
    /// Absent from this listing until now, which meant `lev ps` could only ever
    /// print a run id and an agent name while the dashboard - reading the same
    /// runs off disk - had a title for them.
    #[serde(default)]
    pub title: Option<String>,
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
    /// Unix seconds when this run was launched, for its age.
    ///
    /// The listing carried only `last_progress_at`, so `lev ps` could report how
    /// long since a run moved and had no way to say how long it had existed - it
    /// printed the first under the heading of the second. Defaulted for an older
    /// daemon that omits it, which then reads as an unknown age rather than as a
    /// run launched at the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// The run's working stopwatch: how much of its age it spent actually
    /// working. See [`ActiveClock`](leviath_core::run_meta::ActiveClock).
    ///
    /// `None` from a daemon older than the clock, which reads as an unknown
    /// working time rather than as zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<leviath_core::run_meta::ActiveClock>,
    /// Whether this run is unattended (`--yolo`). An unattended run should never
    /// be sitting on a prompt; if it is, something dropped the flag.
    #[serde(default)]
    pub unattended: bool,
    /// The yolo profile the run was launched under, when it named one.
    /// Omitted by a daemon older than profiles, and for the bare flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo_profile: Option<String>,
    /// Whether this run finished having modified nothing, when its blueprint
    /// gave it a way to. Only ever true for a run that has stopped.
    ///
    /// Carried on the row rather than left in `meta.json`, which is read back
    /// only on restart: a run that finished with no work to show for it
    /// otherwise looks exactly like one that succeeded. Defaulted for the same
    /// reason as `unattended` - an older daemon simply omits it.
    #[serde(default)]
    pub empty_output: bool,
    /// Fan-outs this run degraded: a `fan_out` stage that transitioned without
    /// ever starting any workers. Defaulted like `empty_output`, for an older
    /// daemon that omits it.
    #[serde(default)]
    pub splits_degraded: usize,
    /// Rhai scripts this run needed and could not use, by name.
    ///
    /// See [`leviath_core::run_meta`] for why these are worth surfacing: the
    /// runtime skips a broken script rather than failing the run, which is right
    /// and was also silent. Defaulted for an older daemon that omits it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub broken_scripts: Vec<String>,
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
/// getting anywhere at all". They are different questions, and a wedge is only
/// visible in the second: every individual run looks fine while the daemon as a
/// whole has not moved in hours.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DaemonHealth {
    /// Loaded agents by status.
    pub agents: crate::world::AgentCounts,
    /// Inference-pool occupancy, one entry per model actually used.
    pub inference: Vec<crate::inference_pool::PoolOccupancy>,
    /// Provider-pool occupancy, one entry per provider with a configured cap
    /// that has been used. Empty unless `[limits]` caps a provider, which is
    /// why it is `default`ed rather than required of an older client's payload.
    #[serde(default)]
    pub inference_providers: Vec<crate::inference_pool::ProviderPoolOccupancy>,
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
    /// parses a newer daemon's response.
    #[serde(default)]
    pub providers_down: Vec<crate::pipeline::ProviderCircuitState>,
    /// What the persistence lane has written, and what it has lost.
    ///
    /// A daemon whose journal is refusing writes answers every request and looks
    /// well, so this is the only place the state shows. `#[serde(default)]` for
    /// the same reason as the field above: an older daemon's reply has no such
    /// object, and reads as a lane that has done nothing.
    #[serde(default)]
    pub journal: crate::persist_stats::JournalHealth,
}

/// What a host asks of whoever starts its runs: the daemon, or an embedder.
///
/// The host owns the world and nothing else. Resolving a request against the
/// machine, writing the run's file and binding its live handles all happen
/// here, off the world, on a task of their own; the host only places what
/// comes back, with [`crate::insert::insert`], which cannot fail.
#[async_trait::async_trait]
pub trait RunStarter: Send + Sync {
    /// Resolve `request` for `caller`, record it, and bind it.
    async fn start(
        &self,
        request: SpawnRequest,
        caller: Caller,
    ) -> Result<PreparedRun, SpawnIssues>;

    /// Resolve `request` for `caller` as a dry run: nothing is written and no
    /// seed with side effects runs.
    async fn check(
        &self,
        request: SpawnRequest,
        caller: Caller,
    ) -> Result<SpawnSummary, SpawnIssues>;

    /// Bring the world up to date for a run about to be placed in it: the
    /// settings that live on the world rather than on the run (the provider
    /// set, the taint policy, the limits). Runs on the world, just before the
    /// insert. Nothing to do by default.
    fn before_insert(&self, _world: &mut PipelineWorld, _spec: &RunSpec) {}
}

/// A run resolved, recorded and bound, ready to place.
pub struct PreparedRun {
    /// Its spec.
    pub spec: std::sync::Arc<RunSpec>,
    /// The live handles binding built for it.
    pub bindings: crate::spec::env::Bindings,
    /// The state it starts from.
    pub state: crate::state::RunState,
}

impl std::fmt::Debug for PreparedRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedRun")
            .field("run_id", &self.spec.run_id)
            .field("bindings", &self.bindings)
            .field("seq", &self.state.seq)
            .finish()
    }
}

/// A refusal that is about the host rather than the request, said the way
/// every spawn refusal is said.
pub(crate) fn host_refusal(message: impl Into<String>) -> SpawnIssues {
    SpawnIssue::new(SpecPath::root(), IssueCode::Unavailable, message).into()
}

/// The daemon-installed function that pages a previously-unloaded run back into
/// the world from its on-disk state: given a run id, it reads the run's file,
/// binds its spec and places it, and returns the new entity, or `None` if there
/// is no such resumable run on disk. Used for reload-on-demand - a
/// control/sub-agent op targeting a run that isn't currently in memory pages it
/// in first via the host's internal resolve-or-reload step. Installed with
/// [`super::WorldHost::set_reloader`].
pub(crate) type Reloader = Box<dyn FnMut(&mut PipelineWorld, &str) -> Option<AgentId> + Send>;

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
pub(crate) type ForceTerminator = Box<dyn FnMut(&str) -> bool + Send>;

/// The daemon-installed hook run just before a terminal agent's entity is
/// despawned (reaped). It receives the world and the entity while both are still
/// valid, so the daemon can release per-agent resources the runtime doesn't know
/// about - tearing down the agent's sandbox and dropping its tool state.
/// Installed with [`super::WorldHost::set_reaper`]; a no-op when none is set.
pub type Reaper = Box<dyn FnMut(&mut PipelineWorld, Entity) + Send>;

/// The daemon-installed hook run when a run starts moving again: a `Resume`
/// control op, an answered approval prompt, or a run paged back in from disk.
/// It receives the world and the agent's entity.
///
/// The runtime has no opinion about what resuming means beyond un-pausing. The
/// daemon's is that a run re-reads the parts of `config.toml` a person edits to
/// unblock it - tool permissions, safe commands, read-path grants, write
/// ceilings - so a run stuck on one of them can be freed without cancelling it
/// and starting again. A stage that is already running keeps the snapshot it
/// started on; this fires only at the boundary. Installed with
/// [`super::WorldHost::set_resumer`]; a no-op when none is set.
pub type Resumer = Box<dyn FnMut(&mut PipelineWorld, Entity) + Send>;

/// The daemon-installed hook run on every safety re-drive, whether or not
/// anything woke the host: the place for work that has to happen while the
/// daemon is otherwise idle. The daemon uses it to notice an edited
/// `config.toml` or `mime_types.toml` and re-apply the settings that reach
/// runs already under way, so an edit lands within one re-drive interval
/// rather than at the next spawn. Installed with
/// [`super::WorldHost::set_housekeeper`]; a no-op when none is set.
pub type Housekeeper = Box<dyn FnMut(&mut PipelineWorld) + Send>;

/// A world-access request from an agent's tool lane. The sub-agent tools
/// (`spawn_agent`/`check_agent`/`send_to_agent`/`kill_agent`) need the world and
/// the [`RunStarter`], which only the host holds - the tool lane runs async, off
/// the world. Each carries a oneshot reply, so the (sequential) tool lane blocks on
/// the host applying it, mirroring the interaction hub.
pub enum SubAgentOp {
    /// Start a child run of `parent_run_id` from `request`. The child is
    /// resolved as a [`Caller::Child`] of the parent, so it is never trusted
    /// with more than the parent is, and refused when the parent has no depth
    /// left. Reply is the child's run id.
    Spawn {
        /// What the child runs. Boxed because it is much larger than the other
        /// variants' payloads.
        request: Box<SpawnRequest>,
        /// The run id of the agent doing the spawning.
        parent_run_id: String,
        /// Reply: the child's run id, or every reason it was refused.
        reply: oneshot::Sender<Result<RunId, SpawnIssues>>,
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
/// Both halves, not the status alone. `wait_for_agent`'s schema promises to
/// return the child's final result, and a status word leaves a parent no way to
/// receive a child's work except by agreeing on a file path out of band.
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
    /// Start a top-level run. Reply is its run id, or every reason it was
    /// refused.
    Spawn {
        /// What to run. Boxed because it is much larger than the other
        /// variants' payloads.
        request: Box<SpawnRequest>,
        /// Reply channel.
        reply: oneshot::Sender<Result<RunId, SpawnIssues>>,
    },
    /// Resolve a top-level run without starting it. Reply is a summary of the
    /// run it would be, or every reason it would be refused.
    ValidateSpawn {
        /// What would run.
        request: Box<SpawnRequest>,
        /// Reply channel.
        reply: oneshot::Sender<Result<SpawnSummary, SpawnIssues>>,
    },
    /// A run's state: read live from the world when the run is there, else
    /// the last state its run file holds. `None` for a run that is neither.
    Inspect {
        /// The run to read.
        run_id: String,
        /// Reply channel.
        reply: oneshot::Sender<Option<Box<crate::state::RunState>>>,
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
    /// Read one stored part of a run by its sha256: the bytes behind an
    /// artifact [`Result`](Self::Result) named. Reply is `None` when the
    /// run's store holds no such blob.
    Blob {
        /// The run whose store holds the bytes.
        run_id: String,
        /// The store's key for them.
        sha256: String,
        /// Reply channel.
        reply: oneshot::Sender<Option<Vec<u8>>>,
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
    /// Deliver a message to a running agent (by agent id). Reply is `Ok(false)`
    /// if the world's message channel is closed, and `Err` with the reason for a
    /// message that says nothing.
    Message {
        /// Target agent id.
        agent_id: String,
        /// Message body.
        content: String,
        /// Optional target region (defaults to the conversation region).
        target_region: Option<String>,
        /// Files sent with the message, written beside it as stored parts.
        parts: Vec<leviath_core::mime::InboundPart>,
        /// Reply channel.
        reply: oneshot::Sender<Result<bool, String>>,
    },
    /// List every open interaction awaiting an answer, as `(agent_id, request)`.
    ListInteractions {
        /// Reply channel.
        reply: oneshot::Sender<Vec<(String, InteractionRequest)>>,
    },
    /// Answer an open interaction. Reply is `Ok(false)` if no such request is
    /// open, and `Err` with the reason for an answer the request cannot take.
    AnswerInteraction {
        /// The answer (its `request_id` selects the interaction).
        response: InteractionResponse,
        /// Reply channel.
        reply: oneshot::Sender<Result<bool, String>>,
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
