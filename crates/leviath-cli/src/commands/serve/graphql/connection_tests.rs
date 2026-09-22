//! Tests for the generic connection.
//!
//! Two things are worth pinning here. One is the name: a generic that produced
//! `Connection` for every listing would collide the moment two of them were in
//! one schema, so the SDL is printed and read. The other is the laziness of
//! `total`, which is only observable as work that did not happen - so the
//! counter is a flag that says whether it ran.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_graphql::{EmptyMutation, EmptySubscription, Object, Schema, SimpleObject, Value};

use super::{Connection, NoExtras, Paged, PositionQuery, Total, position_page};
use crate::commands::serve::graphql::filter::{BoxFuture, Filterable, MatchCx, Tri};
use crate::commands::serve::graphql::scalars::Cursor;

/// An item type, standing in for a mirrored output type.
#[derive(Debug, SimpleObject)]
struct Row {
    /// The row's name.
    name: String,
}

impl Paged for Row {
    const NAME: &'static str = "Row";
}

/// A second item type, so the two instantiations can be told apart.
#[derive(Debug, SimpleObject)]
struct Note {
    /// What the note says.
    body: String,
}

impl Paged for Note {
    const NAME: &'static str = "Note";
}

/// What one listing adds beyond the three core fields.
#[derive(Debug, SimpleObject)]
struct RowExtras {
    /// Why these rows matched.
    highlights: Vec<String>,
}

/// How many times the counter was driven, shared with the test that reads it.
type Tally = Arc<AtomicUsize>;

fn rows() -> Vec<Row> {
    vec![
        Row {
            name: "a".to_string(),
        },
        Row {
            name: "b".to_string(),
        },
    ]
}

/// A count that records the fact that it ran.
fn counted(tally: &Tally, total: usize) -> Total {
    let tally = Arc::clone(tally);
    Total::lazy(async move {
        tally.fetch_add(1, Ordering::SeqCst);
        total
    })
}

/// The test schema's root, carrying one connection of each shape.
struct Query {
    /// How many times the row listing's count ran.
    tally: Tally,
}

#[Object]
impl Query {
    /// A plain listing.
    async fn rows(&self) -> Connection<Row> {
        Connection::plain(
            rows(),
            Some(Cursor("next".to_string())),
            counted(&self.tally, 7),
        )
    }

    /// A listing with something extra to say.
    async fn notes(&self) -> Connection<Note, RowExtras> {
        Connection::new(
            vec![Note {
                body: "hi".to_string(),
            }],
            None,
            Total::known(1),
            RowExtras {
                highlights: vec!["body".to_string()],
            },
        )
    }
}

fn schema(tally: &Tally) -> Schema<Query, EmptyMutation, EmptySubscription> {
    Schema::build(
        Query {
            tally: Arc::clone(tally),
        },
        EmptyMutation,
        EmptySubscription,
    )
    .finish()
}

async fn answer(tally: &Tally, query: &str) -> Value {
    let response = schema(tally).execute(query).await;
    assert_eq!(response.errors, Vec::new());
    response.data
}

/// One generic, two names: without a name per item type the second listing
/// would collide with the first the moment both were in one schema.
#[test]
fn each_instantiation_prints_its_own_connection_type() {
    let sdl = schema(&Tally::default()).sdl();
    assert!(sdl.contains("type RowConnection {"));
    assert!(sdl.contains("type NoteConnection {"));
    assert!(!sdl.contains("type Connection {"));
}

/// The three core fields, in the one shape every listing has.
#[test]
fn a_connection_has_results_a_cursor_and_a_total() {
    let sdl = schema(&Tally::default()).sdl();
    let start = sdl.find("type RowConnection {").expect("the row listing");
    let block = sdl.split_at(start).1.split('}').next().expect("a body");
    assert!(block.contains("results: [Row!]!"));
    assert!(block.contains("cursor: Cursor"));
    assert!(block.contains("total: Int!"));
}

/// A listing with extras keeps the three and gains the rest, rather than
/// wrapping them in something a client has to unpack.
#[test]
fn extras_are_flattened_in_beside_the_core_fields() {
    let sdl = schema(&Tally::default()).sdl();
    let start = sdl.find("type NoteConnection {").expect("the note listing");
    let block = sdl.split_at(start).1.split('}').next().expect("a body");
    assert!(block.contains("results: [Note!]!"));
    assert!(block.contains("total: Int!"));
    assert!(block.contains("highlights: [String!]!"));
    // The extras type itself contributes fields, not a type of its own.
    assert!(!sdl.contains("type RowExtras"));
    assert!(!sdl.contains("NoExtras"));
}

/// The point of making `total` a resolver: a page that nobody asked to count
/// costs nothing to count.
#[tokio::test]
async fn a_total_nobody_selected_never_runs() {
    let tally = Tally::default();
    let data = answer(&tally, "{ rows { results { name } cursor } }").await;
    assert_eq!(tally.load(Ordering::SeqCst), 0);
    assert!(
        data.to_string().contains("\"next\""),
        "the cursor is still there: {data}"
    );
}

