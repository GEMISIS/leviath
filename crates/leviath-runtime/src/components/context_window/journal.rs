//! Recording what changed a context window, as it changes.
//!
//! The persistence lane already carries the window itself, as a snapshot per
//! tick. That is what a region held; it is not what moved it, and the two are
//! not recoverable from one another: a plan region that emptied looks the same
//! whether a compaction took it, a stage-edge transform cleared it, or the model
//! called `context_delete`.
//!
//! A change is recorded as the transaction it is. One write to one region is the
//! smallest thing that happens and rarely the whole of what happened - a
//! compaction summarises one region and empties another, a stage edge clears
//! four, a resume rebuilds every region there is - so a transaction is opened
//! over the regions it is about to touch, and committed once, naming the window
//! it started from and the window it produced. Read
//! [`leviath_core::run_archive::RunRecord::ContextTransaction`] for what that
//! buys a reader.
//!
//! Each commit appends its own small record, the same fire-and-forget shape
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
use leviath_core::run_archive::{RegionCommit, RunRecord};
use leviath_core::run_meta::revision::{EntryFacts, RegionFacts, region_digest, window_revision};

use crate::persistence_bridge::PersistMsg;

use super::ContextWindow;

/// Where one window's change records go.
#[derive(Debug, Clone)]
pub(crate) struct ContextJournal {
    /// The run whose archive the records belong in.
    run_id: String,
    /// The tool execution whose work the next commits belong to, or empty when
    /// no call is being handled.
    ///
    /// Set per call by the dispatcher that is handling one, and cleared when it
    /// stops handling calls. A write outside any call - the model's own reply, a
    /// compaction, a nudge - commits with this empty, and the record then says
    /// that no execution was known rather than borrowing the last one.
    executing: String,
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

/// What one region held when a transaction opened.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RegionBefore {
    /// The region's name, which is what pairs it with its later self.
    name: String,
    /// The identity of its contents.
    digest: String,
    /// What those contents came to in tokens.
    tokens: usize,
    /// How many entries there were.
    entries: usize,
}

/// A change to a context window, opened over the regions it will touch and
/// committed once the writing is done.
///
/// Carries nothing at all for a window with no journal - every test window,
/// `lev test`, an agent in a world that persists nothing. Such a window records
/// no history, so measuring one costs it nothing either.
#[derive(Debug)]
pub(crate) struct ContextTxn {
    /// The window as it was, or `None` when nothing is being recorded.
    opened: Option<Opened>,
}

/// The window a transaction started from.
#[derive(Debug)]
struct Opened {
    /// The window's revision before the change.
    revision: String,
    /// Each region the transaction was opened over, in the order it named them.
    regions: Vec<RegionBefore>,
}

/// How many entries a transaction pushed into each region it touched.
///
/// Stated by the caller because only the caller knows: what a region ended up
/// holding cannot tell a write that appended one entry into a full sliding
/// region from one that appended nothing, and the difference is whether the
/// entry that left was evicted or was never there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pushed {
    /// Nothing was pushed. The transaction only took entries away.
    Nothing,
    /// This many entries, into the transaction's single region.
    Into(usize),
    /// Every entry each region now holds: a resume putting a window back by
    /// assignment, where nothing was there before and everything arrived.
    Everything,
    /// One entry per region that grew, and none where a keyed write replaced an
    /// entry where it stood.
    Upsert,
}

