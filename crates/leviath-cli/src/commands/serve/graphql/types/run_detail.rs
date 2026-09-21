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
    /// Waiting on an answer to a question the run asked.
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

impl WaitReasonKind {
    /// The kind behind one of the daemon's own reasons.
    ///
    /// Separate from the whole-reason conversion below because the run filter
    /// matches on the kind alone, and one exhaustive match is what keeps the
    /// two vocabularies from drifting.
    pub(crate) fn of(reason: &leviath_core::run_meta::WaitReason) -> Self {
        use leviath_core::run_meta::WaitReason as Core;
        match reason {
            Core::ToolApproval => Self::ToolApproval,
            Core::UserPrompt => Self::UserPrompt,
            Core::TaintGate => Self::TaintGate,
            Core::InteractionPoint => Self::InteractionPoint,
            Core::FanOutWorkers { .. } => Self::FanOutWorkers,
            Core::Children { .. } => Self::Children,
            Core::NeedsSetup { .. } => Self::NeedsSetup,
        }
    }
}

impl From<&leviath_core::run_meta::WaitReason> for WaitReason {
    fn from(reason: &leviath_core::run_meta::WaitReason) -> Self {
        use leviath_core::run_meta::WaitReason as Core;
        let plain = Self::plain(WaitReasonKind::of(reason), reason.needs_a_person());
        match reason {
            Core::ToolApproval | Core::UserPrompt | Core::TaintGate | Core::InteractionPoint => {
                plain
            }
            Core::FanOutWorkers { outstanding } | Core::Children { outstanding } => Self {
                outstanding: Some(i32::try_from(*outstanding).unwrap_or(i32::MAX)),
                ..plain
            },
            Core::NeedsSetup { blocker, remedy } => Self {
                blocker: Some(blocker.into()),
                remedy: Some(remedy.clone()),
                ..plain
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
    /// This visit's own id, as the ledger recorded it.
    ///
    /// The correlation key for everything that happened during the stay. Null on
    /// a record written before visits had identity, where a visit was identified
    /// by its position in the stage's list and so changed identity as soon as
    /// that list was capped.
    pub(crate) id: Option<String>,
    /// Which entry into this stage this is, counting from one. The same number
    /// the `stageTransition` frame carries as `iteration`.
    pub(crate) ordinal: i32,
    /// When the run entered, unix epoch seconds.
    pub(crate) entered_at: Timestamp,
    /// When it left; null for the visit in progress.
    pub(crate) left_at: Option<Timestamp>,
    /// Whether this is the visit in progress. The same fact as `leftAt` being
    /// null, said the way a list is filtered on. Not a clock: the `active` on
    /// a run and on a stage is how long each has been working.
    pub(crate) in_progress: bool,
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

/// One provider and model a stage ran an inference on.
///
/// Both halves, because neither identifies what ran on its own: one provider
/// serves many models, and one model is spelled differently by each provider
/// that routes to it.
#[derive(Debug, SimpleObject)]
pub(crate) struct StageModelUse {
    /// The registered provider that served the call.
    pub(crate) provider: String,
    /// The model the call named, spelled as that provider spells it.
    pub(crate) model: String,
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
    /// What this stage actually ran on, in the order it first reached each
    /// entry.
    ///
    /// A list because a stage that fails over runs on more than one: the first
    /// entry is where it started, the last is where it ended up, and one entry
    /// means it never moved. Each pair appears once however many calls it
    /// served, so this is what ran and not how often.
    ///
    /// Null when the stage has run no inference at all - one the run never
    /// entered, one whose first call has not come back, one whose provider
    /// could not be reached, and every stage of a run that finished before
    /// Leviath recorded this. Choosing a model is not running on one, so there
    /// is nothing here to read as the stage's intended model.
    pub(crate) models: Option<Vec<StageModelUse>>,
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
            // An empty list would read as "this stage ran on nothing", which is
            // a claim; null is the absence of the answer, which is the truth
            // for a stage that has not run and for a run recorded before this
            // was kept.
            models: match record.models.is_empty() {
                true => None,
                false => Some(
                    record
                        .models
                        .iter()
                        .map(|used| StageModelUse {
                            provider: used.provider.clone(),
                            model: used.model.clone(),
                        })
                        .collect(),
                ),
            },
            visit_count: count(record.visit_count),
            visits: record
                .visits
                .iter()
                .enumerate()
                .map(|(at, visit)| StageVisit {
                    id: Some(visit.id.clone()).filter(|id| !id.is_empty()),
                    ordinal: count(at + 1),
                    entered_at: Timestamp(visit.entered_at),
                    left_at: visit.left_at.map(Timestamp),
                    in_progress: visit.left_at.is_none(),
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

/// The resolver state behind the `ContextRegion` type.
pub(crate) struct ContextRegion {
    /// The snapshot this region came from, shared rather than copied.
    pub(crate) snapshot: std::sync::Arc<leviath_core::run_meta::ContextSnapshot>,
    /// Which region, by position in the window.
    pub(crate) at: usize,
}

/// One region of a run's live context window: its name, its budget, and the
/// text it is holding.
///
/// The declared region is on the blueprint; this is what that region holds
/// right now. Keeping them apart is what lets a client read either without the
/// other. `content` is the expensive field, so select it only for the regions
/// you are going to show.
#[Object]
impl ContextRegion {
    /// The region's name, matching the blueprint's declaration.
    async fn name(&self) -> &str {
        &self.region().name
    }

    /// What the region does when it fills, as the snapshot recorded it.
    ///
    /// Null for a kind this build does not have a name for, which is a run
    /// written by a newer one.
    async fn kind(&self) -> Option<super::blueprint::RegionKind> {
        super::blueprint::RegionKind::from_snapshot(&self.region().kind)
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

/// The resolver state behind the `ContextWindow` type.
pub(crate) struct ContextWindow {
    /// The snapshot read from the run's directory.
    pub(crate) snapshot: std::sync::Arc<leviath_core::run_meta::ContextSnapshot>,
}

/// Everything a run is holding in mind, region by region, as of one moment.
///
/// A window is a reading rather than a live handle: the run keeps working and
/// what it knows keeps changing, so every window carries a `revision` naming
/// exactly the contents it was read at. Hold that revision to fetch the same
/// window again, or read a fresh one to see where the run has got to.
#[Object]
impl ContextWindow {
    /// This window's revision: a content address of what it holds.
    ///
    /// Derived from the window's contents and budgets, so it names one window for
    /// ever. Two guarantees follow, and the whole point of the field is that you
    /// may rely on both. A revision you hold always means the same content: no
    /// later write can change what it refers to, because a write produces a
    /// *different* revision. And `contextSnapshot(revision:)` resolves one to
    /// exactly that content, never to whatever the run holds now.
    ///
    /// It is the same value `ContextChange.revisionBefore` and `revisionAfter`
    /// carry, so a change joins to the windows either side of it.
    ///
    /// Two points holding identical contents share a revision - that is what
    /// content addressing means. The stage the run was in is not part of it:
    /// `stageName` and `ContextSnapshotPoint.at` say where and when.
    async fn revision(&self) -> String {
        leviath_core::run_meta::revision::context_revision(&self.snapshot)
    }

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

/// One stored part a run holds.
///
/// The bytes are not here. They are behind `url`, which is a short-lived signed
/// link the byte route verifies: bytes never ride a query answer, and a page can
/// put that link straight in an `<img src>`.
#[derive(Debug, SimpleObject)]
pub(crate) struct BlobEntry {
    /// The store's key: the bytes' SHA-256.
    pub(crate) sha256: String,
    /// The type the bytes were stored as.
    pub(crate) mime_type: String,
    /// The name the part carries, when its context gave it one.
    pub(crate) name: Option<String>,
    /// Size in bytes.
    pub(crate) size: BigInt,
    /// Pixel width, for an image or a video.
    pub(crate) width: Option<i32>,
    /// Pixel height, for an image or a video.
    pub(crate) height: Option<i32>,
    /// Duration in milliseconds, for audio or video.
    pub(crate) duration_ms: Option<i32>,
    /// What the part is budgeted at in context.
    pub(crate) tokens: i32,
    /// Every region holding an entry that names this part.
    pub(crate) regions: Vec<String>,
    /// Whether the bytes are still on disk. A context can name a part whose
    /// file was too large to keep, or that a pruned run directory lost.
    pub(crate) stored: bool,
    /// A short-lived signed link to the bytes. Null when they are not on disk.
    pub(crate) url: Option<String>,
}

/// One file a run handed back beside its answer.
#[derive(Debug, SimpleObject)]
pub(crate) struct Artifact {
    /// What the submission called it.
    pub(crate) name: String,
    /// Its declared mime type.
    pub(crate) mime_type: String,
    /// Size in bytes, when the record has it.
    pub(crate) size: Option<BigInt>,
    /// Content hash, when it was computed.
    pub(crate) sha256: Option<String>,
    /// Where the run wrote it, relative to its working directory. What the run
    /// recorded, so it can name a file the run has since replaced or removed: the
    /// link is what fetches the bytes that were handed back.
    pub(crate) path: String,
    /// A short-lived signed link to the bytes.
    pub(crate) url: String,
}

/// One recorded artifact, with a signed link to its bytes.
///
/// Shared by the run's own list and by the execution that produced it, so the two
/// describe one file the same way and mint the same kind of link for it.
pub(crate) fn artifact(
    state: &crate::commands::serve::AppState,
    run_id: &str,
    artifact: &leviath_core::output::Artifact,
) -> Artifact {
    Artifact {
        url: super::super::super::signed_url::signed_path(
            &state.signer,
            &format!("/api/agents/{run_id}/artifacts/{}", artifact.name),
            &[],
            leviath_core::duration::now_secs(),
        ),
        name: artifact.name.clone(),
        mime_type: artifact.mime_type.to_string(),
        size: Some(BigInt(artifact.size as i64)),
        sha256: Some(artifact.sha256.clone()).filter(|hash| !hash.is_empty()),
        path: artifact.path.clone(),
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
