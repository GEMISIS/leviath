//! The live frames, typed.
//!
//! `/ws` sends every frame to every subscriber and leaves the filtering to the
//! client. Here a subscription says which frame types it wants and which runs
//! it cares about, and the filtering happens before a frame is converted or
//! serialized: a client watching one run of five thousand pays for one run's
//! frames.
//!
//! The union below is one type per frame the daemon can send, plus one the
//! daemon cannot: [`EventsDropped`]. A subscriber that falls behind is skipped
//! past rather than allowed to hold the fan-out, and it is told so with a
//! count, which is what tells "nothing happened" from "you missed some of it".

use async_graphql::{Enum, SimpleObject, Union};

use super::super::events::ServerEvent;
use super::scalars::{BigInt, Decimal, Timestamp};

/// One frame type off the daemon broadcast.
///
/// Subscribe with the types you render. Each value names the union member it
/// selects for, so a client reads one vocabulary for the filter and for the
/// frames it gets back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Enum)]
pub(crate) enum RunEventType {
    /// A run spawned.
    RunSpawned,
    /// A run's status, stage or counters moved.
    RunStatusChanged,
    /// A run got its title.
    RunRenamed,
    /// Token usage ticked up.
    Tokens,
    /// Context window totals moved.
    ContextUpdate,
    /// The run entered a new stage.
    StageTransition,
    /// A tool call began.
    ToolCallStarted,
    /// A tool call finished.
    ToolCallFinished,
    /// One log line.
    Log,
    /// Spend crossed a threshold.
    RunSpend,
    /// A run parked on a prompt somebody has to answer.
    InteractionNeeded,
    /// A run reached a terminal status.
    RunCompleted,
    /// The serving daemon's identity or link state changed.
    DaemonLink,
    /// The config file's health changed.
    ConfigHealth,
    /// A self-update step moved.
    UpdateProgress,
    /// A self-update finished.
    UpdateFinished,
    /// This subscription fell behind and frames were dropped.
    EventsDropped,
}

impl RunEventType {
    /// The type of one frame.
    pub(crate) fn of(event: &ServerEvent) -> Self {
        match event {
            ServerEvent::AgentSpawned { .. } => Self::RunSpawned,
            ServerEvent::AgentStatus { .. } => Self::RunStatusChanged,
            ServerEvent::RunRenamed { .. } => Self::RunRenamed,
            ServerEvent::Tokens { .. } => Self::Tokens,
            ServerEvent::ContextUpdate { .. } => Self::ContextUpdate,
            ServerEvent::StageTransition { .. } => Self::StageTransition,
            ServerEvent::ToolCallStarted { .. } => Self::ToolCallStarted,
            ServerEvent::ToolCallFinished { .. } => Self::ToolCallFinished,
            ServerEvent::Log { .. } => Self::Log,
            ServerEvent::AgentSpend { .. } => Self::RunSpend,
            ServerEvent::InteractionNeeded { .. } => Self::InteractionNeeded,
            ServerEvent::AgentCompleted { .. } => Self::RunCompleted,
            ServerEvent::DaemonLink { .. } => Self::DaemonLink,
            ServerEvent::ConfigHealth { .. } => Self::ConfigHealth,
            ServerEvent::UpdateProgress { .. } => Self::UpdateProgress,
            ServerEvent::UpdateFinished { .. } => Self::UpdateFinished,
        }
    }
}

/// A run spawned.
#[derive(Debug, SimpleObject)]
pub(crate) struct RunSpawned {
    /// The run's durable id.
    pub(crate) run_id: String,
    /// The agent's live id in the world, which a run outlives.
    pub(crate) agent_id: String,
    /// The blueprint it was spawned from, by name.
    pub(crate) blueprint: String,
    /// The run that spawned it, for a sub-agent.
    pub(crate) parent_id: Option<String>,
}

/// A run's status, stage or counters moved.
#[derive(Debug, SimpleObject)]
pub(crate) struct RunStatusChanged {
    /// The run.
    pub(crate) run_id: String,
    /// The agent's live id.
    pub(crate) agent_id: String,
    /// The status, in the daemon's own spelling.
    pub(crate) status: String,
    /// The stage it is in, by name.
    pub(crate) stage: String,
    /// Inference turns in that stage.
    pub(crate) iteration: i32,
    /// Tool calls across the whole run.
    pub(crate) tool_calls: i32,
    /// Whether a message reaches this run right now.
    pub(crate) accepts_messages: bool,
    /// Its title, once it has one.
    pub(crate) title: Option<String>,
}

