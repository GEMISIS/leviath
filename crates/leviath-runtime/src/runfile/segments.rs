//! A run's segments, read back from its file.
//!
//! The state holds only the segment in progress; every earlier one ended in
//! a step whose successor records a
//! [`RunEvent::Continued`] carrying how it
//! ended. So the list is the history's `Continued` events, oldest first, with
//! the run as it stands now at the end.

use super::error::RunFileError;
use super::reader::RunFileReader;
use crate::spec::names::{RunId, StageName};
use crate::state::files::FileRef;
use crate::state::segment::{ContinuedState, spent_since};
use crate::state::{FinalOutputState, RunEvent, RunState, RunStatus, Spend};

/// One segment of a run.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// Its number, counted from 1.
    pub index: u32,
    /// The stage it started in.
    pub stage: StageName,
    /// The step it started at: 0 for a run's first segment, and the step that
    /// reopened the run for every other.
    pub started_seq: u64,
    /// When it started, in unix seconds.
    pub started_at: i64,
    /// The run it was forked from, for a fork's first segment.
    pub forked_from: Option<RunId>,
    /// How it stands: still going for the last segment of a live run, and
    /// complete, failed or cancelled for one that ended.
    pub status: RunStatus,
    /// What it spent itself, earlier segments left out.
    pub spend: Spend,
    /// The tool calls it made itself.
    pub tool_calls: u64,
    /// The answer it handed back, when it handed one back.
    pub final_output: Option<FinalOutputState>,
    /// Where that answer's bytes are, relative to the run's directory.
    pub file: Option<FileRef>,
    /// The last step it recorded: the run's last step for the current one.
    pub last_seq: u64,
    /// Whether it is the segment the run is in now.
    pub current: bool,
}

/// Every segment of the run in `reader`, oldest first. A run never continued
/// has one.
pub fn segments(reader: &RunFileReader) -> Result<Vec<Segment>, RunFileError> {
    let mut starts: Vec<(u64, i64, ContinuedState)> = Vec::new();
    for delta in reader.delta_iter(1, reader.last_seq()) {
        let delta = delta?;
        for event in delta.events {
            if let RunEvent::Continued(continued) = event {
                starts.push((delta.seq, delta.at, *continued));
            }
        }
    }
    let first = reader.state_at(0)?;
    let now = reader.latest_state()?;
    Ok(build(reader.spec().created_at, &first, &now, starts))
}

/// The segments of a run that started in `first` at `created_at`, stands at
/// `now`, and was continued at each of `starts` (its step, when, and what the
/// step recorded).
pub(crate) fn build(
    created_at: i64,
    first: &RunState,
    now: &RunState,
    starts: Vec<(u64, i64, ContinuedState)>,
) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    // A fork's history starts with the context it carried over, which is no
    // segment of its own: its first segment is the one its first step opened.
    let forked = starts
        .first()
        .is_some_and(|(_, _, c)| c.forked_from.is_some());
    if !forked {
        out.push(open(1, first.cursor.stage.clone(), 0, created_at, None));
    }
    let mut spent_before = Spend::default();
    let mut calls_before = 0;
    for (seq, at, continued) in starts {
        if let (Some(last), Some(end)) = (out.last_mut(), &continued.previous) {
            last.status = end.status.clone();
            last.spend = spent_since(&end.spend, &spent_before);
            last.tool_calls = end.tool_calls.saturating_sub(calls_before);
            last.final_output = end.final_output.clone();
            last.file = end.file.clone();
            last.last_seq = end.seq;
            last.current = false;
            spent_before = end.spend.clone();
            calls_before = end.tool_calls;
        }
        out.push(open(
            continued.segment,
            continued.stage,
            seq,
            at,
            continued.forked_from,
        ));
    }
    // A run has a first segment, or a fork's continuation opened one.
    let last = out.last_mut().expect("a run has at least one segment");
    last.status = now.status.clone();
    last.spend = spent_since(&now.totals.spend, &spent_before);
    last.tool_calls = now.totals.tool_calls.saturating_sub(calls_before);
    last.final_output = now.final_output.clone();
    last.file = now.files.final_output.clone();
    last.last_seq = now.seq;
    out
}

/// A segment that has just started.
fn open(
    index: u32,
    stage: StageName,
    started_seq: u64,
    started_at: i64,
    forked_from: Option<RunId>,
) -> Segment {
    Segment {
        index,
        stage,
        started_seq,
        started_at,
        forked_from,
        status: RunStatus::Active,
        spend: Spend::default(),
        tool_calls: 0,
        final_output: None,
        file: None,
        last_seq: started_seq,
        current: true,
    }
}

#[cfg(test)]
#[path = "segments_tests.rs"]
mod tests;
