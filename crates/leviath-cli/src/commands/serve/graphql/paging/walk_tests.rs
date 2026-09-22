//! Tests for the lazy walk.
//!
//! The reader here counts. Most of what the walk promises is about work it
//! does *not* do - files it never opens, items it never looks at twice - and
//! the only way to test an absence is to count the thing that would have
//! happened.

use std::sync::Mutex;

use futures_util::future::BoxFuture;

use super::super::order::{Order, OrderDirection, OrderField, Orderable, Position, Term};
use super::{CONFIRM_CHUNK, Page, Sift, Verdict, walk};
use crate::commands::serve::cursor::{Cursor, CursorKey};

/// One row: an id, an in-memory rank, and something only a file would say.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    id: String,
    rank: i64,
    /// What the pretend file holds. Reaching it goes through the counter.
    deep: bool,
}

/// The one field these rows sort by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rank;

impl OrderField for Rank {
    fn wire(self) -> &'static str {
        "rank"
    }
}

impl Orderable<()> for Row {
    type Field = Rank;

    fn key(&self, _field: Rank, _cx: &()) -> CursorKey {
        CursorKey::Int(self.rank)
    }
}

/// What the tests filter with, and the tally of every file it pretended to
/// open.
struct Counting {
    /// Rows whose cheap half fails outright.
    drop_ids: Vec<String>,
    /// Whether any row needs reading at all.
    reads: bool,
    /// The order the walk runs in.
    order: Order<Rank>,
    /// Every id whose confirmation was run, in the order they ran.
    confirmed: Mutex<Vec<String>>,
}

impl Counting {
    fn new(reads: bool, direction: OrderDirection) -> Self {
        Self {
            drop_ids: Vec::new(),
            reads,
            order: Order::new(vec![Term {
                field: Rank,
                direction,
            }]),
            confirmed: Mutex::new(Vec::new()),
        }
    }

    fn dropping(mut self, ids: &[&str]) -> Self {
        self.drop_ids = ids.iter().map(|id| (*id).to_string()).collect();
        self
    }

    fn reads_so_far(&self) -> Vec<String> {
        leviath_core::sync::lock(&self.confirmed).clone()
    }
}

impl Sift for Counting {
    type Item = Row;

    fn test(&self, item: &Row) -> Verdict {
        if self.drop_ids.contains(&item.id) {
            return Verdict::Drop;
        }
        match self.reads {
            true => Verdict::NeedsIo,
            false => Verdict::Keep,
        }
    }

    fn confirm<'a>(&'a self, item: &'a Row) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            leviath_core::sync::lock(&self.confirmed).push(item.id.clone());
            item.deep
        })
    }

    fn position(&self, item: &Row) -> Position {
        self.order.position(item, item.id.clone(), &())
    }

    fn descending(&self) -> &[bool] {
        self.order.descending()
    }
}

/// `count` rows, ranked 0 upward, every one of which a file would keep.
fn rows(count: usize) -> Vec<Row> {
    (0..count)
        .map(|at| Row {
            id: format!("row-{at:03}"),
            rank: i64::try_from(at).expect("small"),
            deep: true,
        })
        .collect()
}

fn ids<S: Sift<Item = Row>>(page: &Page<S>) -> Vec<String> {
    page.items.iter().map(|row| row.id.clone()).collect()
}

/// The cursor that resumes after this page, as a walk would read it back.
fn resume(sift: &Counting, page: &Page<Counting>) -> Cursor {
    let position = page.next.as_ref().expect("another page follows");
    let raw = sift.order.encode("abcd1234", position);
    sift.order.decode(&raw, "abcd1234").expect("round trip")
}

#[tokio::test]
async fn a_page_returns_what_was_asked_for_in_order() {
    let sift = Counting::new(false, OrderDirection::Asc);
    let page = walk(sift, rows(5), None, 2).await;
    assert_eq!(ids(&page), vec!["row-000", "row-001"]);
    assert_eq!(
        page.next.as_ref().map(|position| position.id.clone()),
        Some("row-001".to_string())
    );
}

/// The walk sorts; whatever order the snapshot happened to be in does not
/// leak into the answer.
#[tokio::test]
async fn the_walk_sorts_before_it_pages() {
    let mut shuffled = rows(4);
    shuffled.reverse();
    let ascending = walk(
        Counting::new(false, OrderDirection::Asc),
        shuffled.clone(),
        None,
        4,
    )
    .await;
    assert_eq!(
        ids(&ascending),
        vec!["row-000", "row-001", "row-002", "row-003"]
    );
    let descending = walk(
        Counting::new(false, OrderDirection::Desc),
        shuffled,
        None,
        4,
    )
    .await;
    assert_eq!(
        ids(&descending),
        vec!["row-003", "row-002", "row-001", "row-000"]
    );
}

/// The extra item examined past the page is never returned; it only decides
/// whether there is a next page at all.
#[tokio::test]
async fn the_last_page_has_no_cursor() {
    let page = walk(Counting::new(false, OrderDirection::Asc), rows(3), None, 3).await;
    assert_eq!(ids(&page).len(), 3);
    assert_eq!(page.next, None);
}