/// A run got its title.
#[derive(Debug, SimpleObject)]
pub(crate) struct RunRenamed {
    /// The run.
    pub(crate) run_id: String,
    /// The agent's live id.
    pub(crate) agent_id: String,
    /// The new title.
    pub(crate) title: String,
}

/// Token usage ticked up.
#[derive(Debug, SimpleObject)]
pub(crate) struct TokensUpdated {
    /// The run.
    pub(crate) run_id: String,
    /// The agent's live id.
    pub(crate) agent_id: String,
    /// Input tokens so far, cached ones included.
    pub(crate) prompt_tokens: BigInt,
    /// Output tokens so far.
    pub(crate) completion_tokens: BigInt,
    /// Counted within `promptTokens`, not on top of it.
    pub(crate) cached_tokens: BigInt,
    /// Tokens written to the provider's cache.
    pub(crate) cache_write_tokens: BigInt,
}

/// Context window totals moved.
#[derive(Debug, SimpleObject)]
pub(crate) struct ContextUpdated {
    /// The run.
    pub(crate) run_id: String,
    /// The agent's live id.
    pub(crate) agent_id: String,
    /// Tokens held across every region.
    pub(crate) total_tokens: i32,
    /// The window's budget.
    pub(crate) max_tokens: i32,
}

/// The run entered a new stage.
#[derive(Debug, SimpleObject)]
pub(crate) struct StageTransition {
    /// The run.
    pub(crate) run_id: String,
    /// The agent's live id.
    pub(crate) agent_id: String,
    /// The stage it left.
    pub(crate) from: String,
    /// The stage it entered.
    pub(crate) to: String,
    /// How many times the destination has been entered, this entry included.
    pub(crate) iteration: i32,
}

/// A tool call began.
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolCallStarted {
    /// The run.
    pub(crate) run_id: String,
    /// The agent's live id.
    pub(crate) agent_id: String,
    /// The provider's own call id, kept as external correlation. Not an
    /// identity: a provider may reuse one across a retry or a reissue, which is
    /// why the execution id exists.
    pub(crate) call_id: String,
    /// This attempt's own id, minted at dispatch and recorded in the journal.
    /// What pairs a start with its finish, and either with the journal. Empty
    /// from a daemon that predates execution identity.
    pub(crate) execution_id: String,
    /// The tool called.
    pub(crate) tool: String,
}

/// A tool call finished.
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolCallFinished {
    /// The run.
    pub(crate) run_id: String,
    /// The agent's live id.
    pub(crate) agent_id: String,
    /// The provider's own call id, as correlation. See `ToolCallStarted`.
    pub(crate) call_id: String,
    /// The attempt that finished, matching the start's.
    pub(crate) execution_id: String,
    /// The tool called.
    pub(crate) tool: String,
    /// Whether the call took effect. False for a refused or failed call: a
    /// finish is not a success on its own.
    ///
    /// Read from the result's own text, so it is a summary rather than the
    /// durable verdict. The recorded outcome lives on the execution in the
    /// journal, where a refusal and a failure are different states and an
    /// unobserved ending is its own.
    pub(crate) ok: bool,
    /// The result, flattened to one line and cut to fit.
    pub(crate) summary: String,
}

/// One log line.
#[derive(Debug, SimpleObject)]
pub(crate) struct LogLine {
    /// The run.
    pub(crate) run_id: String,
    /// The agent's live id.
    pub(crate) agent_id: String,
    /// The line, without its trailing newline. Long lines are cut for the
    /// broadcast; the stage log on disk keeps the whole thing.
    pub(crate) line: String,
}

/// Spend crossed a threshold.
#[derive(Debug, SimpleObject)]
pub(crate) struct RunSpend {
    /// The run.
    pub(crate) run_id: String,
    /// The agent's live id.
    pub(crate) agent_id: String,
    /// The figure that was crossed, in dollars.
    pub(crate) threshold_usd: Decimal,
    /// What the run has spent so far, in dollars.
    pub(crate) total_usd: Decimal,
    /// Whether every call behind that total could be priced. When false the
    /// run has spent at least this and more by an unknown amount, so it must
    /// not be shown as a final figure.
    pub(crate) complete: bool,
    /// The stage that was running when it crossed.
    pub(crate) stage: String,
}

