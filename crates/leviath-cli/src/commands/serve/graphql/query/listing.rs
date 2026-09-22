//! The five steps every root listing takes, written once.
//!
//! A listing collects its items, sifts them through the mirror the client
//! wrote, walks them in the order it asked for and hands back one page. Only
//! the first of those four is different from listing to listing, so only the
//! first is written per field: everything after it is [`connection`], and a new
//! root listing is a resolver that gathers its items and calls this.
//!
//! The order matters as much as the filter, because both go into the cursor's
//! digest: a cursor minted under one filter and order is refused by any other,
//! which is what stops a walk changing what it is walking halfway through.

use async_graphql::{InputType, OutputType};

use super::super::super::cursor;
use super::super::connection::{Connection, Paged, Total};
use super::super::error::IntoGraphql;
use super::super::filter::{Filterable, MatchCx, Sifted};
use super::super::paging::digest::canonical;
use super::super::paging::order::{Order, OrderField, Orderable, Term};
use super::super::paging::page::page;
use super::super::paging::walk::walk;
use super::super::scalars::Cursor;

/// How much of a listing one request asks for.
///
/// The cap is per listing because the listings are not the same size: a
/// catalogue of tens and a store of thousands want different answers to "you
/// asked for too much", and the refusal names which cap it hit.
pub(crate) struct Window {
    /// The page size the client asked for.
    pub(crate) first: i32,
    /// The cursor the previous page handed back, if this is not the first.
    pub(crate) after: Option<Cursor>,
    /// The most this listing serves in one page.
    pub(crate) cap: usize,
    /// What that cap is called in the refusal, as the tail of a sentence.
    pub(crate) cap_name: &'static str,
}

/// The sort terms a request runs under: what it asked for, or the listing's
/// own order when it asked for nothing.
///
/// An omitted `orderBy` and an empty one ask the same thing, and both get the
/// order the listing is read in without a filter.
pub(crate) fn terms<O, F>(
    asked: Option<Vec<O>>,
    term: fn(O) -> Term<F>,
    fallback: fn() -> Vec<Term<F>>,
) -> Vec<Term<F>> {
    let asked = asked.unwrap_or_default();
    match asked.is_empty() {
        true => fallback(),
        false => asked.into_iter().map(term).collect(),
    }
}

/// One page of `items`, filtered, ordered and cursored.
///
/// `id` is the item's own identity, which is what makes the order total: two
/// items with the same sort key still have one place each, so a cursor can
/// resume between them.
pub(crate) async fn connection<T, F>(
    items: Vec<T>,
    filter: Option<T::Filter>,
    terms: Vec<Term<F>>,
    id: fn(&T) -> String,
    window: Window,
) -> async_graphql::Result<Connection<T>>
where
    T: Filterable + Paged + OutputType + Send + Sync + 'static,
    T: for<'a> Orderable<MatchCx<'a>, Field = F>,
    T::Filter: InputType + Default + Send + Sync + 'static,
    F: OrderField + Send + Sync + 'static,
{
    let limit = page(window.first, window.cap, window.cap_name).gql()?;
    let filter = filter.unwrap_or_default();
    // Taken over what the client wrote rather than over anything compiled, so
    // an empty filter digests as nothing at all and its cursor interchanges
    // with the unfiltered REST one.
    let rendered = canonical(&filter).gql()?;
    let digest = cursor::filter_digest(&[rendered.as_str()]);

    let order = Order::new(terms);
    let resume = match window.after {
        None => None,
        Some(ref token) => Some(order.decode(&token.0, &digest).gql()?),
    };

    let sift = Sifted::new(
        filter,
        MatchCx::at(leviath_core::duration::now_secs()),
        order,
        id,
    );
    let mut walked = walk(sift, items, resume.as_ref(), limit).await;
    let results = std::mem::take(&mut walked.items);
    let cursor = walked
        .next
        .clone()
        .map(|position| Cursor(walked.sift().order().encode(&digest, &position)));
    // Counting settles every item the page did not reach, so it runs only where
    // the client selected `total`.
    Ok(Connection::plain(
        results,
        cursor,
        Total::lazy(async move { walked.total().await }),
    ))
}
