//! Plain, serializable run-state data types.
//!
//! These are pure data (`serde`-derived structs/enums plus trivial constructors)
//! with no filesystem or async dependencies, so they can be named by both
//! `leviath-cli` and the `leviath-runtime` engine. All on-disk IO for
//! these types (reading/writing `meta.json`, run directories, snapshots, etc.)
//! lives in `leviath_cli::runstate`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Current status of a background run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Accepted and being set up; no inference has been issued yet.
    Starting,
    /// Working: inferring, calling tools, or moving between stages.
    Running,
    /// Blocked on a person. A human-in-the-loop tool or an interaction point is
    /// waiting for an answer, and the run holds its concurrency slot until it
    /// gets one.
    WaitingInput,
    /// Finished, with nothing further to accept.
    Complete,
    /// All required stages done; agent still accepts optional follow-up input.
    /// Shown as "Complete" in the dashboard - no kill option, input still enabled.
    CompleteInteractive,
    /// Paused by the user; resumes on request and is restored paused after a
    /// daemon restart.
    Paused,
    /// Stopped by a failure. `RunMeta::error` carries what went wrong.
    Error,
    /// Stopped from outside, by `lev kill` or a shutting-down daemon. Distinct
    /// from [`Error`](Self::Error): nothing went wrong, someone decided.
    Cancelled,
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunStatus::Starting => write!(f, "Starting"),
            RunStatus::Running => write!(f, "Running"),
            RunStatus::WaitingInput => write!(f, "WaitingInput"),
            RunStatus::Complete => write!(f, "Complete"),
            RunStatus::CompleteInteractive => write!(f, "CompleteInteractive"),
            RunStatus::Paused => write!(f, "Paused"),
            RunStatus::Error => write!(f, "Error"),
            RunStatus::Cancelled => write!(f, "Cancelled"),
        }
    }
}

/// Why a run's status is [`RunStatus::WaitingInput`].
///
/// `WaitingInput` alone is several unrelated situations wearing one word, and
/// they call for opposite responses: a fan-out parent whose workers are
/// churning is healthy and needs nothing, while a run parked on a
/// tool-approval prompt is stopped dead until a person answers it. Issue #184
/// is what happens when the two are indistinguishable - an operator reading
/// `waiting` across a factory concluded it had stalled and started killing
/// healthy runs. Issue #431 is the same conflation reaching every client that
/// reads `meta.json`.
///
/// Derived on demand from markers the engine already sets, by
/// [`wait_reason_from`]; nothing tracks it separately, so it cannot fall out of
/// sync with the status it explains. It lives here rather than in the runtime
/// because it is both reported live over the control socket and written to
/// `meta.json`, and one vocabulary across those two is the whole point.
///
/// Deliberately not new [`RunStatus`] variants: the status is matched
/// exhaustively across the codebase and serialized two ways on the wire, so
/// splitting it would break every consumer to express something that is not a
/// new state. The run really is waiting; this says on what.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum WaitReason {
    /// Blocked on a tool-approval prompt. Needs a person (or `--yolo`).
    ToolApproval,

    /// Blocked on a question the agent itself asked (`ask_user_*`,
    /// `present_for_review`). Needs a person.
    UserPrompt,

    /// Blocked on a taint-gate clearance prompt. Needs a person.
    TaintGate,

    /// Blocked on a blueprint stage-boundary checkpoint. Needs a person.
    InteractionPoint,

    /// Parked while fan-out workers run. Healthy; resolves on its own.
    FanOutWorkers {
        /// Workers still to finish, counting both running and not-yet-started.
        outstanding: usize,
    },

    /// Parked while spawned sub-agents run (`requires_children`). Healthy;
    /// resolves on its own.
    Children {
        /// Children that have not reached a terminal status.
        outstanding: usize,
    },

    /// Parked because something on the machine has to change before this run
    /// can go on: a provider it needs is not configured, a key was rejected,
    /// an account is out of credits.
    ///
    /// These used to end the run. They are all deterministic and all outside
    /// the run's control, so ending it threw away everything it had done to
    /// punish a person for a typo in `config.toml`. The run holds its place
    /// instead, and `lev resume` picks it up once the machine is fixed.
    ///
    /// The distinction that matters is not "is there a fix" but "does the fix
    /// let *this* run continue": a broken blueprint is equally deterministic
    /// and equally fixable, and still cannot be resumed into, because the
    /// blueprint was read at spawn.
    NeedsSetup {
        /// Which kind of problem, so a client can offer the right thing to do
        /// rather than parse the sentence below.
        blocker: SetupBlocker,
        /// What to do about it, in a sentence, for whoever reads the run.
        remedy: String,
    },
}

/// What is stopping a [`WaitReason::NeedsSetup`] run, in a form a client can
/// branch on.
///
/// One variant per remedy, not per error: these are the cases whose *fixes*
/// differ. Topping up an account, adding a provider to `config.toml` and
/// replacing a rejected key are three different screens, and a console that
/// had only the sentence would be reduced to matching on its wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupBlocker {
    /// The stage names a provider this install has not configured.
    ProviderMissing,
    /// The account behind the provider is out of credits.
    CreditsExhausted,
    /// The key was rejected.
    AuthFailed,
    /// The key is valid but not allowed to use the model.
    Forbidden,
    /// Every candidate is out of service, for reasons that do not agree or are
    /// not known. The remedy names what was tried last.
    ProvidersUnavailable,
}

impl std::fmt::Display for SetupBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProviderMissing => f.write_str("provider"),
            Self::CreditsExhausted => f.write_str("credits"),
            Self::AuthFailed => f.write_str("key"),
            Self::Forbidden => f.write_str("access"),
            Self::ProvidersUnavailable => f.write_str("providers"),
        }
    }
}

/// What a parked run needs, gathered where the markers are visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupNeeded {
    /// Which kind of problem it is.
    pub blocker: SetupBlocker,
    /// What to do about it.
    pub remedy: String,
}

impl WaitReason {
    /// Whether clearing this needs a person. `false` means the run is parked on
    /// other work and will move on by itself.
    pub fn needs_a_person(&self) -> bool {
        !matches!(self, Self::FanOutWorkers { .. } | Self::Children { .. })
    }
}