#[tokio::test]
async fn an_empty_listing_pages_to_nothing() {
    let mut page = walk(
        Counting::new(false, OrderDirection::Asc),
        Vec::new(),
        None,
        5,
    )
    .await;
    assert_eq!(ids(&page).len(), 0);
    assert_eq!(page.next, None);
    assert_eq!(page.total().await, 0);
}

/// A filter answerable from memory opens nothing, whatever the page size.
#[tokio::test]
async fn a_cheap_only_filter_confirms_nothing() {
    let sift = Counting::new(false, OrderDirection::Asc).dropping(&["row-001"]);
    let mut page = walk(sift, rows(6), None, 2).await;
    assert_eq!(ids(&page), vec!["row-000", "row-002"]);
    assert_eq!(page.total().await, 5);
    assert_eq!(page.sift.reads_so_far().len(), 0);
}

/// The promise of the seek: resuming reads nothing for the page before.
#[tokio::test]
async fn page_two_confirms_nothing_that_precedes_the_cursor() {
    let first = walk(Counting::new(true, OrderDirection::Asc), rows(8), None, 3).await;
    assert_eq!(ids(&first), vec!["row-000", "row-001", "row-002"]);
    let after = resume(&first.sift, &first);

    let second = walk(
        Counting::new(true, OrderDirection::Asc),
        rows(8),
        Some(&after),
        3,
    )
    .await;
    assert_eq!(ids(&second), vec!["row-003", "row-004", "row-005"]);
    let read = second.sift.reads_so_far();
    assert!(!read.iter().any(|id| id.as_str() < "row-003"));
    assert_eq!(read.first().map(String::as_str), Some("row-003"));
}

/// Walked to exhaustion a cursor chain covers everything once: no repeats, no
/// gaps, and the last page says it is the last.
#[tokio::test]
async fn a_cursor_chain_covers_the_listing_exactly_once() {
    let mut seen: Vec<String> = Vec::new();
    let mut after: Option<Cursor> = None;
    loop {
        let sift = Counting::new(true, OrderDirection::Desc);
        let page = walk(sift, rows(7), after.as_ref(), 2).await;
        seen.extend(ids(&page));
        if page.next.is_none() {
            break;
        }
        after = Some(resume(&page.sift, &page));
    }
    let mut expected: Vec<String> = rows(7).into_iter().map(|row| row.id).collect();
    expected.reverse();
    assert_eq!(seen, expected);
}

/// A run that a file says no to is dropped at step four, and the page fills up
/// from behind it rather than coming back short.
#[tokio::test]
async fn an_item_a_file_refuses_does_not_take_a_slot() {
    let mut all = rows(5);
    all[1].deep = false;
    all[2].deep = false;
    let page = walk(Counting::new(true, OrderDirection::Asc), all, None, 2).await;
    assert_eq!(ids(&page), vec!["row-000", "row-003"]);
}

/// The reads run in ordered chunks, so a page needing many of them does not
/// pay one round trip per item - and does not read the whole store either.
#[tokio::test]
async fn confirmations_run_in_ordered_chunks() {
    let count = CONFIRM_CHUNK * 3;
    let sift = Counting::new(true, OrderDirection::Asc);
    let page = walk(sift, rows(count), None, CONFIRM_CHUNK * 2).await;
    assert_eq!(page.items.len(), CONFIRM_CHUNK * 2);
    let read = page.sift.reads_so_far();
    // Everything the page returned, plus the one that says a page follows.
    // Far short of the whole listing.
    assert_eq!(read.len(), CONFIRM_CHUNK * 2 + 1);
    let mut ordered = read.clone();
    ordered.sort();
    assert_eq!(read, ordered);
}

/// A count is the whole listing, not what is left of it, and it settles only
/// what the page did not.
#[tokio::test]
async fn a_total_counts_every_page_and_reuses_what_the_page_settled() {
    let mut all = rows(10);
    all[4].deep = false;
    let sift = Counting::new(true, OrderDirection::Asc);
    let first = walk(sift, all.clone(), None, 2).await;
    let after = resume(&first.sift, &first);

    let mut second = walk(
        Counting::new(true, OrderDirection::Asc),
        all,
        Some(&after),
        2,
    )
    .await;
    let before = second.sift.reads_so_far().len();
    assert_eq!(second.total().await, 9);
    let read = second.sift.reads_so_far();
    assert_eq!(read.len(), 10);
    // Nothing the page had already settled was read a second time.
    let mut unique = read.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), read.len());
    assert!(before > 0);
    // Asking twice costs nothing more.
    assert_eq!(second.total().await, 9);
    assert_eq!(second.sift.reads_so_far().len(), 10);
}

/// A cursor past the end of the listing is an empty last page rather than a
/// failure: the rows it named may simply have been deleted.
#[tokio::test]
async fn a_cursor_past_the_end_gives_an_empty_page() {
    let sift = Counting::new(false, OrderDirection::Asc);
    let order = Order::new(vec![Term {
        field: Rank,
        direction: OrderDirection::Asc,
    }]);
    let beyond = Position {
        key: CursorKey::Int(9_999),
        id: "row-999".to_string(),
    };
    let raw = order.encode("abcd1234", &beyond);
    let cursor = order.decode(&raw, "abcd1234").expect("round trip");
    let page = walk(sift, rows(4), Some(&cursor), 2).await;
    assert_eq!(ids(&page).len(), 0);
    assert_eq!(page.next, None);
}
