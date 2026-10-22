//! A run's segments: the stretches of work between one message that started
//! or continued it and the end it reached.
//!
//! A run that finishes can be continued with a new message: it goes back to
//! a stage of its graph and works again, with the context its regions keep.
//! Each continuation starts a segment, recorded as a
//! [`RunEvent::Continued`](super::RunEvent::Continued) in the step that
//! reopened the run. The event carries how the segment before it ended (its
//! status, its answer, what the run had spent by then), so the run's state
//! only ever holds the segment in progress, and every earlier one is read
//! back from its file's history.

use serde::{Deserialize, Serialize};

use super::files::FileRef;
use super::{FinalOutputState, RunState, RunStatus, Spend};
use crate::spec::graph::OnContinue;
use crate::spec::names::{RunId, StageName};

/// A segment starting: a finished run continued in place, or a new run forked
/// from a finished one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContinuedState {
    /// The segment that starts, counted from 1.
    pub segment: u32,
    /// The stage it starts in.
    pub stage: StageName,
    /// The run this one was forked from, for a fork's first segment. `None`
    /// for a run continued in place.
    pub forked_from: Option<RunId>,
    /// How the segment before it ended. `None` for a fork's first segment,
    /// which has none.
    pub previous: Option<SegmentEnd>,
    /// What each region of the window did, by name, in window order.
    pub regions: Vec<(String, OnContinue)>,
}

/// How a segment ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SegmentEnd {
    /// Its status: complete, failed or cancelled.
    pub status: RunStatus,
    /// The stage it ended in.
    pub stage: StageName,
    /// What the run had spent by then, every segment so far included.
    pub spend: Spend,
    /// The tool calls the run had made by then.
    pub tool_calls: u64,
    /// The answer it handed back, when it handed one back.
    pub final_output: Option<FinalOutputState>,
    /// The answer's bytes, kept beside the run file under
    /// `segments/<segment>/` so the next segment's answer does not replace
    /// them.
    pub file: Option<FileRef>,
    /// The last step it recorded.
    pub seq: u64,
}

impl SegmentEnd {
    /// How the run in `state` ended, with its answer's bytes kept at `file`.
    pub fn of(state: &RunState, file: Option<FileRef>) -> Self {
        Self {
            status: state.status.clone(),
            stage: state.cursor.stage.clone(),
            spend: state.totals.spend.clone(),
            tool_calls: state.totals.tool_calls,
            final_output: state.final_output.clone(),
            file,
            seq: state.seq,
        }
    }
}

/// Where segment `segment`'s answer is kept once the run is continued,
/// relative to the run's directory.
pub fn kept_answer_path(segment: u32) -> String {
    format!("segments/{segment}/{}", leviath_core::FINAL_OUTPUT_FILE)
}

/// What one spend took beyond another: `now` less `before`, field by field,
/// never below zero. A segment's own spend is the run's spend at its end less
/// the run's spend when it started.
pub fn spent_since(now: &Spend, before: &Spend) -> Spend {
    Spend {
        prompt_tokens: now.prompt_tokens.saturating_sub(before.prompt_tokens),
        completion_tokens: now
            .completion_tokens
            .saturating_sub(before.completion_tokens),
        cached_tokens: now.cached_tokens.saturating_sub(before.cached_tokens),
        cache_write_tokens: now
            .cache_write_tokens
            .saturating_sub(before.cache_write_tokens),
        priced_usd: (now.priced_usd - before.priced_usd).max(0.0),
        reported_calls: now.reported_calls.saturating_sub(before.reported_calls),
        computed_calls: now.computed_calls.saturating_sub(before.computed_calls),
        unpriced_calls: now.unpriced_calls.saturating_sub(before.unpriced_calls),
        cost_unknown: now.cost_unknown,
    }
}

#[cfg(test)]
#[path = "segment_tests.rs"]
mod tests;