impl std::fmt::Display for WaitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolApproval => f.write_str("tool approval"),
            Self::UserPrompt => f.write_str("user prompt"),
            Self::TaintGate => f.write_str("taint gate"),
            Self::InteractionPoint => f.write_str("checkpoint"),
            Self::FanOutWorkers { outstanding } => write!(f, "workers({outstanding})"),
            Self::Children { outstanding } => write!(f, "children({outstanding})"),
            // The remedy is a sentence; this is a table cell. The blocker is
            // the half that fits, and the half that says which screen to open.
            Self::NeedsSetup { blocker, .. } => write!(f, "needs {blocker}"),
        }
    }
}

/// The parking markers an agent carries, gathered by whoever can see them.
///
/// The live listing reads these straight off the world; the persistence system
/// reads them off its query. Both then hand them here, so the precedence below
/// is written once instead of once per surface - two copies of it would
/// disagree the first time either was edited.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WaitMarkers {
    /// A taint-gate clearance prompt is outstanding.
    pub gate_prompt: bool,
    /// A blueprint stage-boundary checkpoint is holding.
    pub interaction_point: bool,
    /// Fan-out workers still to finish, when this run is a fan-out parent.
    pub fan_out_outstanding: Option<usize>,
    /// Sub-agents still running, when this run is held for its children.
    pub children_outstanding: Option<usize>,
    /// The kind of hub request holding this run, when one is.
    pub interaction: Option<crate::interaction::InteractionKind>,
    /// Whether a hub request is holding it at all. Separate from the kind
    /// because the kind can be unknown while the block is real.
    pub awaiting_interaction: bool,
    /// The run is parked until the machine is fixed, and this is what it
    /// needs.
    pub needs_setup: Option<SetupNeeded>,
}

/// Why a parked run is parked, or `None` when it is not parked or nothing has
/// claimed it.
///
/// Order matters, and it is the specific claim first. A taint-gate block and a
/// stage checkpoint each open a hub request of their own, so both also look
/// like a generic prompt; asking the specific markers first is what keeps them
/// from all reporting as one.
pub fn wait_reason_from(parked: bool, markers: &WaitMarkers) -> Option<WaitReason> {
    if !parked {
        return None;
    }
    // First, because it outranks everything: a run whose provider is missing
    // is not going to be unblocked by answering a prompt.
    if let Some(need) = &markers.needs_setup {
        return Some(WaitReason::NeedsSetup {
            blocker: need.blocker,
            remedy: need.remedy.clone(),
        });
    }
    if markers.gate_prompt {
        return Some(WaitReason::TaintGate);
    }
    if markers.interaction_point {
        return Some(WaitReason::InteractionPoint);
    }
    if let Some(outstanding) = markers.fan_out_outstanding {
        return Some(WaitReason::FanOutWorkers { outstanding });
    }
    if let Some(outstanding) = markers.children_outstanding {
        return Some(WaitReason::Children { outstanding });
    }
    if markers.awaiting_interaction {
        return Some(match markers.interaction {
            Some(crate::interaction::InteractionKind::ToolApproval) => WaitReason::ToolApproval,
            _ => WaitReason::UserPrompt,
        });
    }
    None
}

