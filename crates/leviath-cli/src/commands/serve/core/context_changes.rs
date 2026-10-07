//! Why a run's context window changed, read whole.
//!
//! The snapshots say what every region held at each point; they cannot say what
//! moved it. A run file records each change to the window as part of a step,
//! beside the events of that step, and the events are what name the cause: a
//! step that delivered a message changed the window by the message, a step in
//! which a tool finished by its result, and so on.
//!
//! A run file keeps each change with its cause as an event of the step it
//! landed in: the window revisions on either side, the regions it moved and
//! the execution it came from. A file written without those (one converted
//! from an older layout) has its causes read off the step's other events, and
//! a step whose events name no cause, or name more than one, is left out. A
//! history that mislabels a change is worse than one that admits it does not
//! know, which is the rule [`ContextCause`] is written to.

use std::ops::ControlFlow;

use leviath_core::context_cause::ContextCause;
use leviath_runtime::runfile::history::{ContextChangeRecord, IndexedChange, RegionTransition};
use leviath_runtime::runfile::record::RegionCommit;
use leviath_runtime::spec::run_spec::RunSpec;
use leviath_runtime::state::context::RegionChange;
use leviath_runtime::state::journal::CauseState;
use leviath_runtime::state::{Change, ContextDiff, ContextState, RunEvent, RunState};

use super::error::ServeError;
use super::run_file::{self, Pair};

/// Largest page of changes the GraphQL listing takes.
///
/// The same cap as the run listing and the interactions listing. It is well
/// above the history listing's, because a change carries no window: a page of
/// these is six small fields apiece.
pub(crate) const CONTEXT_CHANGES_MAX_LIMIT: usize = 200;

/// Every change one execution committed, in the order they landed.
///
/// Empty for an execution that committed none, which most are, and for an empty
/// id: an id that names nothing matches nothing rather than matching every change
/// that recorded no execution.
pub(crate) fn by_execution(
    run_id: &str,
    execution_id: &str,
) -> Result<Vec<IndexedChange>, ServeError> {
    if execution_id.is_empty() {
        return Ok(Vec::new());
    }
    Ok(read(run_id)?
        .into_iter()
        .filter(|change| change.record.execution_id.as_deref() == Some(execution_id))
        .collect())
}

/// Every change a run's file records a cause for, in the order they landed.
/// Each one's position is its step.
///
/// A run with no run file is not an error here, and neither is one that names
/// no causes: a run whose writes all went through steps that cannot name one
/// has nothing to report, and an empty list says so.
pub(crate) fn read(run_id: &str) -> Result<Vec<IndexedChange>, ServeError> {
    let Some(reader) = run_file::open(run_id)? else {
        return Ok(Vec::new());
    };
    let spec = reader.spec();
    let mut changes = Vec::new();
    let mut kept = Vec::new();
    run_file::walk_pairs(run_id, &reader, &mut |step| {
        let position = step.delta.seq;
        kept.extend(
            step.delta
                .events
                .iter()
                .filter_map(|event| recorded(event, step.delta.at))
                .map(|record| IndexedChange { position, record }),
        );
        let diff = step.delta.changes.iter().find_map(|change| match change {
            Change::Context(diff) => Some(diff),
            _ => None,
        });
        let found = diff.and_then(|diff| change(spec, &step, diff));
        changes.extend(found.map(|record| IndexedChange { position, record }));
        ControlFlow::Continue(())
    })?;
    if !kept.is_empty() {
        return Ok(kept);
    }
    Ok(changes)
}

/// What caused a change, as the journal names it.
fn cause(c: CauseState) -> ContextCause {
    match c {
        CauseState::Seed => ContextCause::Seed,
        CauseState::Message => ContextCause::Message,
        CauseState::ModelReply => ContextCause::ModelReply,
        CauseState::ToolResult => ContextCause::ToolResult,
        CauseState::ProducedPart => ContextCause::ProducedPart,
        CauseState::Compaction => ContextCause::Compaction,
        CauseState::Transform => ContextCause::Transform,
        CauseState::ContextTool => ContextCause::ContextTool,
        CauseState::Hook => ContextCause::Hook,
        CauseState::FanOut => ContextCause::FanOut,
        CauseState::Interaction => ContextCause::Interaction,
        CauseState::Resume => ContextCause::Resume,
        CauseState::Framework => ContextCause::Framework,
    }
}

