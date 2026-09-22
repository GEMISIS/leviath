//! The live frames, typed.
//!
//! `/ws` sends every frame to every subscriber and leaves the filtering to the
//! client. Here a subscription says which frames it wants and which runs it
//! cares about, and the filtering happens before a frame is converted or
//! serialized: a client watching one run of five thousand pays for one run's
//! frames.
//!
//! Every frame carries its place in the stream. [`Event`] is the interface
//! that says so - this server's own number for the frame, and the second it
//! sent it - and [`RunEvent`] adds the run the frame is about, with the run
//! itself readable on demand. So a client can select `seq at runId` once, in
//! one fragment, and only reach for a member type when it needs that member's
//! own fields.
//!
//! Two frames come from this server rather than from the daemon:
//! [`SubscriptionOpenedEvent`], which is always the first frame of every
//! subscription, and [`EventsDroppedEvent`], which says a subscriber fell
//! behind. They carry the stamp like everything else, so they implement
//! [`Event`]; they are about no run, so they stay out of [`RunEvent`], and
//! that is what keeps domain and transport separable with one fragment.
//!
//! The number is this server's, one counter for the whole process rather than
//! one per subscription. A subscription that asked for three frame types sees
//! the numbers of those three, so a gap is the ordinary shape of a filtered
//! stream and says nothing about what was missed. [`EventsDroppedEvent`] is
//! the only thing that announces a drop.

use std::sync::Arc;

use async_graphql::{ComplexObject, Context, Enum, ID, Interface, SimpleObject, Union};
use leviath_graphql_derive::mirror;

use super::super::config_types::ConfigErrorInfo;
use super::super::events::{ServerEvent, Stamped};
use super::super::types::{AppState, FinalOutputResp};
use super::super::update_job;
use super::scalars::{BigInt, Decimal, Timestamp};
/// Re-exported so a live frame's pending ask is the same type the
/// journal-backed interaction answers with, held in one place.
pub(crate) use super::types::interaction::InteractionOutput;
use super::types::machine::{ConfigError, ConfigErrorKind};
use super::types::run::{Run, RunStatus};
use super::types::run_detail::{FinalOutput, WaitReason};
use super::types::update::{
    DaemonStatus, UpdateJob, UpdateJobStatus, UpdateStep, UpdateStepStatus,
};

/// The stamp every frame carries, held together so the one place it is read
/// off the bus is the one place it is written onto a frame.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Head {
    /// This frame's place in the stream.
    seq: BigInt,
    /// When the server sent it.
    at: Timestamp,
}

impl Head {
    /// The stamp of one frame off the bus.
    fn of(stamped: &Stamped) -> Self {
        Self {
            seq: BigInt(i64::try_from(stamped.seq).unwrap_or(i64::MAX)),
            at: Timestamp(stamped.at),
        }
    }
}

/// The run a frame is about, read only when a client selects it.
///
/// Read through the same cached index every listing reads, so a subscription
/// watching a busy run pays a stat rather than a parse per frame, and a frame
/// about a run that has since been deleted answers null rather than failing
/// the subscription.
async fn run_of(ctx: &Context<'_>, run_id: &ID) -> async_graphql::Result<Option<Run>> {
    let state = ctx.data_unchecked::<AppState>();
    let snapshot = state.caches.run_index.snapshot().await;
    let now = leviath_core::duration::now_secs();
    Ok(snapshot.get(run_id.as_str()).map(|meta| Run {
        meta: Arc::clone(meta),
        now,
    }))
}