/// Metadata for a single background agent run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunMeta {
    /// Identifies the run everywhere, and names its directory under
    /// `~/.leviath/runs/`. Assigned at spawn and never reused.
    pub run_id: String,
    /// The blueprint's `[agent] name`, not the file it was loaded from. Two runs
    /// of the same agent from different paths share this.
    pub agent_name: String,
    /// Absolute path to the agent manifest directory
    pub agent_path: String,
    /// The task text the run was started with, verbatim.
    pub task: String,
    /// The `provider/model` actually resolved for the entry stage, or `None`
    /// before resolution. Later stages may use a different one; this is not
    /// rewritten to follow them.
    pub model: Option<String>,
    /// Always 0. There is no worker process per run: the daemon hosts every run
    /// as an entity in one shared world, so no run has a pid of its own.
    ///
    /// Kept because it is written into every `meta.json` there has ever been,
    /// and served from `GET /api/agents`. Do not key liveness on it. `pid == 0`
    /// is true of a run that is working, a run that has finished, and a run
    /// nothing is driving, so a sweeper that reverts on it reverts everything.
    /// Ask the daemon (`lev ps`) whether it is still hosting the run, and read
    /// `status` and `last_progress_at` off disk for what became of it.
    #[serde(default)]
    pub pid: u32,
    /// Where the run stands. The durable counterpart to the ECS world's live
    /// `AgentStatus`, and the one that survives a daemon restart.
    pub status: RunStatus,
    /// Name of the stage the run is in, matching a key under `[stages]`.
    pub current_stage: String,
    /// Zero-based position of `current_stage` in the blueprint's stage list.
    /// Not a progress measure: stages can loop and revisit.
    pub stage_index: usize,
    /// How many stages the blueprint declares, so a reader can render
    /// `stage_index` as "3 of 7" without loading the manifest.
    pub num_stages: usize,
    /// Inference turns taken in the current stage, reset on entering a new one.
    /// Compared against the stage's `max_iterations`.
    pub iteration: usize,
    /// Cumulative input tokens billed across every inference this run has made,
    /// including retries.
    pub prompt_tokens: usize,
    /// Cumulative output tokens billed across every inference this run has made.
    pub completion_tokens: usize,
    /// Cumulative tokens read from provider cache.
    #[serde(default)]
    pub cached_tokens: usize,
    /// Cumulative tokens written to provider cache.
    #[serde(default)]
    pub cache_write_tokens: usize,
    /// Total number of tool calls made across all iterations.
    #[serde(default)]
    pub tool_calls: usize,
    /// Absolute path to the working directory for tool execution
    pub workdir: String,
    /// Unix timestamp (seconds)
    pub started_at: i64,
    /// Unix timestamp (seconds)
    pub updated_at: i64,
    /// Unix seconds when this run last actually moved: a new iteration, a new
    /// stage, or a change of status. `None` before the first snapshot lands, and
    /// on runs written by a daemon older than this field.
    ///
    /// Distinct from `updated_at`, which also advances on the 30-second
    /// persistence heartbeat and so stays fresh on a run that is wedged. A fresh
    /// `updated_at` is evidence the daemon is alive, and no evidence at all about
    /// the run. Anything that ages a run must read this instead. Note that a
    /// daemon restart resets it: a reloaded run really is re-driven from its
    /// saved context, so it really has just moved.
    #[serde(default)]
    pub last_progress_at: Option<i64>,
    /// What went wrong, set alongside [`RunStatus::Error`]. `None` on every
    /// other status.
    pub error: Option<String>,
    /// Short human-readable title generated from the task prompt (None until generated).
    #[serde(default)]
    pub title: Option<String>,
    /// Custom key-value pairs from the spawn request (API metadata).
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    /// Webhook URL to POST on agent completion/error.
    #[serde(default)]
    pub callback_url: Option<String>,
    /// Optional shared secret used to HMAC-SHA256 sign the webhook body
    /// (`X-Leviath-Signature` header) so the receiver can verify authenticity.
    ///
    /// Persisted, because the daemon must still be able to sign a webhook for a
    /// run it reloaded after a restart. **Never serve it** - strip it with
    /// [`RunMeta::redacted`] before any of this struct leaves the process. See
    /// that method for what went wrong.
    #[serde(default)]
    pub callback_secret: Option<String>,
    /// Links sub-agent runs to their parent run.
    #[serde(default)]
    pub parent_run_id: Option<String>,
    /// Run-ids of this agent's direct sub-agents (sub-agent-tool spawns and
    /// fan-out workers). Persisted so the daemon can rebuild the exact
    /// parent→children tree on restart rather than reload children as orphans.
    #[serde(default)]
    pub children: Vec<String>,
    /// This agent's depth in the sub-agent tree (0 for a top-level run).
    /// Persisted so a reloaded child enforces its remaining spawn-depth budget.
    #[serde(default)]
    pub depth: usize,
    /// The sub-agent depth cap this agent imposes on its own children
    /// (0 when it has none). Restores `SubAgentChildren::max_child_depth`.
    #[serde(default)]
    pub max_child_depth: usize,
    /// Why this run may have produced nothing useful - see [`RunFlags`].
    #[serde(default)]
    pub flags: RunFlags,
    /// Whether the run was launched unattended (`--yolo`), so a daemon restart
    /// resumes it the way it was started.
    ///
    /// This used to be dropped on reload, on the reasoning that forgetting a
    /// launch override can only prompt more, never less. In practice it meant a
    /// restart silently converted an unattended run into one parked on a prompt
    /// nobody was watching for - the operator's own consent, given at launch,
    /// discarded by an implementation detail they never saw. Runs written before
    /// this field existed default to attended, so nothing is escalated
    /// retroactively.
    #[serde(default)]
    pub yolo: bool,
    /// How much of the blueprint's `[read_paths]` the config granted, as
    /// resolved at spawn. `None` for a blueprint that declared none, and for
    /// runs written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_paths: Option<ReadPathGrantCounts>,
    /// What the agent handed back, if it submitted anything: everything about
    /// the answer except the bytes.
    ///
    /// This is the run's answer, as distinct from `error` (why it failed) and
    /// from the stage logs (what it did along the way). The content itself is
    /// in a sidecar file beside this one, because this file is parsed for every
    /// run on every listing and must stay small no matter how long an answer is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_output: Option<crate::output::FinalOutputDescriptor>,

    /// Why this run is parked, when it is. `None` on every other status, and
    /// on a run written before this field existed. Same vocabulary the live
    /// listing reports, so `lev ps` and a client reading this file describe a
    /// run the same way.
    ///
    /// Additive on purpose: `default` means a `meta.json` from an older build
    /// still loads, and `skip_serializing_if` means a run that is not parked
    /// writes exactly the file it wrote before, so an older build reading a
    /// newer run sees nothing new either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<WaitReason>,
    /// The output shape this run was launched asking for, when the caller
    /// overrode the blueprint's.
    ///
    /// Persisted for the same reason `yolo` is: a daemon restart rebuilds the
    /// run's spawn arguments from this file, and dropping the request would
    /// silently revert the run to the blueprint's shape partway through. The
    /// caller asked once and should not have to ask again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_request: Option<crate::output::OutputSpec>,
}

/// How many `[read_paths]` entries a run's blueprint declared, and how many of
/// them the user's config actually granted.
///
/// Declaring is not granting: an ungranted entry is inert, and the reads it was
/// meant to allow are refused. Recorded at spawn, because that is when the
/// policy the run enforces is fixed - editing the config afterwards changes
/// nothing for a run already in flight.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadPathGrantCounts {
    /// Entries the blueprint declares.
    pub declared: usize,
    /// Entries the config grants.
    pub granted: usize,
}

