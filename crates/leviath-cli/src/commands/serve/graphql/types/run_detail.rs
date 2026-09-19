//! What a run holds beyond its summary: why it is parked, what it produced,
//! what each stage cost, and the context window it is working in.
//!
//! Every one of these is read from a file in the run's directory, so each is
//! its own field and none of them is read unless a client asks for it. That is
//! the difference between a run listing that costs one stat per run and one
//! that reads four files per run to answer a question nobody asked.

use async_graphql::{Enum, Object, SimpleObject};

use super::super::scalars::{BigInt, Decimal, Timestamp};
use super::run::{CostBreakdown, TokenUsage, WorkingClock};

/// Why a run is parked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum WaitReasonKind {
    /// Waiting on a tool-permission decision.
    ToolApproval,
    /// Waiting on an answer to a question the agent asked.
    UserPrompt,
    /// Waiting at a taint gate.
    TaintGate,
    /// Waiting at a blueprint interaction point.
    InteractionPoint,
    /// Fan-out workers are still running. Healthy; it resolves on its own.
    FanOutWorkers,
    /// Child runs are still running. Healthy; it resolves on its own.
    Children,
    /// Something on the machine has to change before this run can go on.
    NeedsSetup,
}

/// What has to change before a parked run can go on.
///
/// One value per remedy, not per error: topping up an account, adding a
/// provider and replacing a rejected key are three different screens, and a
/// client with only the sentence would have to match on its wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum SetupBlocker {
    /// The stage names a provider this install has not configured.
    ProviderMissing,
    /// The account behind the provider is out of credits.
    CreditsExhausted,
    /// The key was rejected.
    AuthFailed,
    /// The key is valid but not allowed to use the model.
    Forbidden,
    /// No provider is available at all.
    ProvidersUnavailable,
    /// The provider could not be reached.
    ProviderUnreachable,
    /// The provider took too long.
    ProviderTimedOut,
    /// The provider failed.
    ProviderFailed,
}

impl From<&leviath_core::run_meta::SetupBlocker> for SetupBlocker {
    fn from(blocker: &leviath_core::run_meta::SetupBlocker) -> Self {
        use leviath_core::run_meta::SetupBlocker as Core;
        match blocker {
            Core::ProviderMissing => Self::ProviderMissing,
            Core::CreditsExhausted => Self::CreditsExhausted,
            Core::AuthFailed => Self::AuthFailed,
            Core::Forbidden => Self::Forbidden,
            Core::ProvidersUnavailable => Self::ProvidersUnavailable,
            Core::ProviderUnreachable => Self::ProviderUnreachable,
            Core::ProviderTimedOut => Self::ProviderTimedOut,
            Core::ProviderFailed => Self::ProviderFailed,
        }
    }
}

/// Why a run is parked, and what would unblock it.
///
/// Present only while the run is in `WAITING_INPUT`. A run waiting on a person
/// carries the prompt in `interaction`; a run parked on its own sub-agents
/// carries the reason here and needs nobody.
#[derive(Debug, SimpleObject)]
pub(crate) struct WaitReason {
    /// The machine-readable cause.
    pub(crate) reason: WaitReasonKind,
    /// What is blocked. Present when `reason` is `NEEDS_SETUP`.
    pub(crate) blocker: Option<SetupBlocker>,
    /// What to do about it, in a sentence. Present with `blocker`.
    pub(crate) remedy: Option<String>,
    /// How many are still outstanding. Present for `FAN_OUT_WORKERS` and
    /// `CHILDREN`.
    pub(crate) outstanding: Option<i32>,
    /// Whether this needs a person, as against resolving on its own.
    ///
    /// The difference a client cares about: a run waiting on workers is
    /// healthy, and a run waiting on an answer is a row somebody has to act
    /// on.
    pub(crate) needs_a_person: bool,
}