#[tokio::test]
async fn selecting_the_total_runs_the_count_once() {
    let tally = Tally::default();
    let data = answer(&tally, "{ rows { total } }").await;
    assert_eq!(tally.load(Ordering::SeqCst), 1);
    assert!(data.to_string().contains('7'), "{data}");
}

/// Naming the field twice in one query is one count, not two.
#[tokio::test]
async fn asking_for_the_total_twice_counts_once() {
    let tally = Tally::default();
    let data = answer(&tally, "{ rows { total again: total } }").await;
    assert_eq!(tally.load(Ordering::SeqCst), 1);
    assert_eq!(data.to_string().matches('7').count(), 2, "{data}");
}

/// A listing bounded by its own definition knows its count already, and says
/// so without a future to drive.
#[tokio::test]
async fn a_known_total_needs_no_counting() {
    let tally = Tally::default();
    let data = answer(
        &tally,
        "{ notes { results { body } cursor total highlights } }",
    )
    .await;
    assert_eq!(tally.load(Ordering::SeqCst), 0);
    assert!(data.to_string().contains("\"hi\""), "{data}");
}

/// A count larger than the `Int` a client reads it as saturates rather than
/// wrapping into a negative number of runs.
#[tokio::test]
async fn an_enormous_count_saturates() {
    let huge = Total::known(usize::MAX);
    let page: Connection<Row> = Connection::plain(Vec::new(), None, huge);
    let schema = Schema::build(page, EmptyMutation, EmptySubscription).finish();
    let response = schema.execute("{ total }").await;
    assert_eq!(response.errors, Vec::new());
    assert!(response.data.to_string().contains(&i32::MAX.to_string()));
}

/// The default shape has no extras at all, which is what keeps it out of the
/// schema.
#[test]
fn the_default_extras_carry_nothing() {
    assert_eq!(format!("{:?}", NoExtras), "NoExtras");
}

/// One recorded item, standing in for a journal entry.
///
/// The filter is a plain boolean: what these tests are about is where the walk
/// gets to and what the cursor records, not what a filter decides.
#[derive(Debug, PartialEq, Eq)]
struct Entry(&'static str);

impl Filterable for Entry {
    type Filter = bool;

    fn test(&self, filter: &bool, _cx: &MatchCx<'_>) -> Tri {
        Tri::of(*filter)
    }

    fn confirm<'a>(&'a self, filter: &'a bool, _cx: &'a MatchCx<'a>) -> BoxFuture<'a, bool> {
        Box::pin(std::future::ready(*filter))
    }
}

/// The names on one page, and the cursor it ended on.
async fn positions(
    entries: Vec<Entry>,
    descending: bool,
    limit: usize,
    after: Option<&str>,
) -> (Vec<&'static str>, Option<String>) {
    let cx = MatchCx::at(1_788_000_000);
    let page = position_page(
        entries,
        &true,
        &cx,
        PositionQuery {
            digest: "testdigest",
            after,
            descending,
            limit,
        },
    )
    .await
    .expect("the page walks");
    (
        page.items.iter().map(|entry| entry.0).collect(),
        page.cursor.map(|cursor| cursor.0),
    )
}

/// A cursor names where an item sits in what was recorded, whichever way the
/// walk runs.
///
/// A journal grows at the end, so a position counted from the end moves every
/// time the run records something: a descending page two would hand back rows
/// page one already showed. Counted from the start, the position of an item
/// that was already there never changes, and a descending walk resumes below
/// the boundary rather than above it.
#[tokio::test]
async fn a_descending_cursor_survives_the_journal_growing_under_it() {
    let first = vec![Entry("e0"), Entry("e1"), Entry("e2")];
    let (names, cursor) = positions(first, true, 2, None).await;
    assert_eq!(names, vec!["e2", "e1"]);
    let cursor = cursor.expect("there is another page");

    // The run records one more while the client reads.
    let grown = vec![Entry("e0"), Entry("e1"), Entry("e2"), Entry("e3")];
    let (names, cursor) = positions(grown, true, 2, Some(&cursor)).await;
    assert_eq!(
        names,
        vec!["e0"],
        "page two resumes past `e1` rather than at it"
    );
    assert!(cursor.is_none(), "the walk reached the end");
}

/// The ascending walk resumes past the position it recorded, and the cursor
/// minted one way round is not a cursor for the other.
#[tokio::test]
async fn an_ascending_cursor_resumes_where_it_left_off() {
    let entries = || vec![Entry("e0"), Entry("e1"), Entry("e2")];
    let (names, cursor) = positions(entries(), false, 2, None).await;
    assert_eq!(names, vec!["e0", "e1"]);
    let cursor = cursor.expect("there is another page");
    let (names, next) = positions(entries(), false, 2, Some(&cursor)).await;
    assert_eq!(names, vec!["e2"]);
    assert!(next.is_none());

    // The direction is part of the cursor's identity.
    let cx = MatchCx::at(1_788_000_000);
    let refused = position_page(
        entries(),
        &true,
        &cx,
        PositionQuery {
            digest: "testdigest",
            after: Some(&cursor),
            descending: true,
            limit: 2,
        },
    )
    .await
    .err()
    .expect("a cursor minted ascending is not a descending one");
    assert_eq!(refused.code(), "BAD_USER_INPUT");
}