/// Post-hoc diagnosis of a run's productivity, persisted in `meta.json` so a
/// harness (or the dashboard) can tell an empty run from a successful one
/// without inspecting the workspace or parsing logs.
///
/// The motivating failure: 13/300 SWE-bench runs completed their whole stage
/// pipeline and produced no file changes at all. Nothing on disk said so, or
/// said why.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RunFlags {
    /// Paths passed to file-modifying tools that succeeded, in first-touch
    /// order. Capped at [`MAX_TRACKED_MODIFIED_FILES`]; `modified_file_count`
    /// keeps the true total.
    #[serde(default)]
    pub modified_files: Vec<String>,
    /// Total successful file-modifying tool calls across the run (uncapped).
    #[serde(default)]
    pub modified_file_count: usize,
    /// The run reached a terminal status having modified nothing, and its
    /// blueprint gave it a way to modify something. See [`Self::no_output_tools`].
    #[serde(default)]
    pub empty_output: bool,
    /// No stage of the blueprint advertised a file-modifying tool, so this run
    /// could never have produced the file changes `empty_output` looks for.
    ///
    /// Recorded because "modified no files" only diagnoses an agent that was
    /// supposed to modify files. A router that spawns sub-agents, or an agent
    /// whose answer is its text, would otherwise report itself empty on every
    /// successful run - which is what happened in issue #192. The framework has
    /// no basis to judge such a run, so it says nothing rather than accusing.
    ///
    /// This mirrors the escape the runtime's `gate_blocks` already applies per
    /// stage: a `require_modifications` gate on a stage that advertises no
    /// modifying tool is skipped, because it could never pass.
    ///
    /// Phrased negatively so the `false` that [`Default`] and `serde(default)`
    /// produce means "was capable" - the behavior every `meta.json` written
    /// before this field had.
    #[serde(default)]
    pub no_output_tools: bool,
    /// How many stages exhausted their `max_iterations`.
    #[serde(default)]
    pub max_iterations_hit: usize,
    /// How many transitions proceeded past an unsatisfied gate because the
    /// gate's re-run budget ran out.
    #[serde(default)]
    pub gates_forced: usize,
    /// Regions declared `required` that were still empty when the stage that
    /// owed them gave up and moved on, in the order they were abandoned.
    ///
    /// The mechanism re-runs the stage a bounded number of times and then
    /// proceeds with a log line, which nothing downstream reads: a run whose
    /// agent wrote its plan and a run where we asked twice and moved on both
    /// finished `complete`, with the second silently missing the artifact every
    /// later stage's prompt says to work from (#371). Names rather than a count
    /// because knowing *which* region was abandoned is what makes it
    /// actionable, and a run cannot abandon many.
    #[serde(default)]
    pub required_regions_abandoned: Vec<String>,
    /// The working directory disappeared mid-run.
    #[serde(default)]
    pub workspace_lost: bool,
    /// The run submitted a final output.
    ///
    /// Counts as having produced something, alongside file modifications.
    /// Without this an agent whose whole deliverable is its answer - a
    /// researcher, a reviewer, a router - reported itself empty on every
    /// successful run, which is the same mistake [`Self::no_output_tools`] was
    /// added to correct from the other direction.
    #[serde(default)]
    pub produced_output: bool,
    /// How many stages transitioned without the final output they required,
    /// because the re-run budget ran out.
    ///
    /// The counterpart to [`Self::gates_forced`]: the run finished, and this
    /// says the answer it hands back may be missing.
    #[serde(default)]
    pub output_forced: usize,
}

/// How many distinct modified paths [`RunFlags`] records before it stops
/// growing (the count keeps rising). Bounds `meta.json` for a long run.
pub const MAX_TRACKED_MODIFIED_FILES: usize = 200;

impl RunFlags {
    /// Record a successful modifying tool call on `path`.
    pub fn record_modification(&mut self, path: &str) {
        self.modified_file_count += 1;
        if self.modified_files.len() < MAX_TRACKED_MODIFIED_FILES
            && !self.modified_files.iter().any(|p| p == path)
        {
            self.modified_files.push(path.to_string());
        }
    }
}

impl RunMeta {
    /// This run's metadata with the webhook signing secret removed, for anything
    /// that leaves the process.
    ///
    /// `GET /api/agents`, `/api/agents/{id}` and `/api/agents/{id}/children` all
    /// serialized `RunMeta` whole, so any holder of the API token could read
    /// every run's `callback_secret` - the key that authenticates Leviath's
    /// webhooks to their receivers. Mirrors the `RedactedConfig` pattern the
    /// `/api/config` handler already uses correctly.
    ///
    /// Returns an owned copy rather than mutating in place so a caller cannot
    /// accidentally redact the record the daemon still needs for signing.
    #[must_use]
    pub fn redacted(&self) -> Self {
        Self {
            callback_secret: None,
            ..self.clone()
        }
    }

    /// A newly accepted run: [`RunStatus::Starting`], both timestamps now, every
    /// counter at zero and every optional field unset.
    ///
    /// Only the seven values a caller genuinely knows at spawn are parameters.
    /// Everything else is filled in by the daemon as the run proceeds, so taking
    /// them here would invite a caller to invent a stage or a token count.
    pub fn new(
        run_id: String,
        agent_name: String,
        agent_path: String,
        task: String,
        model: Option<String>,
        workdir: String,
        num_stages: usize,
    ) -> Self {
        let now = now_secs();
        Self {
            run_id,
            agent_name,
            agent_path,
            task,
            model,
            pid: 0,
            status: RunStatus::Starting,
            current_stage: String::new(),
            stage_index: 0,
            num_stages,
            iteration: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            tool_calls: 0,
            workdir,
            started_at: now,
            updated_at: now,
            last_progress_at: None,
            error: None,
            title: None,
            metadata: HashMap::new(),
            callback_url: None,
            callback_secret: None,
            parent_run_id: None,
            children: Vec::new(),
            depth: 0,
            max_child_depth: 0,
            final_output: None,
            waiting_on: None,
            output_request: None,
            flags: RunFlags::default(),
            yolo: false,
            read_paths: None,
        }
    }

    /// Stamp `updated_at` with the current time.
    ///
    /// Deliberately does **not** touch `last_progress_at`: the 30-second
    /// persistence heartbeat calls this, and a run that is wedged must not look
    /// like one that just moved. See [`RunMeta::last_progress_at`].
    pub fn touch(&mut self) {
        self.updated_at = now_secs();
    }
}

/// One content entry within a region, captured at snapshot time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegionEntrySnapshot {
    /// The entry's text, exactly as it sat in the live region.
    pub content: String,
    /// The entry's token cost as counted when it was added, carried through the
    /// snapshot so a reload does not have to re-tokenize to rebuild budgets.
    pub tokens: usize,
    /// The entry's role/kind, so a snapshot round-trips faithfully when the
    /// daemon reloads it on restart. Defaults to `Text` for older snapshots.
    #[serde(default)]
    pub kind: crate::region::EntryKind,
    /// Free-form structured data an entry writer attached, passed through
    /// untouched. Nothing in the engine interprets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Key for HashMap region entries (file paths, section names, etc.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// How sensitive this entry is.
    ///
    /// Persisted because taint was not, and a restore that dropped it silently
    /// disarmed the gate: the reloaded run re-enabled taint tracking, found
    /// every region back at `Public`, and let outbound tools through that had
    /// been blocked a moment earlier. Any restart, crash-recovery, `resume`, or
    /// page-in did it.
    ///
    /// Defaults to `Public` for snapshots written before this field existed -
    /// the same value they were being restored with anyway, so nothing is worse
    /// than it was, and new runs are correct from their first write.
    #[serde(default)]
    pub taint: crate::taint::TaintLevel,
}