/// The change an event kept whole, at `at`.
fn recorded(event: &RunEvent, at: i64) -> Option<ContextChangeRecord> {
    match event {
        RunEvent::ContextCommitted(commit) => Some(ContextChangeRecord {
            cause: cause(commit.cause),
            revision_before: Some(commit.revision_before.clone()),
            revision_after: Some(commit.revision_after.clone()),
            execution_id: commit.execution_id.clone(),
            regions: commit
                .regions
                .iter()
                .map(|r| {
                    RegionTransition::from(RegionCommit {
                        region: r.region.clone(),
                        digest_before: r.digest_before.clone(),
                        digest_after: r.digest_after.clone(),
                        tokens_before: r.tokens_before as usize,
                        tokens_after: r.tokens_after as usize,
                        entries_before: r.entries_before as usize,
                        entries_after: r.entries_after as usize,
                        entries_added: r.entries_added as usize,
                    })
                })
                .collect(),
            at,
        }),
        // A change recorded one region at a time names neither the window it
        // moved nor what the region held, so a reader gets the counts it does
        // carry and nothing invented around them.
        RunEvent::ContextNoted(note) => Some(ContextChangeRecord {
            cause: cause(note.cause),
            revision_before: None,
            revision_after: None,
            execution_id: None,
            regions: vec![RegionTransition {
                region: note.region.clone(),
                digest_before: None,
                digest_after: None,
                tokens_before: None,
                tokens_after: None,
                token_delta: note.token_delta,
                entries_before: None,
                entries_after: None,
                entries_added: note.entries_added as usize,
                entries_removed: note.entries_removed as usize,
            }],
            at,
        }),
        _ => None,
    }
}

/// The change `diff` made in `step`, when the step names its cause and it
/// touched a region's entries.
fn change(spec: &RunSpec, step: &Pair<'_>, diff: &ContextDiff) -> Option<ContextChangeRecord> {
    let cause = cause_of(step)?;
    let before = &step.before.context;
    let after = &step.after.context;
    let mut regions: Vec<RegionTransition> = diff
        .regions
        .iter()
        .filter_map(|(name, _, entries)| {
            entries
                .as_ref()
                .map(|entries| transition(name.as_str(), before, after, entries))
        })
        .collect();
    regions.extend(
        diff.removed
            .iter()
            .map(|name| removal(name.as_str(), before)),
    );
    if regions.is_empty() {
        return None;
    }
    let finished: Vec<&str> = step
        .delta
        .events
        .iter()
        .filter_map(|e| match e {
            RunEvent::ToolFinished { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    Some(ContextChangeRecord {
        cause,
        revision_before: Some(revision(spec, step.before)),
        revision_after: Some(revision(spec, step.after)),
        execution_id: match finished.as_slice() {
            [one] => Some((*one).to_string()),
            _ => None,
        },
        regions,
        at: step.delta.at,
    })
}

/// The window's revision in `state`, as the history names it.
fn revision(spec: &RunSpec, state: &RunState) -> String {
    leviath_core::run_meta::revision::context_revision(&leviath_runtime::runfile::context_snapshot(
        spec, state,
    ))
}

/// The one cause the step's events name, if they name exactly one.
fn cause_of(step: &Pair<'_>) -> Option<ContextCause> {
    let mut causes: Vec<ContextCause> = step
        .delta
        .events
        .iter()
        .filter_map(|event| match event {
            RunEvent::Message(_) => Some(ContextCause::Message),
            RunEvent::Answered { .. } => Some(ContextCause::Interaction),
            RunEvent::ToolFinished { .. } => Some(ContextCause::ToolResult),
            RunEvent::Inference { .. } => Some(ContextCause::ModelReply),
            _ => None,
        })
        .collect();
    let moved = !step.delta.transitions().is_empty();
    if moved {
        causes.push(ContextCause::Transform);
    }
    let first = *causes.first()?;
    causes.iter().all(|c| *c == first).then_some(first)
}

/// How the region `name` moved, from what it held before to what it holds now.
fn transition(
    name: &str,
    before: &ContextState,
    after: &ContextState,
    entries: &RegionChange,
) -> RegionTransition {
    let was = before.region(name);
    let now = after.region(name);
    let tokens_before = was.map_or(0, |r| r.current_tokens as usize);
    let tokens_after = now.map_or(0, |r| r.current_tokens as usize);
    let entries_before = was.map_or(0, |r| r.entries.len());
    let entries_after = now.map_or(0, |r| r.entries.len());
    let entries_added = match entries {
        RegionChange::Append(added) => added.len(),
        RegionChange::Replace(all) => {
            let kept = was.map_or(0, |r| {
                r.entries
                    .iter()
                    .zip(all)
                    .take_while(|(old, new)| old == new)
                    .count()
            });
            all.len() - kept
        }
    };
    RegionTransition {
        region: name.to_string(),
        digest_before: None,
        digest_after: None,
        tokens_before: Some(tokens_before),
        tokens_after: Some(tokens_after),
        token_delta: tokens_after as i64 - tokens_before as i64,
        entries_before: Some(entries_before),
        entries_after: Some(entries_after),
        entries_added,
        entries_removed: (entries_before + entries_added).saturating_sub(entries_after),
    }
}

/// A region that went away, with everything it held.
fn removal(name: &str, before: &ContextState) -> RegionTransition {
    let was = before.region(name);
    let tokens = was.map_or(0, |r| r.current_tokens as usize);
    let entries = was.map_or(0, |r| r.entries.len());
    RegionTransition {
        region: name.to_string(),
        digest_before: None,
        digest_after: None,
        tokens_before: Some(tokens),
        tokens_after: Some(0),
        token_delta: -(tokens as i64),
        entries_before: Some(entries),
        entries_after: Some(0),
        entries_added: 0,
        entries_removed: entries,
    }
}

#[cfg(test)]
#[path = "context_changes_tests.rs"]
mod tests;
