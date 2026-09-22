//! The five-step lazy walk every listing pages with.
//!
//! ```text
//!  snapshot, in memory
//!         |
//!    (1) cheap test per item ..... Drop -> gone        | no file is opened
//!         |                        Keep / NeedsIo kept |
//!    (2) sort by the order ....... keys are in-memory values only
//!         |
//!    (3) seek past `after` ....... a keyset compare: nothing before the cursor
//!         |                        is ever confirmed or re-read
//!    (4) walk forward ............ Keep -> take ; NeedsIo -> confirm (reads
//!         |                        files), in ordered chunks for throughput
//!    (5) stop at first + 1 ....... the extra one only decides whether there is
//!         |                        a next page
//!      items, and `next` = the position of the last item returned
//! ```
//!
//! **There is no scan cap.** A filter that has to read a file may walk the
//! whole store, and that is accepted: the alternative was a cap that quietly
//! truncated an answer and a `scanTruncated` flag for the client to wonder
//! about. What keeps it affordable is the order of the steps. The half of a
//! filter that can be answered from memory runs first and throws most items
//! out; the sort and the seek are in memory; and reading only starts once the
//! walk reaches the page being asked for. Page two of a file-backed filter
//! therefore reads nothing for page one's items.
//!
//! [`Page::total`] is the one thing that can cost a full pass, which is why it
//! is a resolver rather than a field: it runs only if the client selects it,
//! and it settles only what the page left unsettled.

use futures_util::future::BoxFuture;

use super::order::Position;
use crate::commands::serve::cursor::{self, Cursor};

/// How many file-reading confirmations a walk runs at once.
///
/// One at a time makes a page of fifty file-backed matches fifty round trips
/// of latency; all of them at once is an unbounded fan-out over a store of
/// unknown size. Sixteen keeps the reads in flight without letting the walk
/// read far past the page it was asked for.
pub(crate) const CONFIRM_CHUNK: usize = 16;

/// Whether an item belongs in a listing, when the answer may need a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// It matches, and nothing had to be read to know that.
    Keep,
    /// It does not match, and nothing had to be read to know that.
    Drop,
    /// Everything answerable from memory passed; the rest needs files.
    NeedsIo,
}

/// The test and the ordering one walk runs on.
///
/// Deliberately says nothing about where the context comes from: an
/// implementation owns whatever its test and its keys need, so this stays the
/// same shape for runs, blueprints, journal rows and anything else that grows.
pub(crate) trait Sift: Send + Sync {
    /// What is being paged.
    type Item: Send + Sync;

    /// The verdict reachable without touching the filesystem.
    ///
    /// Called for every item in the snapshot, so it has to stay cheap: this is
    /// the step that makes the sort and the seek affordable.
    fn test(&self, item: &Self::Item) -> Verdict;

    /// Settle a [`Verdict::NeedsIo`] item, reading whatever that takes.
    ///
    /// Called only for items the page actually reaches, never for one the
    /// cursor already skipped past.
    fn confirm<'a>(&'a self, item: &'a Self::Item) -> BoxFuture<'a, bool>;

    /// Where this item sits in the walk order.
    fn position(&self, item: &Self::Item) -> Position;

    /// One direction per key component, for the keyset comparison.
    fn descending(&self) -> &[bool];
}

/// One item's place in a walk.
struct Entry<T> {
    /// The item, until the page takes it.
    item: Option<T>,
    /// Where it sits in the order.
    position: Position,
    /// Whether it matched: `None` until a confirmation settles it.
    kept: Option<bool>,
}

/// One page, and the state that can go on to count the whole listing.
pub(crate) struct Page<S: Sift> {
    /// The items on this page, in walk order, at most as many as were asked
    /// for.
    pub(crate) items: Vec<S::Item>,
    /// Where the next page resumes: the position of the last item returned.
    /// Absent when this is the last page.
    pub(crate) next: Option<Position>,
    /// What the walk ran on, kept so a count can finish what the page started.
    sift: S,
    /// Every item that survived the cheap test, in order, with whatever the
    /// page already settled about it.
    entries: Vec<Entry<S::Item>>,
}

