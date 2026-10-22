//! `RunSegment`: one stretch of a run's work, between the message that
//! started or continued it and the end it reached.

use std::sync::Arc;

use async_graphql::{ID, Object};
use leviath_graphql_derive::mirror;

use super::super::super::super::blocking::blocking;
use super::super::super::super::core::segments::{self as core, SegmentRead};
use super::super::super::error::IntoGraphql;
use super::super::super::scalars::{BigInt, Decimal, Timestamp};
use super::super::run_detail::FinalOutput;
use super::RunStatus;
use super::support::{CostBreakdown, TokenUsage, as_i32};
use crate::runstate::RunMeta;

/// The resolver state behind `RunSegment`: the segment and its answer.
pub(crate) struct RunSegment(SegmentRead);

/// One segment of a run.
///
/// A run that finishes can be continued with a new message: it goes back to
/// work, and what it does from then on is a new segment, with its own spend,
/// answer and artifacts. A run never continued has one segment, and the run's
/// own fields always describe the latest.
#[mirror(no_filter)]
#[Object]
impl RunSegment {
    /// Its number, counted from 1.
    async fn index(&self) -> i32 {
        i32::try_from(self.0.segment.index).unwrap_or(i32::MAX)
    }

    /// Whether it is the segment the run is in now: the last one.
    async fn current(&self) -> bool {
        self.0.segment.current
    }

    /// The stage it started in.
    async fn stage(&self) -> &str {
        self.0.segment.stage.as_str()
    }

    /// When it started.
    async fn started_at(&self) -> Timestamp {
        Timestamp(self.0.segment.started_at)
    }

    /// The step of the run file it started at: 0 for the first segment of a
    /// run, the step that reopened the run for every other.
    async fn started_seq(&self) -> BigInt {
        BigInt(i64::try_from(self.0.segment.started_seq).unwrap_or(i64::MAX))
    }

    /// The last step it recorded.
    async fn last_seq(&self) -> BigInt {
        BigInt(i64::try_from(self.0.segment.last_seq).unwrap_or(i64::MAX))
    }

    /// How it stands: the run's status for the current segment, and how it
    /// ended for every earlier one.
    async fn status(&self) -> RunStatus {
        use leviath_core::run_meta::RunStatus as Core;
        use leviath_runtime::state::RunStatus as State;
        RunStatus::from(&match &self.0.segment.status {
            State::Idle => Core::Starting,
            State::Active => Core::Running,
            State::Waiting => Core::WaitingInput,
            State::Paused => Core::Paused,
            State::Complete => Core::Complete,
            State::Error(_) => Core::Error,
            State::Cancelled => Core::Cancelled,
        })
    }

    /// The run it was forked from, for the first segment of a fork. Null for
    /// every other.
    async fn forked_from(&self) -> Option<ID> {
        self.0
            .segment
            .forked_from
            .as_ref()
            .map(|r| ID(r.to_string()))
    }

    /// The tokens it used itself, earlier segments left out.
    async fn usage(&self) -> TokenUsage {
        let spend = &self.0.segment.spend;
        let big = |n: u64| BigInt(i64::try_from(n).unwrap_or(i64::MAX));
        TokenUsage {
            prompt_tokens: big(spend.prompt_tokens),
            completion_tokens: big(spend.completion_tokens),
            cached_tokens: big(spend.cached_tokens),
            cache_write_tokens: big(spend.cache_write_tokens),
        }
    }

    /// What it cost itself, earlier segments left out.
    async fn cost(&self) -> CostBreakdown {
        let spend = &self.0.segment.spend;
        let (cost_usd, cost_is_exact) = leviath_runtime::insert::place::cost(spend);
        CostBreakdown {
            cost_usd: cost_usd.map(Decimal),
            cost_priced_usd: Decimal(spend.priced_usd),
            cost_is_exact,
            unpriced_calls: as_i32(spend.unpriced_calls as usize),
        }
    }

    /// The tool calls it made itself.
    async fn tool_call_count(&self) -> i32 {
        i32::try_from(self.0.segment.tool_calls).unwrap_or(i32::MAX)
    }

    /// The answer it handed back. Null when it handed back none.
    async fn final_output(&self) -> Option<FinalOutput> {
        let out = self.0.segment.final_output.as_ref()?;
        Some(FinalOutput {
            content: self.0.answer.clone()?,
            format: out.format.clone(),
            stage: out.stage.to_string(),
            submitted_at: Timestamp(out.submitted_at),
            truncated: out.truncated,
        })
    }

    /// The files it handed back with its answer, by name. Empty when it
    /// handed back none.
    async fn artifacts(&self) -> Vec<String> {
        self.0
            .segment
            .final_output
            .iter()
            .flat_map(|out| &out.artifacts)
            .map(|a| a.name.clone())
            .collect()
    }
}

/// Every segment of the run `run_id`, oldest first.
pub(super) async fn read(run_id: &str) -> async_graphql::Result<Vec<RunSegment>> {
    let id = run_id.to_string();
    let read = blocking(move || core::segments(&id)).await.gql()?;
    Ok(read.into_iter().map(RunSegment).collect())
}

/// Whether the run `meta` describes can be continued. Null when its file
/// does not say.
pub(super) async fn continuable(meta: &Arc<RunMeta>) -> Option<bool> {
    let meta = Arc::clone(meta);
    blocking(move || core::continuable(&meta)).await
}

#[cfg(test)]
#[path = "segments_tests.rs"]
mod tests;
