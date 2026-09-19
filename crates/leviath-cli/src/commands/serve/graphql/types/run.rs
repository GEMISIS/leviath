//! `Run`: one agent run, and the values that hang off it.
//!
//! A run is read from the shared index, so the object below holds the same
//! `Arc<RunMeta>` the REST listing holds: a page of fifty is fifty pointer
//! copies, not fifty parses. Fields that only need what is already in memory
//! resolve without touching the disk, which is what makes a selection set the
//! cheaper way to ask.

use std::sync::Arc;

use async_graphql::{Context, Enum, Object, SimpleObject};

use super::super::super::blocking::blocking;
use super::super::super::core::blueprints;
use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::scalars::{BigInt, Decimal, Timestamp};
use super::blueprint::Blueprint;
use crate::runstate::RunMeta;

/// The lifecycle states a run moves through.
///
/// One state per variant of the daemon's own `RunStatus`, so the two cannot
/// drift: the conversion below is exhaustive and a new daemon state will not
/// compile until it is named here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum RunStatus {
    /// Spawned but not yet running.
    Starting,
    /// Moving: inferring, calling tools, transitioning.
    Running,
    /// Parked: on a prompt somebody has to answer, or holding for children.
    WaitingInput,
    /// Paused by `lev pause`; resumes with `lev resume`.
    Paused,
    /// Finished with an answer or a terminal state.
    Complete,
    /// Every required stage finished but the agent still accepts messages.
    CompleteInteractive,
    /// Unrecoverable failure; `Run.error` carries what went wrong.
    Error,
    /// Stopped from outside. Nothing went wrong; somebody decided.
    Cancelled,
}

impl From<&leviath_core::run_meta::RunStatus> for RunStatus {
    fn from(status: &leviath_core::run_meta::RunStatus) -> Self {
        use leviath_core::run_meta::RunStatus as Daemon;
        match status {
            Daemon::Starting => Self::Starting,
            Daemon::Running => Self::Running,
            Daemon::WaitingInput => Self::WaitingInput,
            Daemon::Paused => Self::Paused,
            Daemon::Complete => Self::Complete,
            Daemon::CompleteInteractive => Self::CompleteInteractive,
            Daemon::Error => Self::Error,
            Daemon::Cancelled => Self::Cancelled,
        }
    }
}

impl RunStatus {
    /// The daemon's own spelling of this state.
    ///
    /// The filters read `meta.json`, which stores these words, so a GraphQL
    /// enum value has to become one before it can filter anything. Going
    /// through the daemon's own `wire()` keeps one spelling for one state
    /// across both surfaces.
    pub(crate) fn wire(self) -> &'static str {
        use leviath_core::run_meta::RunStatus as Daemon;
        match self {
            Self::Starting => Daemon::Starting.wire(),
            Self::Running => Daemon::Running.wire(),
            Self::WaitingInput => Daemon::WaitingInput.wire(),
            Self::Paused => Daemon::Paused.wire(),
            Self::Complete => Daemon::Complete.wire(),
            Self::CompleteInteractive => Daemon::CompleteInteractive.wire(),
            Self::Error => Daemon::Error.wire(),
            Self::Cancelled => Daemon::Cancelled.wire(),
        }
    }
}

/// Token counts for a run, a stage, or a subtree roll-up.
#[derive(Debug, SimpleObject)]
pub(crate) struct TokenUsage {
    /// Input tokens, cached ones included.
    pub(crate) prompt_tokens: BigInt,
    /// Output tokens.
    pub(crate) completion_tokens: BigInt,
    /// Counted within `promptTokens`, not on top of it. Do not add them twice.
    pub(crate) cached_tokens: BigInt,
    /// Tokens written to the provider's cache.
    pub(crate) cache_write_tokens: BigInt,
}