impl From<&leviath_core::run_meta::WaitReason> for WaitReason {
    fn from(reason: &leviath_core::run_meta::WaitReason) -> Self {
        use leviath_core::run_meta::WaitReason as Core;
        let needs_a_person = reason.needs_a_person();
        match reason {
            Core::ToolApproval => Self::plain(WaitReasonKind::ToolApproval, needs_a_person),
            Core::UserPrompt => Self::plain(WaitReasonKind::UserPrompt, needs_a_person),
            Core::TaintGate => Self::plain(WaitReasonKind::TaintGate, needs_a_person),
            Core::InteractionPoint => Self::plain(WaitReasonKind::InteractionPoint, needs_a_person),
            Core::FanOutWorkers { outstanding } => Self {
                outstanding: Some(i32::try_from(*outstanding).unwrap_or(i32::MAX)),
                ..Self::plain(WaitReasonKind::FanOutWorkers, needs_a_person)
            },
            Core::Children { outstanding } => Self {
                outstanding: Some(i32::try_from(*outstanding).unwrap_or(i32::MAX)),
                ..Self::plain(WaitReasonKind::Children, needs_a_person)
            },
            Core::NeedsSetup { blocker, remedy } => Self {
                blocker: Some(blocker.into()),
                remedy: Some(remedy.clone()),
                ..Self::plain(WaitReasonKind::NeedsSetup, needs_a_person)
            },
        }
    }
}

impl WaitReason {
    /// A reason carrying nothing but its kind.
    fn plain(reason: WaitReasonKind, needs_a_person: bool) -> Self {
        Self {
            reason,
            blocker: None,
            remedy: None,
            outstanding: None,
            needs_a_person,
        }
    }
}

/// Post-hoc diagnostics: an empty or degraded run told from a healthy one
/// without reading logs.
#[derive(Debug, SimpleObject)]
pub(crate) struct RunFlags {
    /// The run finished with nothing to show.
    pub(crate) empty_output: bool,
    /// Whether the run ever submitted an answer.
    pub(crate) produced_output: bool,
    /// How many times a stage that must submit was let through without having
    /// submitted, after being asked again its bounded number of times.
    pub(crate) output_forced: i32,
    /// Whether the run was offered no tool it could answer with.
    pub(crate) no_output_tools: bool,
    /// How many transitions were taken with their gates forced.
    pub(crate) gates_forced: i32,
    /// How many times a stage exhausted its iteration budget.
    pub(crate) max_iterations_hit: i32,
    /// How many fan-outs ran degraded, with fewer workers than they split.
    pub(crate) splits_degraded: i32,
    /// Files the run recorded as changed.
    pub(crate) modified_file_count: i32,
    /// The files themselves, capped when recorded.
    pub(crate) modified_files: Vec<String>,
    /// Search operations the run performed.
    pub(crate) searches_run: i32,
    /// Searches that matched nothing.
    pub(crate) searches_empty: i32,
    /// Required regions the run left empty when it ended.
    pub(crate) required_regions_abandoned: Vec<String>,
    /// Whether the run's working directory went missing under it.
    pub(crate) workspace_lost: bool,
}

impl From<&leviath_core::run_meta::RunFlags> for RunFlags {
    fn from(flags: &leviath_core::run_meta::RunFlags) -> Self {
        Self {
            empty_output: flags.empty_output,
            produced_output: flags.produced_output,
            output_forced: count(flags.output_forced),
            no_output_tools: flags.no_output_tools,
            gates_forced: count(flags.gates_forced),
            max_iterations_hit: count(flags.max_iterations_hit),
            splits_degraded: count(flags.splits_degraded),
            modified_file_count: count(flags.modified_file_count),
            modified_files: flags.modified_files.clone(),
            searches_run: count(flags.searches_run),
            searches_empty: count(flags.searches_empty),
            required_regions_abandoned: flags.required_regions_abandoned.clone(),
            workspace_lost: flags.workspace_lost,
        }
    }
}

/// The answer a run submitted.
#[derive(Debug, SimpleObject)]
pub(crate) struct FinalOutput {
    /// The answer itself.
    pub(crate) content: String,
    /// The output format label the submission carried.
    pub(crate) format: Option<String>,
    /// The stage that submitted it.
    pub(crate) stage: String,
    /// When it was submitted, unix epoch seconds.
    pub(crate) submitted_at: Timestamp,
    /// True when the stored answer was cut to fit.
    pub(crate) truncated: bool,
}

impl From<leviath_core::FinalOutput> for FinalOutput {
    fn from(output: leviath_core::FinalOutput) -> Self {
        Self {
            content: output.content,
            format: output.format,
            stage: output.stage,
            submitted_at: Timestamp(output.submitted_at),
            truncated: output.truncated,
        }
    }
}