/// Per-region token snapshot written by the background worker after each inference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegionSnapshot {
    /// The region's name, matching its key under `[context.regions]`.
    pub name: String,
    /// Stringified kind: "pinned", "temporary", "clearable", "sliding", "compacting", "history"
    pub kind: String,
    /// Tokens the region held when the snapshot was taken.
    pub current_tokens: usize,
    /// The region's ceiling at snapshot time, already resolved against the
    /// model in front of it, so a percentage budget appears here as a number.
    pub max_tokens: usize,
    /// Actual content entries stored in this region (empty for zero-token regions).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<RegionEntrySnapshot>,
}

/// Snapshot of the full context window, written to `context.json` alongside `meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextSnapshot {
    /// The stage the run was in when this was written.
    pub stage_name: String,
    /// Tokens held across every region, which is what the next request costs
    /// before the model's reply.
    pub total_tokens: usize,
    /// The whole window's budget, from the blueprint's `total_budget_tokens` or
    /// the model's own limit.
    pub max_tokens: usize,
    /// Every region, in layout order.
    pub regions: Vec<RegionSnapshot>,
}

/// Status of an individual stage within a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StageRunStatus {
    /// Declared but not yet entered.
    Pending,
    /// The stage the run is in right now. At most one stage is `Active`.
    Active,
    /// Entered, and blocked on a person answering.
    WaitingInput,
    /// Finished and left. A stage that loops back becomes `Active` again.
    Complete,
    /// Ended in a failure. The run's own `error` carries the message.
    Error,
    /// The run finished without ever entering this stage.
    ///
    /// Distinct from [`Pending`](Self::Pending), which means "not yet" while a
    /// run is live, and from [`Complete`](Self::Complete), which these used to
    /// be recorded as: the ledger marked every stage positioned before the
    /// cursor complete, and a graph does not visit its stages in index order,
    /// so an error-recovery branch nothing reached was filed as having run
    /// (#372). Its `region_tokens` is empty because nothing ever wrote it,
    /// which made the next real stage look like it had written every region
    /// from zero.
    Skipped,
}

impl std::fmt::Display for StageRunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StageRunStatus::Pending => write!(f, "Pending"),
            StageRunStatus::Skipped => write!(f, "Skipped"),
            StageRunStatus::Active => write!(f, "Active"),
            StageRunStatus::WaitingInput => write!(f, "WaitingInput"),
            StageRunStatus::Complete => write!(f, "Complete"),
            StageRunStatus::Error => write!(f, "Error"),
        }
    }
}

/// Metadata record for a single stage within a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageRecord {
    /// The stage's name, matching its key under `[stages]`.
    pub name: String,
    /// Zero-based position in the blueprint's stage list.
    pub index: usize,
    /// Where this stage stands.
    pub status: StageRunStatus,
    /// Whether the run has ever actually been in this stage.
    ///
    /// Position cannot answer this. A graph blueprint reaches its stages in
    /// whatever order its edges describe, so "index below the cursor" includes
    /// every branch the run went past without taking - and reading it as
    /// "finished" is what filed never-entered stages as `Complete` (#372).
    /// Sticky once set, so a stage the run has left and may re-enter stays
    /// entered.
    #[serde(default)]
    pub entered: bool,
    /// Input tokens billed while this stage was active. A revisited stage keeps
    /// accumulating rather than resetting, so the run's total is the sum.
    pub prompt_tokens: usize,
    /// Output tokens billed while this stage was active, accumulating the same
    /// way.
    pub completion_tokens: usize,
    /// Tokens read from provider cache in this stage.
    #[serde(default)]
    pub cached_tokens: usize,
    /// Tokens *written* to provider cache in this stage.
    ///
    /// Without it only half of a cache decision was visible: a stage showing
    /// no reads might be paying to write a prefix nothing reuses, or might not
    /// be caching at all, and the ledger could not tell those apart.
    #[serde(default)]
    pub cache_write_tokens: usize,
    /// Per-region token contribution to this stage's calls, by region name.
    ///
    /// The central question of a structured layout is "what am I paying to
    /// carry, and where", and answering it meant replaying the context history
    /// and grouping by stage - archaeology for something the runtime already
    /// knows. Recorded as the largest each region reached while the stage was
    /// active, which is the number that decides whether a region is earning its
    /// place.
    ///
    /// Every region the window carries is measured, including the ones a stage
    /// layout hides rather than declares, so a stage can list a region it never
    /// assembled into a request.
    #[serde(default)]
    pub region_tokens: std::collections::BTreeMap<String, usize>,
    /// Prompt tokens billed by this stage's first call, the baseline the
    /// runaway-context check compares against. `None` until it runs once.
    #[serde(default)]
    pub first_call_prompt_tokens: Option<usize>,
    /// Whether the runaway-context warning has already fired for this stage, so
    /// it is said once on the crossing rather than on every call afterwards.
    #[serde(default)]
    pub runaway_warned: bool,
    /// Unix timestamp (seconds); None until the stage starts.
    pub started_at: Option<i64>,
    /// Unix timestamp (seconds); None until the stage ends.
    pub ended_at: Option<i64>,
}

impl StageRecord {
    /// A stage the run has not entered yet: [`StageRunStatus::Pending`], zero
    /// tokens, and neither timestamp set.
    pub fn new(name: String, index: usize) -> Self {
        Self {
            name,
            index,
            status: StageRunStatus::Pending,
            entered: false,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            region_tokens: std::collections::BTreeMap::new(),
            first_call_prompt_tokens: None,
            runaway_warned: false,
            started_at: None,
            ended_at: None,
        }
    }
}