/// What kind of answer a parked run is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum InteractionKind {
    /// The person writes anything.
    FreeText,
    /// The person picks from options.
    MultipleChoice,
    /// The person confirms or denies.
    Confirm,
    /// The person approves or denies a tool call.
    ToolApproval,
    /// The person edits a document directly.
    EditText,
}

impl From<&leviath_core::interaction::InteractionKind> for InteractionKind {
    fn from(kind: &leviath_core::interaction::InteractionKind) -> Self {
        use leviath_core::interaction::InteractionKind as Core;
        match kind {
            Core::FreeText => Self::FreeText,
            Core::MultipleChoice => Self::MultipleChoice,
            Core::Confirm => Self::Confirm,
            Core::ToolApproval => Self::ToolApproval,
            Core::EditText => Self::EditText,
        }
    }
}

/// One pending ask, parked on a run.
#[derive(Debug, SimpleObject)]
pub(crate) struct InteractionRequest {
    /// The request's id, which is what an answer names.
    pub(crate) id: String,
    /// What kind of answer it takes.
    pub(crate) kind: InteractionKind,
    /// What the person is asked.
    pub(crate) prompt: String,
    /// A longer document shown alongside the prompt, such as a plan to review.
    pub(crate) body: Option<String>,
    /// The choices, for a multiple-choice ask.
    pub(crate) options: Vec<String>,
    /// The call awaiting approval, for an approval ask: the tool and the
    /// arguments it would run with, paired and typed.
    ///
    /// Null for every other kind of ask. An approval that names a tool whose
    /// arguments do not fit its shape comes through untyped rather than tidied:
    /// deciding whether to approve a call means seeing what it actually says.
    // Boxed because a typed call is as large as the largest tool's arguments,
    // and every live frame would otherwise carry that much room for one it
    // almost never holds. An implementation detail, so not part of the
    // description a client reads.
    pub(crate) tool_call: Option<Box<super::types::tool_calls::ToolCall>>,
    /// The stage the run is in.
    pub(crate) stage_name: String,
    /// Whether the run holds until this is answered.
    pub(crate) required: bool,
}

impl From<leviath_core::interaction::InteractionRequest> for InteractionRequest {
    fn from(request: leviath_core::interaction::InteractionRequest) -> Self {
        Self {
            id: request.id,
            kind: (&request.kind).into(),
            prompt: request.prompt,
            body: request.body,
            options: request.options,
            tool_call: request.tool_name.map(|tool| {
                Box::new(super::types::tool_calls::from_value(
                    &tool,
                    None,
                    // A request that names a tool and no arguments is a call with
                    // none, which is what an empty object says.
                    request
                        .tool_arguments
                        .unwrap_or_else(|| serde_json::Value::Object(Default::default())),
                ))
            }),
            stage_name: request.stage_name,
            required: request.required,
        }
    }
}

/// A run parked on a prompt somebody has to answer.
#[derive(Debug, SimpleObject)]
pub(crate) struct InteractionNeeded {
    /// The run.
    pub(crate) run_id: String,
    /// The agent's live id.
    pub(crate) agent_id: String,
    /// The parked request. Null when this build cannot read the shape the
    /// daemon sent, which is what a newer daemon's new kind looks like.
    pub(crate) request: Option<InteractionRequest>,
}

/// A run reached a terminal status.
#[derive(Debug, SimpleObject)]
pub(crate) struct RunCompleted {
    /// The run.
    pub(crate) run_id: String,
    /// The agent's live id.
    pub(crate) agent_id: String,
    /// The terminal status.
    pub(crate) status: String,
    /// The run's error, when that is how it ended.
    pub(crate) error: Option<String>,
    /// The answer it submitted, when there is one.
    pub(crate) final_output: Option<CompletedOutput>,
}

/// The answer carried on a completion frame.
#[derive(Debug, SimpleObject)]
pub(crate) struct CompletedOutput {
    /// The answer itself.
    pub(crate) content: String,
    /// The output format label the submission carried.
    pub(crate) format: Option<String>,
    /// The stage that submitted it.
    pub(crate) stage: String,
    /// When it was submitted.
    pub(crate) submitted_at: Timestamp,
    /// True when the stored answer was cut to fit.
    pub(crate) truncated: bool,
}

/// The daemon on the other end of the link.
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

