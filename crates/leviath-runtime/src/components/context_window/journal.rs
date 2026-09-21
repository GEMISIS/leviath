//! Recording why a region changed, as it changes.
//!
//! The persistence lane already carries the window itself, as a snapshot per
//! tick. That is what a region held; it is not what moved it, and the two are
//! not recoverable from one another: a plan region that emptied looks the same
//! whether a compaction took it, a stage-edge transform cleared it, or the model
//! called `context_delete`.
//!
//! So each write appends its own small record, the same fire-and-forget shape
//! [`crate::inference_usage`] uses for what a provider call cost:
//! [`PersistMsg::Append`] with no ack, because nothing downstream waits on it.
//! Deliberately not a buffer on the window drained by the snapshot lane - that
//! lane takes the window immutably and coalesces superseded snapshots, so a
//! buffer would lose writes on exactly the busiest ticks.
//!
//! The handle lives on the window because the writers do not have one. A write
//! happens wherever a `&mut ContextWindow` does: inside a tool handler, a
//! fan-out worker, a transform, a nudge - most of them several calls below the
//! system that could see the world's resources. Threading the lane down to each
//! of them would be a wider change than the records are worth, and would still
//! leave the ones reached from a plain helper function unattributed.

use leviath_core::ContextCause;
use leviath_core::run_archive::RunRecord;

use crate::persistence_bridge::PersistMsg;

use super::ContextWindow;

/// Where one window's change records go.
#[derive(Debug, Clone)]
pub(crate) struct ContextJournal {
    /// The run whose archive the records belong in.
    run_id: String,
    /// The persistence lane, downgraded from the world's `PersistenceStage`.
    ///
    /// Weak, and that is the whole point. A clean shutdown closes the lane by
    /// dropping the world's own sender and then waiting for the worker's `recv`
    /// to end, so anything else holding a live sender holds the shutdown open
    /// instead: one window per agent, each keeping the channel alive, and
    /// `lev daemon stop` never returns. A weak handle cannot do that. It also
    /// says the right thing about who the lane belongs to - the world owns it,
    /// and a window only writes down it while it is open.
    sender: tokio::sync::mpsc::WeakUnboundedSender<PersistMsg>,
}

/// How much of a region there is: what a change is measured against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RegionShape {
    /// Entries the region holds.
    entries: usize,
    /// What those entries come to in tokens.
    tokens: usize,
}

impl RegionShape {
    /// How many entries the region held.
    ///
    /// The one field anything outside this module needs: a resume rebuilds a
    /// region by assignment and has to say how many entries it put back.
    pub(crate) fn entries(self) -> usize {
        self.entries
    }
}

/// What one change moved in a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegionMove {
    /// Entries the change pushed.
    added: usize,
    /// Entries that left, an eviction the write itself triggered included.
    removed: usize,
    /// How the region's token count moved; negative when it shrank.
    token_delta: i64,
}

impl RegionMove {
    /// The move from `before` to `after`, given that the change itself pushed
    /// `added` entries.
    ///
    /// Removals are inferred rather than counted, because the only code that
    /// knows it dropped an entry is the region's own eviction, several layers
    /// below any caller: a write that appends one entry and leaves the region
    /// two shorter evicted three. Inferring it this way can never invent a
    /// removal - it reports none when the arithmetic does not close, which is
    /// the case for nothing the runtime does today.
    fn between(before: RegionShape, after: RegionShape, added: usize) -> Self {
        Self {
            added,
            removed: (before.entries + added).saturating_sub(after.entries),
            token_delta: after.tokens as i64 - before.tokens as i64,
        }
    }

    /// Whether nothing moved at all, in which case there is nothing to record.
    /// A write a region hook refused and one the budget rejected both leave the
    /// region exactly as it was, and a journal full of those says less than one
    /// without them.
    fn is_still(&self) -> bool {
        self.added == 0 && self.removed == 0 && self.token_delta == 0
    }
}

impl ContextWindow {
    /// Point this window's change records at the run's journal.
    ///
    /// Called once, at spawn, which is the one place both halves are in hand:
    /// the run id the archive is named after, and the lane the rest of the
    /// runtime appends through. A window with no journal - every test window,
    /// `lev test`, an agent in a world that persists nothing - records nothing
    /// and is otherwise unaffected.
    ///
    /// A record can only land once the run's archive exists, and it is the first
    /// snapshot that creates it. The seeds written during spawn itself are
    /// therefore dropped by the lane, exactly as an early usage record is.
    pub(crate) fn attach_journal(
        &mut self,
        run_id: &str,
        persist: Option<&crate::pipeline::PersistenceStage>,
    ) {
        self.journal = persist.map(|stage| ContextJournal {
            run_id: run_id.to_string(),
            sender: stage.0.downgrade(),
        });
    }

    /// What `region` holds right now: an empty shape when the window has no
    /// region by that name, which is also what a write to it would leave.
    pub(crate) fn region_shape(&self, region: &str) -> RegionShape {
        match self.get_region(region) {
            Some(r) => RegionShape {
                entries: r.content.len(),
                tokens: r.current_tokens,
            },
            None => RegionShape::default(),
        }
    }

    /// Record that `cause` changed `region`, which held `before` beforehand and
    /// had `added` entries pushed into it.
    ///
    /// The caller states the cause because only the caller knows it; there is no
    /// default, and a path with no cause of its own records nothing rather than
    /// borrowing the nearest one.
    pub(crate) fn journal_change(
        &self,
        cause: ContextCause,
        region: &str,
        before: RegionShape,
        added: usize,
    ) {
        let Some(journal) = self.journal.as_ref() else {
            return;
        };
        let moved = RegionMove::between(before, self.region_shape(region), added);
        if moved.is_still() {
            return;
        }
        let record = RunRecord::ContextChange {
            region: region.to_string(),
            cause,
            entries_added: moved.added,
            entries_removed: moved.removed,
            token_delta: moved.token_delta,
            at: chrono::Utc::now().timestamp(),
        };
        // A lane that has closed takes nothing: the world drops its sender to
        // shut the lane down, and a write racing that is a write nobody is
        // waiting on. No ack either - a change record is history, and the run
        // does not wait on its own history the way the tool lane waits on a
        // batch record.
        let Some(sender) = journal.sender.upgrade() else {
            return;
        };
        let _ = sender.send(PersistMsg::Append {
            run_id: journal.run_id.clone(),
            record: Box::new(record),
            ack: None,
        });
    }

    /// [`journal_change`](Self::journal_change) for a keyed upsert, which adds an
    /// entry only when the key was not already there.
    ///
    /// Nothing the caller holds tells those two apart, so the entry count
    /// decides: a region that grew took a new key, one that held steady had a
    /// key replaced where it stood.
    pub(crate) fn journal_upsert(&self, cause: ContextCause, region: &str, before: RegionShape) {
        let added = self
            .region_shape(region)
            .entries
            .saturating_sub(before.entries);
        self.journal_change(cause, region, before, added);
    }
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