/// Walk `items` and return the page of `first` that follows `after`.
///
/// `first` is the page size the client asked for; one more than that is
/// examined, and that extra item only decides whether [`Page::next`] is
/// present.
pub(crate) async fn walk<S: Sift>(
    sift: S,
    items: Vec<S::Item>,
    after: Option<&Cursor>,
    first: usize,
) -> Page<S> {
    // (1) The cheap test. No file is opened here, for any item.
    let mut entries: Vec<Entry<S::Item>> = items
        .into_iter()
        .filter_map(|item| {
            let kept = match sift.test(&item) {
                Verdict::Drop => return None,
                Verdict::Keep => Some(true),
                Verdict::NeedsIo => None,
            };
            Some(Entry {
                position: sift.position(&item),
                item: Some(item),
                kept,
            })
        })
        .collect();

    // (2) Sort by the order's own keys, which are in-memory values.
    let descending = sift.descending().to_vec();
    entries.sort_by(|a, b| {
        cursor::compare(
            (&a.position.key, &a.position.id),
            (&b.position.key, &b.position.id),
            &descending,
        )
    });

    // (3) Seek past the cursor. The list is sorted, so the first item that
    // follows the cursor is where the page starts, and nothing before it is
    // looked at again.
    let start = match after {
        None => 0,
        Some(cursor) => entries
            .iter()
            .position(|entry| {
                cursor.precedes_in(&entry.position.key, &entry.position.id, &descending)
            })
            .unwrap_or(entries.len()),
    };

    let mut page = Page {
        items: Vec::new(),
        next: None,
        sift,
        entries,
    };
    page.fill(start, first).await;
    page
}

impl<S: Sift> Page<S> {
    /// What the walk ran on, for a caller that mints the cursor from it.
    pub(crate) fn sift(&self) -> &S {
        &self.sift
    }

    /// Steps four and five: take items in order, confirming only what is
    /// reached, and stop one past the page.
    async fn fill(&mut self, start: usize, first: usize) {
        let want = first + 1;
        let mut taken: Vec<S::Item> = Vec::with_capacity(want);
        let mut positions: Vec<Position> = Vec::with_capacity(want);
        let mut at = start;
        while taken.len() < want && at < self.entries.len() {
            let end = window_end(&self.entries, at, want - taken.len());
            self.settle(at, end).await;
            while at < end && taken.len() < want {
                if self.entries[at].kept == Some(true) {
                    // The walk only moves forward, so an entry is reached once.
                    let item = self.entries[at]
                        .item
                        .take()
                        .expect("each item is reached once");
                    taken.push(item);
                    positions.push(self.entries[at].position.clone());
                }
                at += 1;
            }
        }

        // The extra item is never returned; it only says a next page exists.
        let more = taken.len() > first;
        taken.truncate(first);
        positions.truncate(first);
        self.next = more.then(|| positions.last().cloned()).flatten();
        self.items = taken;
    }

    /// How many items matched, across every page.
    ///
    /// Settles whatever the page did not, including the items the cursor
    /// skipped past, so the answer is the size of the whole listing rather
    /// than of what is left of it. That is why it is worth asking for on the
    /// first page and not on every one.
    pub(crate) async fn total(&mut self) -> usize {
        let mut at = 0;
        while at < self.entries.len() {
            let end = window_end(&self.entries, at, self.entries.len() - at);
            self.settle(at, end).await;
            at = end;
        }
        self.entries
            .iter()
            .filter(|entry| entry.kept == Some(true))
            .count()
    }

    /// Confirm every unsettled entry in `from..to`, all at once.
    async fn settle(&mut self, from: usize, to: usize) {
        let pending: Vec<usize> = (from..to)
            .filter(|&at| self.entries[at].kept.is_none())
            .collect();
        if pending.is_empty() {
            return;
        }
        let answers = futures_util::future::join_all(pending.iter().map(|&at| {
            // An item is only taken once it is settled, so an unsettled entry
            // still holds the one its confirmation needs.
            let item = self.entries[at]
                .item
                .as_ref()
                .expect("an unsettled item has not been taken");
            self.sift.confirm(item)
        }))
        .await;
        for (at, kept) in pending.into_iter().zip(answers) {
            self.entries[at].kept = Some(kept);
        }
    }
}

/// How far the next confirmation window reaches.
///
/// It stops at whichever comes first: enough entries to satisfy what the page
/// still needs, or [`CONFIRM_CHUNK`] entries that need reading. A run of items
/// already settled costs nothing, so it does not shorten the window - which is
/// what makes a cheap-only filter confirm nothing at all.
fn window_end<T>(entries: &[Entry<T>], at: usize, needed: usize) -> usize {
    let mut unsettled = 0usize;
    let mut end = at;
    while end < entries.len() && end - at < needed && unsettled < CONFIRM_CHUNK {
        if entries[end].kept.is_none() {
            unsettled += 1;
        }
        end += 1;
    }
    end
}

#[cfg(test)]
#[path = "walk_tests.rs"]
mod tests;