/// The link to the daemon changed.
///
/// About the machine rather than a run, and delivered to every subscription,
/// scoped or not, because it explains why a run's frames stopped.
#[derive(Debug, SimpleObject)]
pub(crate) struct DaemonLinkChanged {
    /// Whether this server is receiving the daemon's events right now.
    pub(crate) connected: bool,
    /// Who is on the other end, once it has said.
    pub(crate) daemon: Option<DaemonIdentity>,
    /// Whether the daemon behind the link is a different process than before:
    /// a restart this server lived through and its clients need not.
    pub(crate) restarted: bool,
    /// Present when the daemon and this server run different code, with what
    /// to do about it. Requests keep working while the two still understand
    /// each other.
    pub(crate) restart_advised: Option<String>,
}

/// The config file's health changed.
#[derive(Debug, SimpleObject)]
pub(crate) struct ConfigHealthChanged {
    /// Whether the file on disk loads right now.
    pub(crate) healthy: bool,
    /// The file that was checked.
    pub(crate) path: String,
    /// One line on why it does not load. Absent when healthy.
    pub(crate) error: Option<String>,
    /// While unhealthy this is the last good save, not what is on disk.
    pub(crate) config_mtime: Option<Timestamp>,
}

/// A self-update step moved.
#[derive(Debug, SimpleObject)]
pub(crate) struct UpdateProgress {
    /// The update job.
    pub(crate) job_id: String,
    /// Which part of the install the step touches.
    pub(crate) step: String,
    /// The step's status.
    pub(crate) status: String,
    /// One line about what just happened, ready to print.
    pub(crate) detail: String,
}

/// A self-update finished.
#[derive(Debug, SimpleObject)]
pub(crate) struct UpdateFinished {
    /// The update job.
    pub(crate) job_id: String,
    /// Its terminal status.
    pub(crate) status: String,
    /// Whether both this server and the daemon keep running the old build
    /// until they are restarted.
    pub(crate) restart_required: bool,
}

/// This subscription fell behind, and frames were dropped.
///
/// The daemon's broadcast is bounded, and a subscriber that cannot keep up is
/// skipped past rather than allowed to hold the fan-out. This frame is what
/// tells a quiet run from a missed one. Re-query the state you render when you
/// see it.
#[derive(Debug, SimpleObject)]
pub(crate) struct EventsDropped {
    /// How many frames this subscription missed.
    pub(crate) count: BigInt,
}

/// One frame off the daemon broadcast.
#[derive(Union)]
pub(crate) enum RunEvent {
    /// A run spawned.
    RunSpawned(RunSpawned),
    /// A run's status, stage or counters moved.
    RunStatusChanged(RunStatusChanged),
    /// A run got its title.
    RunRenamed(RunRenamed),
    /// Token usage ticked up.
    TokensUpdated(TokensUpdated),
    /// Context window totals moved.
    ContextUpdated(ContextUpdated),
    /// The run entered a new stage.
    StageTransition(StageTransition),
    /// A tool call began.
    ToolCallStarted(ToolCallStarted),
    /// A tool call finished.
    ToolCallFinished(ToolCallFinished),
    /// One log line.
    LogLine(LogLine),
    /// Spend crossed a threshold.
    RunSpend(RunSpend),
    /// A run parked on a prompt somebody has to answer.
    InteractionNeeded(InteractionNeeded),
    /// A run reached a terminal status.
    RunCompleted(RunCompleted),
    /// The link to the daemon changed.
    DaemonLinkChanged(DaemonLinkChanged),
    /// The config file's health changed.
    ConfigHealthChanged(ConfigHealthChanged),
    /// A self-update step moved.
    UpdateProgress(UpdateProgress),
    /// A self-update finished.
    UpdateFinished(UpdateFinished),
    /// This subscription fell behind.
    EventsDropped(EventsDropped),
}