/// Spend for a run, a stage, or a subtree roll-up.
#[derive(Debug, SimpleObject)]
pub(crate) struct CostBreakdown {
    /// Null is unknown, never free: some call in the run went unpriced.
    pub(crate) cost_usd: Option<Decimal>,
    /// The priced subtotal, kept even while `costUsd` is null.
    pub(crate) cost_priced_usd: Decimal,
    /// True when every priced call carried the provider's own figure.
    pub(crate) cost_is_exact: bool,
    /// Calls no provider priced.
    pub(crate) unpriced_calls: i32,
}

/// Elapsed working time: banked spans plus the one in progress.
#[derive(Debug, SimpleObject)]
pub(crate) struct WorkingClock {
    /// Seconds banked by spans that have ended.
    pub(crate) banked_secs: i32,
    /// When the span in progress began; null while the clock is stopped.
    pub(crate) since: Option<Timestamp>,
}

/// One caller-supplied metadata entry on a run.
#[derive(Debug, SimpleObject)]
pub(crate) struct MetadataEntry {
    /// The key.
    pub(crate) key: String,
    /// The value. Always a string.
    pub(crate) value: String,
}

/// One agent run.
///
/// Holds the shared `RunMeta` and the wall-clock second the request was
/// answered at, so every duration in one response is measured from one
/// instant rather than drifting field by field.
pub(crate) struct Run {
    /// The run's record, shared with the index rather than copied.
    pub(crate) meta: Arc<RunMeta>,
    /// Daemon time when this request was answered.
    pub(crate) now: i64,
}

#[Object]
impl Run {
    /// Globally unique run id.
    async fn id(&self) -> &str {
        &self.meta.run_id
    }

    /// The agent name from the blueprint this run was spawned from.
    async fn agent_name(&self) -> &str {
        &self.meta.agent_name
    }

    /// Human title, set by the titling pass; null until it lands.
    async fn title(&self) -> Option<&str> {
        self.meta.title.as_deref()
    }

    /// Why titling failed, when it did.
    async fn title_error(&self) -> Option<&str> {
        self.meta.title_error.as_deref()
    }

    /// The lifecycle state: the filter and subscription vocabulary.
    async fn status(&self) -> RunStatus {
        RunStatus::from(&self.meta.status)
    }

    /// Present only when the status is `ERROR`.
    async fn error(&self) -> Option<&str> {
        self.meta.error.as_deref()
    }

    /// The initial ask this run was given.
    async fn task(&self) -> &str {
        &self.meta.task
    }

    /// Inference turns in the current stage, reset on entering a new one.
    async fn iteration(&self) -> i32 {
        as_i32(self.meta.iteration)
    }

    /// Tool calls across the whole run.
    async fn tool_calls(&self) -> i32 {
        as_i32(self.meta.tool_calls)
    }

    /// Unix epoch seconds, as the daemon stores it.
    async fn started_at(&self) -> Timestamp {
        Timestamp(self.meta.started_at)
    }

    /// Last state change, unix epoch seconds.
    async fn updated_at(&self) -> Timestamp {
        Timestamp(self.meta.updated_at)
    }

    /// When the run last actually moved. Age a wedged run against this.
    async fn last_progress_at(&self) -> Option<Timestamp> {
        self.meta.last_progress_at.map(Timestamp)
    }

    /// Seconds since `startedAt`, wall-clock.
    async fn age_secs(&self) -> BigInt {
        BigInt(self.meta.age_secs(self.now) as i64)
    }

    /// Seconds actually working, parked time excluded.
    async fn working_secs(&self) -> BigInt {
        BigInt(self.meta.active_runtime_secs(self.now) as i64)
    }

    /// The working span in progress; null while the clock is stopped.
    async fn active(&self) -> Option<WorkingClock> {
        self.meta.active.map(|clock| WorkingClock {
            banked_secs: as_i32(clock.banked_secs as usize),
            since: clock.since.map(Timestamp),
        })
    }