impl Pushed {
    /// How many entries this pushed into a region that went from `before` to
    /// `after` entries.
    fn into_region(self, before: usize, after: usize) -> usize {
        match self {
            Pushed::Nothing => 0,
            Pushed::Into(n) => n,
            Pushed::Everything => after,
            Pushed::Upsert => after.saturating_sub(before),
        }
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
            executing: String::new(),
            sender: stage.0.downgrade(),
        });
    }

    /// Attribute the transactions committed from here on to `execution_id`, or
    /// to nothing when it is empty.
    ///
    /// Set by a dispatcher as it starts handling one call and cleared when it
    /// stops, so the attribution is never wider than the call it belongs to.
    pub(crate) fn attribute_to(&mut self, execution_id: &str) {
        if let Some(journal) = self.journal.as_mut() {
            journal.executing.clear();
            journal.executing.push_str(execution_id);
        }
    }

    /// The identity of one region's contents right now.
    ///
    /// An absent region digests as an empty one, which is also what a write to it
    /// would find.
    fn region_digest_now(&self, region: &str) -> String {
        let Some(found) = self.get_region(region) else {
            return region_digest(std::iter::empty());
        };
        region_digest(facts(found))
    }

    /// What `region` held right now, as a transaction measures it.
    fn region_before(&self, region: &str) -> RegionBefore {
        let found = self.get_region(region);
        RegionBefore {
            name: region.to_string(),
            digest: self.region_digest_now(region),
            tokens: found.map_or(0, |r| r.current_tokens),
            entries: found.map_or(0, |r| r.content.len()),
        }
    }

    /// This window's revision: the content-addressed name of what it holds.
    ///
    /// Computed the same way a reader computes it from a snapshot, so a
    /// revision this window recorded is one a reader can find.
    fn revision_now(&self) -> String {
        let digests: Vec<String> = self
            .regions
            .iter()
            .map(|region| region_digest(facts(region)))
            .collect();
        window_revision(
            self.current_tokens,
            self.max_tokens,
            self.regions
                .iter()
                .zip(&digests)
                .map(|(region, digest)| RegionFacts {
                    name: &region.name,
                    kind: crate::persistence::region_kind_str(&region.kind),
                    current_tokens: region.current_tokens,
                    max_tokens: region.max_tokens,
                    digest,
                }),
        )
    }

    /// Open a transaction over one region.
    pub(crate) fn begin_change(&self, region: &str) -> ContextTxn {
        self.begin_changes(std::iter::once(region))
    }

    /// Open a transaction over several regions at once.
    ///
    /// Every region the writing is about to touch has to be named here: a region
    /// left out is one the record will not mention, and a reader would see a
    /// window that moved further than the transaction said it did.
    pub(crate) fn begin_changes<'a>(
        &self,
        regions: impl IntoIterator<Item = &'a str>,
    ) -> ContextTxn {
        // Measured only where it will be recorded. A window with no journal is
        // every test window and every unpersisted agent, and hashing its regions
        // to throw the answer away would put the cost of history on the runs
        // that keep none.
        if self.journal.is_none() {
            return ContextTxn { opened: None };
        }
        ContextTxn {
            opened: Some(Opened {
                revision: self.revision_now(),
                regions: regions
                    .into_iter()
                    .map(|region| self.region_before(region))
                    .collect(),
            }),
        }
    }

    /// Commit `txn` as one transaction that `cause` made, having pushed `added`.
    ///
    /// The caller states the cause because only the caller knows it; there is no
    /// default, and a path with no cause of its own records nothing rather than
    /// borrowing the nearest one.
    ///
    /// A transaction that moved nothing at all is dropped. A write a region hook
    /// refused and one the budget rejected both leave the window exactly as it
    /// was, and a journal full of those says less than one without them.
    pub(crate) fn commit_change(&self, cause: ContextCause, txn: ContextTxn, added: Pushed) {
        let (Some(journal), Some(opened)) = (self.journal.as_ref(), txn.opened) else {
            return;
        };
        let regions: Vec<RegionCommit> = opened
            .regions
            .into_iter()
            .map(|before| self.commit_of(before, added))
            .collect();
        let revision_after = self.revision_now();
        // Nothing moved: the window is the one the transaction opened on. The
        // revision answers this for the whole transaction at once, where a
        // per-region comparison would have to agree with it anyway.
        if revision_after == opened.revision {
            return;
        }
        let record = RunRecord::ContextTransaction {
            revision_before: opened.revision,
            revision_after,
            cause,
            regions,
            execution_id: journal.executing.clone(),
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

    /// One region's part of a commit: what it held when the transaction opened,
    /// beside what it holds now.
    fn commit_of(&self, before: RegionBefore, added: Pushed) -> RegionCommit {
        let now = self.get_region(&before.name);
        let entries_after = now.map_or(0, |r| r.content.len());
        RegionCommit {
            digest_before: before.digest,
            digest_after: self.region_digest_now(&before.name),
            tokens_before: before.tokens,
            tokens_after: now.map_or(0, |r| r.current_tokens),
            entries_before: before.entries,
            entries_after,
            entries_added: added.into_region(before.entries, entries_after),
            region: before.name,
        }
    }
}

/// One region's entries as the fingerprints a digest is taken over.
///
/// Taint comes from the region rather than the entry, which is where the live
/// window keeps it, and defaults to the level a restore would assume for a
/// region that does not track it - the same rule the snapshot writer follows, so
/// the two produce one digest for one region.
fn facts(region: &leviath_core::Region) -> impl Iterator<Item = EntryFacts<'_>> {
    region
        .content
        .iter()
        .enumerate()
        .map(move |(index, entry)| EntryFacts {
            content: &entry.content,
            tokens: entry.tokens,
            kind: &entry.kind,
            metadata: entry.metadata.as_ref(),
            key: entry.key.as_deref(),
            taint: region
                .taint
                .as_ref()
                .and_then(|taint| taint.entry_taint(index))
                .unwrap_or_default(),
            reasoning: entry.reasoning.as_deref(),
        })
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