impl From<ServerEvent> for RunEvent {
    fn from(event: ServerEvent) -> Self {
        match event {
            ServerEvent::AgentSpawned {
                agent_id,
                run_id,
                parent_id,
                blueprint,
            } => Self::RunSpawned(RunSpawned {
                run_id,
                agent_id,
                blueprint,
                parent_id,
            }),
            ServerEvent::AgentStatus {
                agent_id,
                run_id,
                status,
                stage,
                iteration,
                tool_calls,
                accepts_messages,
                title,
                ..
            } => Self::RunStatusChanged(RunStatusChanged {
                run_id,
                agent_id,
                status,
                stage,
                iteration: count(iteration),
                tool_calls: count(tool_calls),
                accepts_messages,
                title,
            }),
            ServerEvent::RunRenamed {
                agent_id,
                run_id,
                title,
            } => Self::RunRenamed(RunRenamed {
                run_id,
                agent_id,
                title,
            }),
            ServerEvent::Tokens {
                agent_id,
                run_id,
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                cache_write_tokens,
            } => Self::TokensUpdated(TokensUpdated {
                run_id,
                agent_id,
                prompt_tokens: BigInt(prompt_tokens as i64),
                completion_tokens: BigInt(completion_tokens as i64),
                cached_tokens: BigInt(cached_tokens as i64),
                cache_write_tokens: BigInt(cache_write_tokens as i64),
            }),
            ServerEvent::ContextUpdate {
                agent_id,
                run_id,
                total_tokens,
                max_tokens,
            } => Self::ContextUpdated(ContextUpdated {
                run_id,
                agent_id,
                total_tokens: count(total_tokens),
                max_tokens: count(max_tokens),
            }),
            ServerEvent::StageTransition {
                agent_id,
                run_id,
                from,
                to,
                iteration,
            } => Self::StageTransition(StageTransition {
                run_id,
                agent_id,
                from,
                to,
                iteration: count(iteration),
            }),
            ServerEvent::ToolCallStarted {
                agent_id,
                run_id,
                call_id,
                execution_id,
                tool,
            } => Self::ToolCallStarted(ToolCallStarted {
                run_id,
                agent_id,
                call_id,
                execution_id,
                tool,
            }),
            ServerEvent::ToolCallFinished {
                agent_id,
                run_id,
                call_id,
                execution_id,
                tool,
                ok,
                summary,
            } => Self::ToolCallFinished(ToolCallFinished {
                run_id,
                agent_id,
                call_id,
                execution_id,
                tool,
                ok,
                summary,
            }),
            ServerEvent::Log {
                agent_id,
                run_id,
                line,
            } => Self::LogLine(LogLine {
                run_id,
                agent_id,
                line,
            }),
            ServerEvent::AgentSpend {
                agent_id,
                run_id,
                threshold_usd,
                total_usd,
                complete,
                stage,
            } => Self::RunSpend(RunSpend {
                run_id,
                agent_id,
                threshold_usd: Decimal(threshold_usd),
                total_usd: Decimal(total_usd),
                complete,
                stage,
            }),
            ServerEvent::InteractionNeeded {
                agent_id,
                run_id,
                request,
            } => Self::InteractionNeeded(InteractionNeeded {
                run_id,
                agent_id,
                // The frame forwards the daemon's own JSON, so a kind this
                // build does not know reads as an absent request rather than
                // failing the whole subscription.
                request: serde_json::from_value::<leviath_core::interaction::InteractionRequest>(
                    request,
                )
                .ok()
                .map(InteractionRequest::from),
            }),
            ServerEvent::AgentCompleted {
                agent_id,
                run_id,
                status,
                result,
                final_output,
            } => Self::RunCompleted(RunCompleted {
                run_id,
                agent_id,
                status,
                error: result,
                final_output: final_output.map(|output| CompletedOutput {
                    content: output.content,
                    format: output.format,
                    stage: output.stage,
                    submitted_at: Timestamp(output.submitted_at),
                    truncated: output.truncated,
                }),
            }),
            ServerEvent::DaemonLink {
                connected,
                daemon,
                restarted,
                restart_advised,
            } => Self::DaemonLinkChanged(DaemonLinkChanged {
                connected,
                daemon: daemon.map(|identity| DaemonIdentity {
                    version: identity.version,
                    build: identity.build,
                    pid: i32::try_from(identity.pid).unwrap_or(i32::MAX),
                    tool_env: identity.tool_env,
                }),
                restarted,
                restart_advised,
            }),
            ServerEvent::ConfigHealth {
                healthy,
                path,
                error,
                config_mtime,
            } => Self::ConfigHealthChanged(ConfigHealthChanged {
                healthy,
                path,
                error: error.map(|info| info.message),
                config_mtime: config_mtime.map(Timestamp),
            }),
            ServerEvent::UpdateProgress {
                job_id,
                step,
                status,
                detail,
            } => Self::UpdateProgress(UpdateProgress {
                job_id,
                step,
                status,
                detail,
            }),
            ServerEvent::UpdateFinished {
                job_id,
                status,
                restart_required,
                ..
            } => Self::UpdateFinished(UpdateFinished {
                job_id,
                status,
                restart_required,
            }),
        }
    }
}

/// Narrow a daemon counter to the 32 bits GraphQL's `Int` carries.
fn count(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