    /// Token roll-up for the whole run.
    async fn usage(&self) -> TokenUsage {
        TokenUsage {
            prompt_tokens: BigInt(self.meta.prompt_tokens as i64),
            completion_tokens: BigInt(self.meta.completion_tokens as i64),
            cached_tokens: BigInt(self.meta.cached_tokens as i64),
            cache_write_tokens: BigInt(self.meta.cache_write_tokens as i64),
        }
    }

    /// Spend roll-up for the whole run.
    async fn cost(&self) -> CostBreakdown {
        CostBreakdown {
            cost_usd: self.meta.cost_usd.map(Decimal),
            cost_priced_usd: Decimal(self.meta.cost_priced_usd),
            cost_is_exact: self.meta.cost_is_exact,
            unpriced_calls: as_i32(self.meta.unpriced_calls),
        }
    }

    /// Whether the run was spawned unattended, so approvals resolve without
    /// a person.
    async fn unattended(&self) -> bool {
        self.meta.yolo
    }

    /// The yolo profile this run was spawned with, when it named one.
    async fn yolo_profile_name(&self) -> Option<&str> {
        self.meta.yolo_profile.as_deref()
    }

    /// The working directory the run executes in.
    async fn workdir(&self) -> &str {
        &self.meta.workdir
    }

    /// The run that spawned this one; null for a top-level run.
    async fn parent_id(&self) -> Option<&str> {
        self.meta.parent_run_id.as_deref()
    }

    /// The blueprint this run executed.
    ///
    /// The run's own snapshot of the manifest, taken at spawn, so it answers
    /// for the run even after the installed blueprint is edited or deleted.
    /// For a run recorded before snapshots existed there is no copy, and this
    /// falls back to the installed file: `blueprint.source` says which, and
    /// `blueprintDigest` is set only for a run that carries its own.
    ///
    /// Null, with an error naming the file, when neither can be read. Nullable
    /// on purpose: one unreadable blueprint in a page of fifty runs must not
    /// cost a client the other forty-nine.
    async fn blueprint(&self, ctx: &Context<'_>) -> async_graphql::Result<Option<Blueprint>> {
        let state = ctx.data_unchecked::<AppState>();
        let meta = Arc::clone(&self.meta);
        // One `meta.json`-sized read, off the async runtime: a selection set
        // that asks fifty runs for their blueprints is fifty small reads, and
        // the parse behind them is shared by digest.
        let manifest = blocking(move || {
            blueprints::manifest_for_run(&blueprints::run_dir(&meta.run_id), &meta)
        })
        .await
        .gql()?;
        let parsed = state.caches.blueprints.parse(&manifest).gql()?;
        Ok(Some(Blueprint {
            parsed,
            digest: manifest.digest,
            source: manifest.source.into(),
        }))
    }

    /// The digest of the manifest this run executed, lowercase hex SHA-256.
    ///
    /// Recorded at spawn. Compare it with the installed blueprint's digest to
    /// tell "this run executed what is installed now" from "this run executed
    /// something else". Null for a run recorded before snapshots existed,
    /// where the answer is unknown rather than "the same".
    async fn blueprint_digest(&self) -> Option<&str> {
        self.meta.blueprint_digest.as_deref()
    }

    /// Caller-supplied metadata from spawn. Values are always strings.
    ///
    /// Sorted by key: the daemon keeps these in a hash map, and a listing
    /// whose order changes between two identical requests is a diff nobody
    /// can read.
    async fn metadata(&self) -> Vec<MetadataEntry> {
        let mut entries: Vec<MetadataEntry> = self
            .meta
            .metadata
            .iter()
            .map(|(key, value)| MetadataEntry {
                key: key.clone(),
                value: value.clone(),
            })
            .collect();
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        entries
    }
}

/// Narrow a daemon counter to the 32 bits GraphQL's `Int` carries.
///
/// These are iteration and tool-call counters, which a run reaches in the
/// thousands at most. Saturating rather than wrapping: if one ever did run
/// away, a client should read an implausible ceiling rather than a small
/// number that looks fine.
fn as_i32(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
