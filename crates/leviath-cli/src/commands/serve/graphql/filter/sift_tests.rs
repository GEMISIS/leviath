//! Tests for the adapter between a mirror and the walk.
//!
//! The types here carry a field that costs a read, which the blueprint
//! catalogue does not, so all three verdicts are reachable and the walk's
//! second phase runs.

use async_graphql::Object;
use leviath_graphql_derive::mirror;

use super::super::super::paging::order::{Order, OrderDirection, Term};
use super::super::super::paging::walk::{Sift, Verdict, walk};
use super::super::scalars::StringFilter;
use super::super::{MatchCx, OrderField, Tri};
use super::{Sifted, verdict};

/// A file whose size is in memory and whose contents are not.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    /// The name, which is also the id the order breaks ties on.
    pub(crate) name: String,
    /// How large it is.
    pub(crate) size: i32,
    /// What it holds, which the walk only reads where it has to.
    pub(crate) body: String,
}

/// Read an entry's contents, slowly enough to be a second-phase field.
async fn read_body(entry: &Entry) -> String {
    std::future::ready(()).await;
    entry.body.clone()
}

#[mirror]
#[Object]
impl Entry {
    /// The entry's name.
    #[filter(orderable)]
    async fn name(&self) -> &str {
        &self.name
    }

    /// How large the entry is.
    #[filter(orderable)]
    async fn size(&self) -> i32 {
        self.size
    }

    /// What the entry holds.
    #[filter(io)]
    async fn body(&self) -> String {
        read_body(self).await
    }
}

/// A few entries to walk.
fn entries() -> Vec<Entry> {
    ["one", "two", "three"]
        .into_iter()
        .enumerate()
        .map(|(at, name)| Entry {
            name: name.to_string(),
            size: i32::try_from(at).unwrap_or(0),
            body: format!("{name} body"),
        })
        .collect()
}

/// The listing one filter and one order walk with.
fn sifted(filter: EntryFilter, direction: OrderDirection) -> Sifted<Entry, EntryOrderField> {
    Sifted::new(
        filter,
        MatchCx::at(0),
        Order::new(vec![Term {
            field: EntryOrderField::Name,
            direction,
        }]),
        |entry| entry.name.clone(),
    )
}

/// A filter naming one entry's name.
fn named(name: &str) -> EntryFilter {
    EntryFilter {
        name: Some(Box::new(StringFilter {
            eq: Some(name.to_string()),
            ..StringFilter::default()
        })),
        ..EntryFilter::default()
    }
}

/// Each answer the cheap pass gives is one the walk reads the same way.
#[test]
fn every_answer_reaches_the_walk_unchanged() {
    assert_eq!(verdict(Tri::Yes), Verdict::Keep);
    assert_eq!(verdict(Tri::No), Verdict::Drop);
    assert_eq!(verdict(Tri::Io), Verdict::NeedsIo);
}

/// A cheap filter settles every entry without the second phase.
#[test]
fn a_cheap_filter_settles_in_the_first_phase() {
    let listing = sifted(named("two"), OrderDirection::Asc);
    let all = entries();
    assert_eq!(listing.test(&all[0]), Verdict::Drop);
    assert_eq!(listing.test(&all[1]), Verdict::Keep);
    assert_eq!(listing.position(&all[1]).id, "two");
    assert_eq!(listing.descending(), [false]);
    assert_eq!(listing.order().sort(), "name");
}

/// A filter on a field that costs a read is settled by the walk, not before.
#[tokio::test]
async fn a_read_costing_filter_is_settled_by_the_walk() {
    let filter = EntryFilter {
        body: Some(Box::new(StringFilter {
            eq: Some("two body".to_string()),
            ..StringFilter::default()
        })),
        ..EntryFilter::default()
    };
    let listing = sifted(filter, OrderDirection::Asc);
    let all = entries();
    assert_eq!(listing.test(&all[0]), Verdict::NeedsIo);
    assert!(!listing.confirm(&all[0]).await);
    assert!(listing.confirm(&all[1]).await);

    let listing = sifted(
        EntryFilter {
            body: Some(Box::new(StringFilter {
                eq: Some("two body".to_string()),
                ..StringFilter::default()
            })),
            ..EntryFilter::default()
        },
        OrderDirection::Asc,
    );
    let page = walk(listing, entries(), None, 10).await;
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].name, "two");
    assert!(page.next.is_none());
}

/// The order the listing was given is the order the walk runs in.
#[tokio::test]
async fn the_walk_runs_in_the_order_it_was_given() {
    let page = walk(
        sifted(EntryFilter::default(), OrderDirection::Desc),
        entries(),
        None,
        10,
    )
    .await;
    let names: Vec<&str> = page.items.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["two", "three", "one"]);
    assert_eq!(EntryOrderField::Size.wire(), "size");
}