/// A stage's own lifecycle state within a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum StageStatus {
    /// Declared but not yet entered.
    Pending,
    /// The stage the run is in right now.
    Active,
    /// Entered, and blocked on a person answering.
    WaitingInput,
    /// Finished and left.
    Complete,
    /// Ended in a failure. The run's own error carries the message.
    Error,
    /// The run finished without ever entering this stage.
    Skipped,
}

impl From<&leviath_core::run_meta::StageRunStatus> for StageStatus {
    fn from(status: &leviath_core::run_meta::StageRunStatus) -> Self {
        use leviath_core::run_meta::StageRunStatus as Core;
        match status {
            Core::Pending => Self::Pending,
            Core::Active => Self::Active,
            Core::WaitingInput => Self::WaitingInput,
            Core::Complete => Self::Complete,
            Core::Error => Self::Error,
            Core::Skipped => Self::Skipped,
        }
    }
}

/// One visit to a stage.
#[derive(Debug, SimpleObject)]
pub(crate) struct StageVisit {
    /// When the run entered, unix epoch seconds.
    pub(crate) entered_at: Timestamp,
    /// When it left; null for the visit in progress.
    pub(crate) left_at: Option<Timestamp>,
    /// Tokens burned on this visit.
    pub(crate) usage: TokenUsage,
    /// Spend on this visit.
    pub(crate) cost: CostBreakdown,
}

/// The most one region reached while a stage was active.
#[derive(Debug, SimpleObject)]
pub(crate) struct RegionPeak {
    /// The region's name.
    pub(crate) region: String,
    /// The most tokens it held while this stage was active.
    pub(crate) tokens: i32,
}

/// One stage's record within a run: what it cost, and how often it ran.
#[derive(Debug, SimpleObject)]
pub(crate) struct StageRecord {
    /// The stage's name.
    pub(crate) name: String,
    /// Visit order within the blueprint.
    pub(crate) index: i32,
    /// The stage's own lifecycle state.
    pub(crate) status: StageStatus,
    /// False when the run never reached this stage.
    pub(crate) entered: bool,
    /// Token roll-up for this stage, across every visit.
    pub(crate) usage: TokenUsage,
    /// Spend roll-up for this stage, across every visit.
    pub(crate) cost: CostBreakdown,
    /// How many times the run entered this stage.
    pub(crate) visit_count: i32,
    /// One entry per stay, capped when recorded. When `visitCount` is larger
    /// than this list, the roll-ups above are still the complete figures.
    pub(crate) visits: Vec<StageVisit>,
    /// The most each region held while this stage was active.
    pub(crate) region_peaks: Vec<RegionPeak>,
    /// Whether the runaway detector fired here.
    pub(crate) runaway_warned: bool,
    /// First entry, unix epoch seconds.
    pub(crate) started_at: Option<Timestamp>,
    /// Last exit; null while the stage is active.
    pub(crate) ended_at: Option<Timestamp>,
    /// The working span in progress; null while the clock is stopped.
    pub(crate) active: Option<WorkingClock>,
}