/// Current Unix time in seconds (saturating to 0 before the epoch).
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn sample_meta() -> RunMeta {
        RunMeta::new(
            "run-1".to_string(),
            "agent".to_string(),
            "/agents/agent".to_string(),
            "do the thing".to_string(),
            Some("claude-sonnet-4-6".to_string()),
            "/work".to_string(),
            3,
        )
    }

    /// The webhook signing secret must not survive into anything served over
    /// the API - an unredacted meta lets `GET /api/agents` hand it to any
    /// token holder.
    #[test]
    fn redacted_drops_the_callback_secret_and_keeps_everything_else() {
        let mut m = sample_meta();
        m.callback_secret = Some("shhh".to_string());
        m.callback_url = Some("https://example.com/hook".to_string());

        let r = m.redacted();
        assert_eq!(r.callback_secret, None);
        // The URL is not a secret and stays: a caller needs to see where its own
        // webhook was pointed.
        assert_eq!(r.callback_url.as_deref(), Some("https://example.com/hook"));
        assert_eq!(r.run_id, m.run_id);
        assert_eq!(r.task, m.task);

        // Serializing the redacted form must not mention it at all - a `None`
        // that still emitted `"callback_secret": null` would be fine, but an
        // assertion on the wire format is what a reviewer actually checks.
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("shhh"), "{json}");

        // ...and the original is untouched, because the daemon still needs it to
        // sign the webhook for a run it reloaded after a restart.
        assert_eq!(m.callback_secret.as_deref(), Some("shhh"));
    }

    #[test]
    fn run_meta_new_sets_defaults() {
        let m = sample_meta();
        assert_eq!(m.run_id, "run-1");
        assert_eq!(m.agent_name, "agent");
        assert_eq!(m.agent_path, "/agents/agent");
        assert_eq!(m.task, "do the thing");
        assert_eq!(m.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(m.workdir, "/work");
        assert_eq!(m.num_stages, 3);
        assert_eq!(m.pid, 0);
        assert_eq!(m.status, RunStatus::Starting);
        assert_eq!(m.stage_index, 0);
        assert_eq!(m.iteration, 0);
        assert_eq!(m.prompt_tokens, 0);
        assert_eq!(m.completion_tokens, 0);
        assert_eq!(m.cached_tokens, 0);
        assert_eq!(m.cache_write_tokens, 0);
        assert_eq!(m.tool_calls, 0);
        assert!(m.error.is_none());
        assert!(m.title.is_none());
        assert!(m.metadata.is_empty());
        assert!(m.callback_url.is_none());
        assert!(m.callback_secret.is_none());
        assert!(m.parent_run_id.is_none());
        assert!(m.children.is_empty());
        assert_eq!(m.depth, 0);
        assert_eq!(m.max_child_depth, 0);
        assert!(m.current_stage.is_empty());
        assert_eq!(m.started_at, m.updated_at);
    }

    #[test]
    fn run_meta_touch_advances_updated_at() {
        let mut m = sample_meta();
        m.updated_at = 0;
        m.touch();
        assert!(m.updated_at > 0);
    }

    /// A `meta.json` written before `waiting_on` existed still loads.
    ///
    /// This is the whole compatibility question for the field, and it is worth
    /// a test rather than a reading of the serde attributes: every run already
    /// on disk was written by a build that had never heard of it, and a
    /// deserialize that insisted on the key would make every one of them
    /// unreadable.
    #[test]
    fn a_run_written_before_waiting_on_existed_still_loads() {
        let mut original = sample_meta();
        original.status = RunStatus::WaitingInput;
        let mut value = serde_json::to_value(&original).unwrap();
        // Whatever the current build writes, an older file simply has no such
        // key. Removing it reproduces that exactly.
        value
            .as_object_mut()
            .expect("meta is an object")
            .remove("waiting_on");
        assert!(value.get("waiting_on").is_none(), "the old shape");

        let back: RunMeta = serde_json::from_value(value).unwrap();
        assert_eq!(back.waiting_on, None);
        assert_eq!(back.status, RunStatus::WaitingInput);
        assert_eq!(back.run_id, original.run_id);
    }

    /// A run that is not parked writes the file it always wrote, so an older
    /// build reading a newer run sees nothing it does not understand.
    #[test]
    fn a_run_that_is_not_parked_writes_no_waiting_on_key() {
        let mut m = sample_meta();
        m.status = RunStatus::Running;
        m.waiting_on = None;
        let json = serde_json::to_value(&m).unwrap();
        assert!(json.get("waiting_on").is_none(), "{json}");

        m.waiting_on = Some(WaitReason::FanOutWorkers { outstanding: 3 });
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(
            json["waiting_on"],
            serde_json::json!({"reason": "fan_out_workers", "outstanding": 3})
        );
    }

    /// Every variant is on the wire in snake_case, the way `RunStatus` is, and
    /// round-trips. The counted ones carry their number with them, which is
    /// what lets a client say "waiting on 3 of them" rather than "waiting".
    #[test]
    fn wait_reason_serializes_in_snake_case() {
        for (variant, wire) in [
            (WaitReason::ToolApproval, "tool_approval"),
            (WaitReason::UserPrompt, "user_prompt"),
            (WaitReason::TaintGate, "taint_gate"),
            (WaitReason::InteractionPoint, "interaction_point"),
            (
                WaitReason::FanOutWorkers { outstanding: 3 },
                "fan_out_workers",
            ),
            (WaitReason::Children { outstanding: 1 }, "children"),
        ] {
            let json = serde_json::to_value(&variant).unwrap();
            assert_eq!(json["reason"], serde_json::json!(wire));
            let back: WaitReason = serde_json::from_value(json).unwrap();
            assert_eq!(back, variant);
        }
    }

    /// Each marker names its own reason, and the counted ones carry the count.
    ///
    /// The precedence is the specific claim first: a taint gate and a
    /// checkpoint each open a hub request of their own, so a generic-prompt
    /// answer would swallow both.
    #[test]
    fn each_marker_names_its_own_reason_specific_first() {
        let cases = [
            (
                WaitMarkers {
                    gate_prompt: true,
                    awaiting_interaction: true,
                    ..Default::default()
                },
                WaitReason::TaintGate,
            ),
            (
                WaitMarkers {
                    interaction_point: true,
                    awaiting_interaction: true,
                    ..Default::default()
                },
                WaitReason::InteractionPoint,
            ),
            (
                WaitMarkers {
                    fan_out_outstanding: Some(5),
                    ..Default::default()
                },
                WaitReason::FanOutWorkers { outstanding: 5 },
            ),
            (
                WaitMarkers {
                    children_outstanding: Some(4),
                    ..Default::default()
                },
                WaitReason::Children { outstanding: 4 },
            ),
        ];
        for (markers, expected) in cases {
            assert_eq!(
                wait_reason_from(true, &markers),
                Some(expected),
                "{markers:?}"
            );
        }
        // A parent holding both kinds of sub-work reports the more specific one.
        assert_eq!(
            wait_reason_from(
                true,
                &WaitMarkers {
                    fan_out_outstanding: Some(2),
                    children_outstanding: Some(9),
                    ..Default::default()
                }
            ),
            Some(WaitReason::FanOutWorkers { outstanding: 2 })
        );
    }

    /// A run parked until the machine is fixed says so before anything else.
    ///
    /// It outranks every other marker on purpose: answering a prompt does not
    /// help a run whose provider is not configured, so sending someone to the
    /// prompt would be sending them to the wrong screen.
    #[test]
    fn needing_setup_outranks_every_other_reason() {
        let need = SetupNeeded {
            blocker: SetupBlocker::ProviderMissing,
            remedy: "add it to config.toml".to_string(),
        };
        let reason = wait_reason_from(
            true,
            &WaitMarkers {
                needs_setup: Some(need.clone()),
                // Everything else at once, so precedence is being tested
                // rather than the absence of competition.
                gate_prompt: true,
                interaction_point: true,
                fan_out_outstanding: Some(2),
                children_outstanding: Some(3),
                awaiting_interaction: true,
                interaction: Some(crate::interaction::InteractionKind::ToolApproval),
            },
        );
        assert_eq!(
            reason,
            Some(WaitReason::NeedsSetup {
                blocker: SetupBlocker::ProviderMissing,
                remedy: "add it to config.toml".to_string(),
            })
        );
        assert!(
            reason.unwrap().needs_a_person(),
            "nothing resolves this without somebody"
        );
    }

    /// Each blocker is its own value on the wire, so a console can offer the
    /// right remedy instead of matching on the sentence.
    #[test]
    fn every_blocker_has_its_own_wire_name_and_label() {
        for (blocker, wire, label) in [
            (
                SetupBlocker::ProviderMissing,
                "provider_missing",
                "provider",
            ),
            (
                SetupBlocker::CreditsExhausted,
                "credits_exhausted",
                "credits",
            ),
            (SetupBlocker::AuthFailed, "auth_failed", "key"),
            (SetupBlocker::Forbidden, "forbidden", "access"),
            (
                SetupBlocker::ProvidersUnavailable,
                "providers_unavailable",
                "providers",
            ),
        ] {
            assert_eq!(serde_json::to_value(blocker).unwrap(), wire);
            assert_eq!(blocker.to_string(), label);
            let back: SetupBlocker = serde_json::from_value(serde_json::json!(wire)).unwrap();
            assert_eq!(back, blocker);
            // The row renders the kind, not the sentence: a remedy is a
            // sentence and this is a table cell.
            assert_eq!(
                WaitReason::NeedsSetup {
                    blocker,
                    remedy: "a whole sentence that would not fit".to_string(),
                }
                .to_string(),
                format!("needs {label}")
            );
        }
    }

    /// A generic hub block reports what kind of prompt it is, so "approve this
    /// tool call" and "answer this question" are not the same row.
    #[test]
    fn a_hub_block_reports_the_kind_of_prompt_holding_it() {
        let held = |kind| WaitMarkers {
            awaiting_interaction: true,
            interaction: kind,
            ..Default::default()
        };
        assert_eq!(
            wait_reason_from(
                true,
                &held(Some(crate::interaction::InteractionKind::ToolApproval))
            ),
            Some(WaitReason::ToolApproval)
        );
        // Anything else the agent asked for is a question for a person. The
        // kind can also be unknown while the block is real, which reads the
        // same way: somebody is being waited on.
        assert_eq!(
            wait_reason_from(
                true,
                &held(Some(crate::interaction::InteractionKind::FreeText))
            ),
            Some(WaitReason::UserPrompt)
        );
        assert_eq!(
            wait_reason_from(true, &held(None)),
            Some(WaitReason::UserPrompt)
        );
    }

    /// Parked with nothing claiming it: the field is left off rather than
    /// filled with a guess, and a run that is not parked never has one.
    #[test]
    fn nothing_claiming_a_parked_run_reports_no_reason() {
        assert_eq!(wait_reason_from(true, &WaitMarkers::default()), None);
        assert_eq!(
            wait_reason_from(
                false,
                &WaitMarkers {
                    gate_prompt: true,
                    ..Default::default()
                }
            ),
            None,
            "a run that is not waiting is not waiting on anything"
        );
    }

    /// The rendered form every text surface uses, counts included. Narrow
    /// enough for a table column, which is why it is not the variant name.
    #[test]
    fn every_reason_renders_for_a_narrow_column() {
        assert_eq!(WaitReason::ToolApproval.to_string(), "tool approval");
        assert_eq!(WaitReason::UserPrompt.to_string(), "user prompt");
        assert_eq!(WaitReason::TaintGate.to_string(), "taint gate");
        assert_eq!(WaitReason::InteractionPoint.to_string(), "checkpoint");
        assert_eq!(
            WaitReason::FanOutWorkers { outstanding: 3 }.to_string(),
            "workers(3)"
        );
        assert_eq!(
            WaitReason::Children { outstanding: 2 }.to_string(),
            "children(2)"
        );
    }

    /// Only the two engine-side reasons resolve on their own; the rest are a
    /// person's to clear. This is the predicate a badge should be built on.
    #[test]
    fn only_the_engine_side_reasons_need_nobody() {
        assert!(WaitReason::ToolApproval.needs_a_person());
        assert!(WaitReason::UserPrompt.needs_a_person());
        assert!(WaitReason::TaintGate.needs_a_person());
        assert!(WaitReason::InteractionPoint.needs_a_person());
        assert!(!WaitReason::FanOutWorkers { outstanding: 2 }.needs_a_person());
        assert!(!WaitReason::Children { outstanding: 2 }.needs_a_person());
    }

    #[test]
    fn run_meta_serde_roundtrip() {
        let mut m = sample_meta();
        m.status = RunStatus::Running;
        m.metadata.insert("k".to_string(), "v".to_string());
        m.title = Some("A title".to_string());
        m.callback_secret = Some("shh".to_string());
        m.parent_run_id = Some("parent-1".to_string());
        m.children = vec!["child-a".to_string(), "child-b".to_string()];
        m.depth = 2;
        m.max_child_depth = 5;
        let json = serde_json::to_string(&m).unwrap();
        let back: RunMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.run_id, m.run_id);
        assert_eq!(back.status, RunStatus::Running);
        assert_eq!(back.metadata.get("k").map(String::as_str), Some("v"));
        assert_eq!(back.title.as_deref(), Some("A title"));
        assert_eq!(back.callback_secret.as_deref(), Some("shh"));
        assert_eq!(back.parent_run_id.as_deref(), Some("parent-1"));
        assert_eq!(
            back.children,
            vec!["child-a".to_string(), "child-b".to_string()]
        );
        assert_eq!(back.depth, 2);
        assert_eq!(back.max_child_depth, 5);
    }

    #[test]
    fn run_status_display_all_variants() {
        assert_eq!(RunStatus::Starting.to_string(), "Starting");
        assert_eq!(RunStatus::Running.to_string(), "Running");
        assert_eq!(RunStatus::WaitingInput.to_string(), "WaitingInput");
        assert_eq!(RunStatus::Complete.to_string(), "Complete");
        assert_eq!(
            RunStatus::CompleteInteractive.to_string(),
            "CompleteInteractive"
        );
        assert_eq!(RunStatus::Paused.to_string(), "Paused");
        assert_eq!(RunStatus::Error.to_string(), "Error");
        assert_eq!(RunStatus::Cancelled.to_string(), "Cancelled");
    }

    #[test]
    fn run_status_serde_snake_case_roundtrip() {
        for s in [
            RunStatus::Starting,
            RunStatus::Running,
            RunStatus::WaitingInput,
            RunStatus::Complete,
            RunStatus::CompleteInteractive,
            RunStatus::Paused,
            RunStatus::Error,
            RunStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: RunStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, s);
        }
        assert_eq!(
            serde_json::to_string(&RunStatus::WaitingInput).unwrap(),
            "\"waiting_input\""
        );
        assert_eq!(
            serde_json::to_string(&RunStatus::Paused).unwrap(),
            "\"paused\""
        );
    }

    #[test]
    fn context_snapshot_serde_roundtrip() {
        let snap = ContextSnapshot {
            stage_name: "plan".to_string(),
            total_tokens: 42,
            max_tokens: 100,
            regions: vec![RegionSnapshot {
                name: "history".to_string(),
                kind: "sliding".to_string(),
                current_tokens: 10,
                max_tokens: 50,
                entries: vec![RegionEntrySnapshot {
                    content: "hi".to_string(),
                    tokens: 1,
                    kind: crate::region::EntryKind::UserMessage,
                    metadata: Some(serde_json::json!({"a": 1})),
                    key: Some("k".to_string()),
                    taint: Default::default(),
                }],
            }],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ContextSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.stage_name, "plan");
        assert_eq!(back.regions.len(), 1);
        assert_eq!(back.regions[0].entries.len(), 1);
        assert_eq!(back.regions[0].entries[0].content, "hi");
        assert_eq!(back.regions[0].entries[0].key.as_deref(), Some("k"));
    }

    #[test]
    fn region_snapshot_skips_empty_entries_in_json() {
        let snap = RegionSnapshot {
            name: "r".to_string(),
            kind: "pinned".to_string(),
            current_tokens: 0,
            max_tokens: 0,
            entries: vec![],
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("entries"));
    }

    #[test]
    fn stage_run_status_display_all_variants() {
        assert_eq!(StageRunStatus::Pending.to_string(), "Pending");
        assert_eq!(StageRunStatus::Skipped.to_string(), "Skipped");
        assert_eq!(StageRunStatus::Active.to_string(), "Active");
        assert_eq!(StageRunStatus::WaitingInput.to_string(), "WaitingInput");
        assert_eq!(StageRunStatus::Complete.to_string(), "Complete");
        assert_eq!(StageRunStatus::Error.to_string(), "Error");
    }

    #[test]
    fn run_flags_record_modification_dedups_paths_and_caps_the_list() {
        let mut flags = RunFlags::default();
        flags.record_modification("src/a.rs");
        flags.record_modification("src/a.rs");
        flags.record_modification("src/b.rs");
        assert_eq!(flags.modified_file_count, 3);
        assert_eq!(flags.modified_files, vec!["src/a.rs", "src/b.rs"]);

        // Past the cap the count keeps rising but the list stops growing, so a
        // long run can't bloat meta.json.
        for i in 0..MAX_TRACKED_MODIFIED_FILES {
            flags.record_modification(&format!("f{i}.rs"));
        }
        assert_eq!(flags.modified_files.len(), MAX_TRACKED_MODIFIED_FILES);
        assert_eq!(flags.modified_file_count, 3 + MAX_TRACKED_MODIFIED_FILES);
    }

    #[test]
    fn run_meta_flags_default_for_older_files() {
        // meta.json written before #107 has no `flags` key at all.
        let mut meta = RunMeta::new(
            "r".to_string(),
            "a".to_string(),
            "/p".to_string(),
            "t".to_string(),
            None,
            "/w".to_string(),
            1,
        );
        meta.flags.empty_output = true;
        // Drop the key structurally rather than by string surgery: a literal
        // spelling of the serialized flags silently stops matching the moment a
        // field is added, and the test then passes for the wrong reason.
        let mut json = serde_json::to_value(&meta).unwrap();
        json.as_object_mut().unwrap().remove("flags").unwrap();
        assert!(!json.to_string().contains("flags"));
        let back: RunMeta = serde_json::from_value(json).unwrap();
        assert_eq!(back.flags, RunFlags::default());
    }

    #[test]
    fn stage_record_new_and_serde_roundtrip() {
        let rec = StageRecord::new("analyze".to_string(), 2);
        assert_eq!(rec.name, "analyze");
        assert_eq!(rec.index, 2);
        assert_eq!(rec.status, StageRunStatus::Pending);
        assert_eq!(rec.prompt_tokens, 0);
        assert_eq!(rec.completion_tokens, 0);
        assert_eq!(rec.cached_tokens, 0);
        assert!(rec.started_at.is_none());
        assert!(rec.ended_at.is_none());

        let json = serde_json::to_string(&rec).unwrap();
        let back: StageRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "analyze");
        assert_eq!(back.status, StageRunStatus::Pending);
    }
}