/// Define the frames, the vocabularies that name them, and the unions that
/// carry them.
///
/// One table, so the four can never disagree: a frame listed here gets its
/// type, its value in the `*EventType` enum a subscription filters with, its
/// arm in the mapping off the bus, and its place in the union at once. The
/// fields every frame shares are written once rather than sixteen times.
macro_rules! frames {
    (
        run {
            $(
                $(#[$rmeta:meta])*
                $rsource:ident => $rvalue:ident => $revent:ident {
                    $( $(#[$rfmeta:meta])* $rfield:ident : $rfty:ty ),* $(,)?
                }
            )*
        }
        machine {
            $(
                $(#[$mmeta:meta])*
                $msource:ident => $mvalue:ident => $mevent:ident {
                    $( $(#[$mfmeta:meta])* $mfield:ident : $mfty:ty ),* $(,)?
                }
            )*
        }
    ) => {
        $(
            $(#[$rmeta])*
            #[derive(Debug, SimpleObject)]
            #[graphql(complex)]
            pub(crate) struct $revent {
                /// Where this frame sits in this server's own numbering of
                /// frames, counting from one.
                ///
                /// One counter for the server, not one per subscription. Two
                /// subscriptions open at once see the same frame under the
                /// same number, and a subscription that asked for some frame
                /// types sees only those numbers, so gaps are ordinary and say
                /// nothing. `EventsDroppedEvent` is what announces a drop.
                seq: BigInt,
                /// When the server sent it, in unix seconds.
                at: Timestamp,
                /// The run this is about.
                run_id: ID,
                /// The agent's live id in the world, which a run outlives.
                agent_id: ID,
                $( $(#[$rfmeta])* $rfield: $rfty, )*
            }

            impl $revent {
                /// Build one from the stamp, the two ids every run frame
                /// carries, and this frame's own fields.
                ///
                /// The frame's own fields arrive as one tuple, so a frame with
                /// nine of them and a frame with one are built through a
                /// function of the same shape.
                fn new(
                    head: Head,
                    run_id: String,
                    agent_id: String,
                    fields: ($($rfty,)*),
                ) -> Self {
                    let ($($rfield,)*) = fields;
                    Self {
                        seq: head.seq,
                        at: head.at,
                        run_id: ID(run_id),
                        agent_id: ID(agent_id),
                        $($rfield,)*
                    }
                }
            }

            #[ComplexObject]
            impl $revent {
                /// The run this frame is about, as the rest of the API reads
                /// it. Null once the run has been deleted.
                async fn run(&self, ctx: &Context<'_>) -> async_graphql::Result<Option<Run>> {
                    run_of(ctx, &self.run_id).await
                }
            }

        )*

        $(
            $(#[$mmeta])*
            #[derive(Debug, SimpleObject)]
            pub(crate) struct $mevent {
                /// Where this frame sits in this server's own numbering of
                /// frames, counting from one. Server-wide rather than per
                /// subscription, so a gap says nothing and only
                /// `EventsDroppedEvent` announces a drop.
                seq: BigInt,
                /// When the server sent it, in unix seconds.
                at: Timestamp,
                $( $(#[$mfmeta])* $mfield: $mfty, )*
            }
        )*

        /// One frame about a run.
        ///
        /// Select `seq at runId agentId` once and every run frame answers it,
        /// whatever it turns out to be. `run` is read only when selected.
        #[derive(Debug, Interface)]
        #[graphql(
            field(
                name = "seq",
                ty = "&BigInt",
                desc = "Where this frame sits in this server's own numbering of frames."
            ),
            field(name = "at", ty = "&Timestamp", desc = "When the server sent it."),
            field(name = "run_id", ty = "&ID", desc = "The run this is about."),
            field(
                name = "agent_id",
                ty = "&ID",
                desc = "The agent's live id in the world, which a run outlives."
            ),
            field(
                name = "run",
                ty = "Option<Run>",
                desc = "The run this frame is about, read only when selected."
            )
        )]
        pub(crate) enum RunEvent {
            $( $(#[$rmeta])* $rvalue($revent), )*
        }

        /// One frame a subscription can yield, whatever it turns out to be.
        ///
        /// The interface every frame implements, the two this server produces
        /// about the subscription itself included, so `... on Event { seq at }`
        /// reads the stream's bookkeeping off any frame at all. Telling a
        /// domain frame from a transport one is what the union is for, and
        /// `RunEvent` is what narrows to the frames about a run.
        #[derive(Debug, Interface)]
        #[graphql(
            field(
                name = "seq",
                ty = "&BigInt",
                desc = "Where this frame sits in this server's own numbering of frames."
            ),
            field(name = "at", ty = "&Timestamp", desc = "When the server sent it.")
        )]
        pub(crate) enum Event {
            $( $(#[$rmeta])* $rvalue($revent), )*
            $( $(#[$mmeta])* $mvalue($mevent), )*
            /// The subscription opened.
            SubscriptionOpened(SubscriptionOpenedEvent),
            /// This subscription fell behind.
            EventsDropped(EventsDroppedEvent),
        }

        /// One frame type a run subscription can ask for.
        ///
        /// Each value names the union member it selects for, so a client reads
        /// one vocabulary for the filter and for the frames it gets back.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Enum)]
        pub(crate) enum RunEventType {
            $( $(#[$rmeta])* $rvalue, )*
        }

        impl RunEventType {
            /// The type of one frame, or nothing when it is not about a run.
            pub(crate) fn of(event: &ServerEvent) -> Option<Self> {
                match event {
                    $( ServerEvent::$rsource { .. } => Some(Self::$rvalue), )*
                    $( ServerEvent::$msource { .. } => None, )*
                }
            }
        }

        /// One frame type a machine subscription can ask for.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Enum)]
        pub(crate) enum MachineEventType {
            $( $(#[$mmeta])* $mvalue, )*
        }

        impl MachineEventType {
            /// The type of one frame, or nothing when it is about a run.
            pub(crate) fn of(event: &ServerEvent) -> Option<Self> {
                match event {
                    $( ServerEvent::$msource { .. } => Some(Self::$mvalue), )*
                    $( ServerEvent::$rsource { .. } => None, )*
                }
            }
        }

        /// What `runEvents` yields.
        ///
        /// The run frames, plus the link frame - a scoped subscriber has to
        /// learn why its run's frames stopped - plus the two this server
        /// produces about the subscription itself.
        #[derive(Debug, Union)]
        pub(crate) enum RunEventFrame {
            $( $(#[$rmeta])* $rvalue($revent), )*
            /// The link to the daemon changed, which is why a run's frames
            /// stopped or started again.
            DaemonLinkChanged(DaemonLinkChangedEvent),
            /// The subscription opened.
            SubscriptionOpened(SubscriptionOpenedEvent),
            /// This subscription fell behind.
            EventsDropped(EventsDroppedEvent),
        }

        /// What `machineEvents` yields.
        #[derive(Debug, Union)]
        pub(crate) enum MachineEventFrame {
            $( $(#[$mmeta])* $mvalue($mevent), )*
            /// The subscription opened.
            SubscriptionOpened(SubscriptionOpenedEvent),
            /// This subscription fell behind.
            EventsDropped(EventsDroppedEvent),
        }

        /// Every frame type this build knows, by its GraphQL type name, paired
        /// with the enum value that selects it. Read by the test that holds
        /// the two in step; nothing else needs it.
        #[cfg(test)]
        pub(crate) const FRAME_VOCABULARY: &[(&str, &str)] = &[
            $( (stringify!($revent), stringify!($rvalue)), )*
            $( (stringify!($mevent), stringify!($mvalue)), )*
        ];
    };
}

frames! {
    run {
        /// A run started, including one spawned as a child of another.
        AgentSpawned => RunSpawned => RunSpawnedEvent {
            /// The blueprint it was spawned from, by name.
            blueprint_name: String,
            /// The run that spawned it, for a sub-agent. Null at the root.
            parent_id: Option<ID>,
        }

        /// A run's status, stage or counters moved.
        AgentStatus => RunStatusChanged => RunStatusChangedEvent {
            /// Where the run stands now.
            status: RunStatus,
            /// The stage it is in, by name.
            stage: String,
            /// Inference turns in that stage, reset on entering a new one.
            iteration: BigInt,
            /// Tool calls across the whole run.
            tool_calls: BigInt,
            /// Whether a message reaches this run right now.
            accepts_messages: bool,
            /// Why the run is parked, when it is. Null for a run that is
            /// moving.
            wait_reason: Option<WaitReason>,
            /// Its title, once it has one.
            title: Option<String>,
        }

        /// A run acquired a title, or had the one it was showing replaced.
        RunRenamed => RunRenamed => RunRenamedEvent {
            /// The title the run now goes by.
            title: String,
        }

        /// Token usage ticked up.
        Tokens => TokensUpdated => TokensUpdatedEvent {
            /// Input tokens so far, cached ones included.
            prompt_tokens: BigInt,
            /// Output tokens so far.
            completion_tokens: BigInt,
            /// Counted within `promptTokens`, not on top of it.
            cached_tokens: BigInt,
            /// Tokens written to the provider's cache.
            cache_write_tokens: BigInt,
        }

        /// How full the run's context window is, after a turn changed it.
        ContextUpdate => ContextUpdated => ContextUpdatedEvent {
            /// Tokens held across every region.
            total_tokens: BigInt,
            /// The whole window's budget.
            max_tokens: BigInt,
        }

        /// The run entered a new stage.
        StageTransition => StageTransitioned => StageTransitionedEvent {
            /// The stage it left.
            from: String,
            /// The stage it entered.
            to: String,
            /// How many times the destination has been entered, this entry
            /// included.
            iteration: BigInt,
        }

        /// A tool call was handed to the async tool lane.
        ToolCallStarted => ToolCallStarted => ToolCallStartedEvent {
            /// The provider's own call id, kept as external correlation. Not
            /// an identity: a provider may reuse one across a retry, which is
            /// why the execution id exists.
            call_id: String,
            /// This attempt's own id, minted at dispatch and recorded in the
            /// journal. What pairs a start with its finish, and either with
            /// the journal. Empty from a daemon that predates execution
            /// identity.
            execution_id: ID,
            /// The tool called.
            tool: String,
        }

        /// A lane-executed tool call returned, paired with its start by the
        /// execution id.
        ToolCallFinished => ToolCallFinished => ToolCallFinishedEvent {
            /// The provider's own call id, matching the start frame's.
            call_id: String,
            /// The attempt that finished, matching the start frame's.
            execution_id: ID,
            /// The tool called.
            tool: String,
            /// Whether the call took effect. False for a refused or failed
            /// call: a finish is not a success on its own.
            ///
            /// Read from the result's own text, so it is a summary rather than
            /// the durable verdict. The recorded outcome lives on the
            /// execution in the journal.
            ok: bool,
            /// The result, flattened to one line and cut to fit.
            summary: String,
        }

        /// One log line, as also written to the stage's log files.
        Log => LogLineWritten => LogLineWrittenEvent {
            /// The line, without its trailing newline. Long lines are cut for
            /// the broadcast; the stage log on disk keeps the whole thing.
            line: String,
        }

        /// A run's spend passed a figure the operator asked to be told about.
        AgentSpend => SpendThresholdCrossed => SpendThresholdCrossedEvent {
            /// The figure that was crossed, in dollars.
            threshold_usd: Decimal,
            /// What the run has spent so far, in dollars.
            total_usd: Decimal,
            /// Whether every call behind that total could be priced. When
            /// false the run has spent at least this and more by an unknown
            /// amount, so it must not be shown as a final figure.
            complete: bool,
            /// The stage that was running when it crossed.
            stage: String,
        }

        /// A run parked on a prompt somebody has to answer.
        InteractionNeeded => InteractionOpened => InteractionOpenedEvent {
            /// The parked ask, in the shape every listing serves it in, with
            /// `settlement` null because it is still open. Null when this build
            /// cannot read the shape the daemon sent, which is what a newer
            /// daemon's new kind looks like.
            interaction: Option<InteractionOutput>,
        }

        /// A run reached a terminal status.
        AgentCompleted => RunCompleted => RunCompletedEvent {
            /// The terminal state the run reached.
            status: RunStatus,
            /// The run's error, when that is how it ended.
            error: Option<String>,
            /// The answer it submitted, when there is one.
            final_output: Option<FinalOutput>,
        }
    }

    machine {
        /// The link between this server and the daemon changed.
        ///
        /// About the machine rather than a run, and delivered to every
        /// subscription, scoped or not, because it explains why a run's frames
        /// stopped.
        DaemonLink => DaemonLinkChanged => DaemonLinkChangedEvent {
            /// Whether this server is receiving the daemon's events right now.
            connected: bool,
            /// Who is on the other end, once it has said.
            daemon: Option<DaemonIdentity>,
            /// Whether the daemon behind the link is a different process than
            /// before: a restart this server lived through and its clients
            /// need not.
            restarted: bool,
            /// Present when the daemon and this server run different code,
            /// with what to do about it. Requests keep working while the two
            /// still understand each other.
            restart_advised: Option<String>,
        }

        /// Whether the config file on disk loads has changed.
        ConfigHealth => ConfigHealthChanged => ConfigHealthChangedEvent {
            /// Whether the file on disk loads right now.
            healthy: bool,
            /// The file that was checked.
            path: String,
            /// Why it does not load. Null when healthy.
            error: Option<ConfigError>,
            /// While unhealthy this is the last good save, not what is on
            /// disk.
            config_mtime: Option<Timestamp>,
        }

        /// A step of an update started by `startUpdate` changed.
        UpdateProgress => UpdateStepChanged => UpdateStepChangedEvent {
            /// The job this is about.
            job_id: ID,
            /// Which part of the install the step touches.
            step: UpdateStep,
            /// Where that step got to.
            status: UpdateStepStatus,
            /// One line about what just happened, ready to print.
            detail: String,
        }

        /// An update started by `startUpdate` reached a terminal status.
        UpdateFinished => UpdateFinished => UpdateFinishedEvent {
            /// The job that finished.
            job_id: ID,
            /// Where the run as a whole got to.
            status: UpdateJobStatus,
            /// The whole record, so a client that connected mid-run, or
            /// dropped a frame, renders the result with no follow-up request.
            job: UpdateJob,
            /// Whether the binary on disk is now newer than the processes
            /// serving this. Both keep running the old build until they are
            /// restarted.
            restart_required: bool,
        }
    }
}

/// The daemon on the other end of the link.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct DaemonIdentity {
    /// The version it was built from.
    pub(crate) version: String,
    /// Its build id.
    pub(crate) build: String,
    /// Its process id.
    pub(crate) pid: i32,
    /// Which tool credentials it can see, by name. Names only: no value ever
    /// crosses the wire.
    ///
    /// Null means the process did not say, which is an older daemon or an
    /// embedder driving the runtime directly. That is "unknown", not "sees
    /// nothing" - an empty list is the answer for that.
    pub(crate) tool_env: Option<Vec<String>>,
}

/// The subscription is open, and this is what it is looking at.
///
/// Always the first frame of every subscription, before anything the daemon
/// sent. A client that gets one knows the stream is live, knows which process
/// is numbering it, and knows whether the daemon behind it is reachable, all
/// without a second request.
#[derive(Debug, SimpleObject)]
pub(crate) struct SubscriptionOpenedEvent {
    /// The number the last frame before this subscription carried, or zero on
    /// a server that has sent none. Every frame on this subscription has a
    /// number above it, though not every number above it arrives here.
    pub(crate) seq: BigInt,
    /// When the subscription opened, in unix seconds.
    pub(crate) at: Timestamp,
    /// The server process serving this subscription.
    ///
    /// Sequence numbers are that process's own. Two subscriptions reporting
    /// different instances were served by different processes, so a client
    /// that was reconnecting has to re-read rather than resume.
    pub(crate) server_instance: ID,
    /// The daemon this server is talking to, as of right now.
    pub(crate) daemon: DaemonStatus,
}

/// This subscription fell behind, and frames were dropped.
///
/// The daemon's broadcast is bounded, and a subscriber that cannot keep up is
/// skipped past rather than allowed to hold the fan-out. This frame is what
/// tells a quiet run from a missed one. Re-read whatever you render when you
/// see it; the subscription itself stays open.
#[derive(Debug, SimpleObject)]
pub(crate) struct EventsDroppedEvent {
    /// How many frames went past unread. Counted off the server's own
    /// numbering, so it includes frames this subscription's filter would have
    /// dropped anyway: an upper bound on what was missed, never an under-count.
    pub(crate) count: BigInt,
    /// The number of the last frame that did arrive.
    pub(crate) seq: BigInt,
    /// When the gap was noticed, in unix seconds.
    pub(crate) at: Timestamp,
}

/// What `updateJobEvents` yields: one job's own frames, and the two about the
/// subscription itself.
#[derive(Debug, Union)]
pub(crate) enum UpdateJobEventFrame {
    /// A step of the job changed.
    UpdateStepChanged(UpdateStepChangedEvent),
    /// The job reached a terminal status.
    UpdateFinished(UpdateFinishedEvent),
    /// The subscription opened.
    SubscriptionOpened(SubscriptionOpenedEvent),
    /// This subscription fell behind.
    EventsDropped(EventsDroppedEvent),
}

/// The frame that opens every subscription.
///
/// `since` is the bus's number as it stood before the receiver was registered,
/// and it is the caller's to read rather than this function's: a filter takes
/// time to resolve, and every frame that arrives while it does is buffered for
/// delivery. Numbering the greeting from here would put it at or above one of
/// those frames, and a client following the field's promise would drop them.
pub(crate) fn opened(state: &AppState, since: u64) -> SubscriptionOpenedEvent {
    SubscriptionOpenedEvent {
        seq: BigInt(i64::try_from(since).unwrap_or(i64::MAX)),
        at: Timestamp(leviath_core::duration::now_secs()),
        server_instance: ID(super::super::events::server_instance().to_string()),
        daemon: DaemonStatus::of(state.control.link(), state.control.code_mismatch()),
    }
}

/// The frame that says a subscriber was skipped past, with the last stamp it
/// did see.
pub(crate) fn dropped(count: u64, last: u64) -> EventsDroppedEvent {
    EventsDroppedEvent {
        count: BigInt(i64::try_from(count).unwrap_or(i64::MAX)),
        seq: BigInt(i64::try_from(last).unwrap_or(i64::MAX)),
        at: Timestamp(leviath_core::duration::now_secs()),
    }
}

/// A count from the daemon, as a 64-bit number.
fn big(value: usize) -> BigInt {
    BigInt(i64::try_from(value).unwrap_or(i64::MAX))
}

/// The link frame, which both a run subscription and a machine one receive.
fn daemon_link_changed(
    head: Head,
    connected: bool,
    daemon: Option<leviath_runtime::control_socket::DaemonIdentity>,
    restarted: bool,
    restart_advised: Option<String>,
) -> DaemonLinkChangedEvent {
    DaemonLinkChangedEvent {
        seq: head.seq,
        at: head.at,
        connected,
        daemon: daemon.map(|identity| DaemonIdentity {
            version: identity.version,
            build: identity.build,
            pid: i32::try_from(identity.pid).unwrap_or(i32::MAX),
            tool_env: identity.tool_env,
        }),
        restarted,
        restart_advised,
    }
}

/// Why the config does not load, as the schema says it.
fn config_error(error: ConfigErrorInfo) -> ConfigError {
    ConfigError {
        kind: ConfigErrorKind::from_wire(&error.kind),
        path: error.path,
        message: error.message,
        line: error.line.and_then(|line| i32::try_from(line).ok()),
        column: error.column.and_then(|column| i32::try_from(column).ok()),
        key: error.key,
        since: Timestamp(error.since),
        note: error.note,
    }
}

/// One step of an update moving, as both the machine and the per-job
/// subscription report it.
fn update_step_changed(
    head: Head,
    job_id: String,
    step: update_job::Step,
    status: update_job::StepStatus,
    detail: String,
) -> UpdateStepChangedEvent {
    use update_job::{Step, StepStatus};
    UpdateStepChangedEvent {
        seq: head.seq,
        at: head.at,
        job_id: ID(job_id),
        step: match step {
            Step::Binary => UpdateStep::Binary,
            Step::Agents => UpdateStep::Blueprints,
            Step::Keys => UpdateStep::Keys,
            Step::Migrations => UpdateStep::Migrations,
        },
        status: match status {
            StepStatus::Pending => UpdateStepStatus::Pending,
            StepStatus::Running => UpdateStepStatus::Running,
            StepStatus::Done => UpdateStepStatus::Done,
            StepStatus::Skipped => UpdateStepStatus::Skipped,
            StepStatus::Advised => UpdateStepStatus::Advised,
            StepStatus::Failed => UpdateStepStatus::Failed,
        },
        detail,
    }
}

/// An update finishing, as both the machine and the per-job subscription
/// report it.
fn update_finished(
    head: Head,
    job_id: String,
    status: update_job::JobStatus,
    job: update_job::UpdateJob,
    restart_required: bool,
) -> UpdateFinishedEvent {
    use update_job::JobStatus;
    UpdateFinishedEvent {
        seq: head.seq,
        at: head.at,
        job_id: ID(job_id),
        status: match status {
            JobStatus::Running => UpdateJobStatus::Running,
            JobStatus::Complete => UpdateJobStatus::Complete,
            JobStatus::Failed => UpdateJobStatus::Failed,
        },
        job: UpdateJob::from(job),
        restart_required,
    }
}

/// The answer a completion frame carried.
fn final_output(output: FinalOutputResp) -> FinalOutput {
    FinalOutput {
        content: output.content,
        format: output.format,
        stage: output.stage,
        submitted_at: Timestamp(output.submitted_at),
        truncated: output.truncated,
    }
}

/// One stamped frame as a run subscription sees it, or nothing when it is
/// about the machine rather than a run.
pub(crate) fn run_frame(stamped: Stamped) -> Option<RunEventFrame> {
    let head = Head::of(&stamped);
    Some(match stamped.event {
        ServerEvent::AgentSpawned {
            agent_id,
            run_id,
            parent_id,
            blueprint,
        } => RunSpawnedEvent::new(head, run_id, agent_id, (blueprint, parent_id.map(ID))).into(),
        ServerEvent::AgentStatus {
            agent_id,
            run_id,
            status,
            stage,
            iteration,
            tool_calls,
            accepts_messages,
            wait_reason,
            title,
        } => RunStatusChangedEvent::new(
            head,
            run_id,
            agent_id,
            (
                RunStatus::from_wire(&status),
                stage,
                big(iteration),
                big(tool_calls),
                accepts_messages,
                wait_reason.as_ref().map(WaitReason::from),
                title,
            ),
        )
        .into(),
        ServerEvent::RunRenamed {
            agent_id,
            run_id,
            title,
        } => RunRenamedEvent::new(head, run_id, agent_id, (title,)).into(),
        ServerEvent::Tokens {
            agent_id,
            run_id,
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            cache_write_tokens,
        } => TokensUpdatedEvent::new(
            head,
            run_id,
            agent_id,
            (
                big(prompt_tokens),
                big(completion_tokens),
                big(cached_tokens),
                big(cache_write_tokens),
            ),
        )
        .into(),
        ServerEvent::ContextUpdate {
            agent_id,
            run_id,
            total_tokens,
            max_tokens,
        } => ContextUpdatedEvent::new(head, run_id, agent_id, (big(total_tokens), big(max_tokens)))
            .into(),
        ServerEvent::StageTransition {
            agent_id,
            run_id,
            from,
            to,
            iteration,
        } => StageTransitionedEvent::new(head, run_id, agent_id, (from, to, big(iteration))).into(),
        ServerEvent::ToolCallStarted {
            agent_id,
            run_id,
            call_id,
            execution_id,
            tool,
        } => ToolCallStartedEvent::new(head, run_id, agent_id, (call_id, ID(execution_id), tool))
            .into(),
        ServerEvent::ToolCallFinished {
            agent_id,
            run_id,
            call_id,
            execution_id,
            tool,
            ok,
            summary,
        } => ToolCallFinishedEvent::new(
            head,
            run_id,
            agent_id,
            (call_id, ID(execution_id), tool, ok, summary),
        )
        .into(),
        ServerEvent::Log {
            agent_id,
            run_id,
            line,
        } => LogLineWrittenEvent::new(head, run_id, agent_id, (line,)).into(),
        ServerEvent::AgentSpend {
            agent_id,
            run_id,
            threshold_usd,
            total_usd,
            complete,
            stage,
        } => SpendThresholdCrossedEvent::new(
            head,
            run_id,
            agent_id,
            (Decimal(threshold_usd), Decimal(total_usd), complete, stage),
        )
        .into(),
        ServerEvent::InteractionNeeded {
            agent_id,
            run_id,
            request,
        } => {
            // The frame forwards the daemon's own JSON, so a kind this build
            // does not know reads as an absent ask rather than failing the
            // whole subscription.
            let parked =
                serde_json::from_value::<leviath_core::interaction::InteractionRequest>(request)
                    .ok()
                    .map(|request| InteractionOutput::open(run_id.clone(), request));
            InteractionOpenedEvent::new(head, run_id, agent_id, (parked,)).into()
        }
        ServerEvent::AgentCompleted {
            agent_id,
            run_id,
            status,
            result,
            final_output: output,
        } => RunCompletedEvent::new(
            head,
            run_id,
            agent_id,
            (
                RunStatus::from_wire(&status),
                result,
                output.map(final_output),
            ),
        )
        .into(),
        ServerEvent::DaemonLink {
            connected,
            daemon,
            restarted,
            restart_advised,
        } => daemon_link_changed(head, connected, daemon, restarted, restart_advised).into(),
        ServerEvent::ConfigHealth { .. }
        | ServerEvent::UpdateProgress { .. }
        | ServerEvent::UpdateFinished { .. } => return None,
    })
}

/// One stamped frame as a machine subscription sees it, or nothing when it is
/// about a run.
pub(crate) fn machine_frame(stamped: Stamped) -> Option<MachineEventFrame> {
    let head = Head::of(&stamped);
    Some(match stamped.event {
        ServerEvent::DaemonLink {
            connected,
            daemon,
            restarted,
            restart_advised,
        } => daemon_link_changed(head, connected, daemon, restarted, restart_advised).into(),
        ServerEvent::ConfigHealth {
            healthy,
            path,
            error,
            config_mtime,
        } => ConfigHealthChangedEvent {
            seq: head.seq,
            at: head.at,
            healthy,
            path,
            error: error.map(config_error),
            config_mtime: config_mtime.map(Timestamp),
        }
        .into(),
        ServerEvent::UpdateProgress {
            job_id,
            step,
            status,
            detail,
        } => update_step_changed(head, job_id, step, status, detail).into(),
        ServerEvent::UpdateFinished {
            job_id,
            status,
            restart_required,
            job,
        } => update_finished(head, job_id, status, job, restart_required).into(),
        _ => return None,
    })
}

/// One stamped frame as a subscription on a single update job sees it.
///
/// Narrowed to `job` here rather than by the caller, because "this job's
/// frames" is the whole of what that subscription is.
pub(crate) fn update_job_frame(stamped: Stamped, job: &str) -> Option<UpdateJobEventFrame> {
    let head = Head::of(&stamped);
    match stamped.event {
        ServerEvent::UpdateProgress {
            job_id,
            step,
            status,
            detail,
        } if job_id == job => Some(update_step_changed(head, job_id, step, status, detail).into()),
        ServerEvent::UpdateFinished {
            job_id,
            status,
            restart_required,
            job: record,
        } if job_id == job => {
            Some(update_finished(head, job_id, status, record, restart_required).into())
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