impl From<&leviath_core::run_meta::StageRecord> for StageRecord {
    fn from(record: &leviath_core::run_meta::StageRecord) -> Self {
        let mut region_peaks: Vec<RegionPeak> = record
            .region_tokens
            .iter()
            .map(|(region, tokens)| RegionPeak {
                region: region.clone(),
                tokens: count(*tokens),
            })
            .collect();
        region_peaks.sort_by(|a, b| a.region.cmp(&b.region));
        Self {
            name: record.name.clone(),
            index: count(record.index),
            status: (&record.status).into(),
            entered: record.entered,
            usage: TokenUsage {
                prompt_tokens: BigInt(record.prompt_tokens as i64),
                completion_tokens: BigInt(record.completion_tokens as i64),
                cached_tokens: BigInt(record.cached_tokens as i64),
                cache_write_tokens: BigInt(record.cache_write_tokens as i64),
            },
            cost: CostBreakdown {
                cost_usd: record.cost_usd.map(Decimal),
                cost_priced_usd: Decimal(record.cost_priced_usd),
                cost_is_exact: record.cost_is_exact,
                unpriced_calls: count(record.unpriced_calls),
            },
            visit_count: count(record.visit_count),
            visits: record
                .visits
                .iter()
                .map(|visit| StageVisit {
                    entered_at: Timestamp(visit.entered_at),
                    left_at: visit.left_at.map(Timestamp),
                    usage: TokenUsage {
                        prompt_tokens: BigInt(visit.prompt_tokens as i64),
                        completion_tokens: BigInt(visit.completion_tokens as i64),
                        cached_tokens: BigInt(visit.cached_tokens as i64),
                        cache_write_tokens: BigInt(visit.cache_write_tokens as i64),
                    },
                    cost: CostBreakdown {
                        cost_usd: visit.cost_usd.map(Decimal),
                        cost_priced_usd: Decimal(visit.cost_priced_usd),
                        cost_is_exact: visit.cost_is_exact,
                        unpriced_calls: count(visit.unpriced_calls),
                    },
                })
                .collect(),
            region_peaks,
            runaway_warned: record.runaway_warned,
            started_at: record.started_at.map(Timestamp),
            ended_at: record.ended_at.map(Timestamp),
            active: record.active.map(|clock| WorkingClock {
                banked_secs: count(clock.banked_secs as usize),
                since: clock.since.map(Timestamp),
            }),
        }
    }
}

/// One region of a run's live context window.
///
/// The declared region is on the blueprint; this is what it holds right now.
/// Keeping them apart is what lets a client read either without the other.
pub(crate) struct ContextRegion {
    /// The snapshot this region came from, shared rather than copied.
    pub(crate) snapshot: std::sync::Arc<leviath_core::run_meta::ContextSnapshot>,
    /// Which region, by position in the window.
    pub(crate) at: usize,
}

#[Object]
impl ContextRegion {
    /// The region's name, matching the blueprint's declaration.
    async fn name(&self) -> &str {
        &self.region().name
    }

    /// What the region does when it fills, as the snapshot recorded it.
    async fn kind(&self) -> &str {
        &self.region().kind
    }

    /// Tokens it holds right now.
    async fn tokens(&self) -> i32 {
        count(self.region().current_tokens)
    }

    /// Its ceiling, in tokens.
    async fn max_tokens(&self) -> i32 {
        count(self.region().max_tokens)
    }

    /// How many entries it holds.
    async fn entry_count(&self) -> i32 {
        count(self.region().entries.len())
    }

    /// One line on what the region is for.
    async fn description(&self) -> Option<&str> {
        self.region().description.as_deref()
    }

    /// The region's text, entries joined in order.
    ///
    /// Heavy, and the reason the window is not one blob: select it only for the
    /// regions you display. An entry holding a stored part reads as whatever
    /// stand-in the registry gives it, which is what the model sees too.
    async fn content(&self) -> String {
        self.region()
            .entries
            .iter()
            .map(|entry| entry.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl ContextRegion {
    /// The region this object stands for.
    fn region(&self) -> &leviath_core::run_meta::RegionSnapshot {
        &self.snapshot.regions[self.at]
    }
}

/// A run's context window as it stands right now.
pub(crate) struct ContextWindow {
    /// The snapshot read from the run's directory.
    pub(crate) snapshot: std::sync::Arc<leviath_core::run_meta::ContextSnapshot>,
}

#[Object]
impl ContextWindow {
    /// Tokens held across every region: what the next request costs before the
    /// model's reply.
    async fn total_tokens(&self) -> i32 {
        count(self.snapshot.total_tokens)
    }

    /// The window's budget, from the blueprint or the model's own limit.
    async fn max_tokens(&self) -> i32 {
        count(self.snapshot.max_tokens)
    }

    /// The stage the run was in when this window was written.
    async fn stage_name(&self) -> &str {
        &self.snapshot.stage_name
    }

    /// Every region, in layout order.
    async fn regions(&self) -> Vec<ContextRegion> {
        (0..self.snapshot.regions.len())
            .map(|at| ContextRegion {
                snapshot: std::sync::Arc::clone(&self.snapshot),
                at,
            })
            .collect()
    }
}

/// Narrow a daemon counter to the 32 bits GraphQL's `Int` carries.
///
/// Saturating rather than wrapping: a counter that ran away should read as an
/// implausible ceiling, not as a small number that looks fine.
fn count(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
#[path = "run_detail_tests.rs"]
mod tests;
