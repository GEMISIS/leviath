//! What every listing needs and none of them should own a copy of.
//!
//! A listing is four decisions - how big a page may be, what order it runs in,
//! which filter the cursor is bound to, and how much work reaching the page
//! costs - and each one used to be answered again per field. The four copies of
//! the page-size check disagreed on their messages, the cursor digest was a
//! `{:?}` of whatever the matcher happened to look like, and a filter that had
//! to read a file was capped so it would not read too many.
//!
//! This module answers all four once:
//!
//! | File | Answers |
//! |---|---|
//! | [`page`] | how large a page may be, and what a refusal says |
//! | [`order`] | what `orderBy` compiles to: a comparison, and the `sort` a cursor records |
//! | [`digest`] | the canonical text a filter digests to, and the size a filter may not exceed |
//! | [`walk`] | the five-step lazy walk that turns a snapshot into one page |
//!
//! The walk is the part worth reading first. It tests every item without
//! opening a file, sorts and seeks past the cursor in memory, and only then
//! reads anything - for the items this page actually reaches, and no others.
//! That is what lets a filter be as selective as it likes without a scan cap
//! to apologise for.

pub(crate) mod digest;
pub(crate) mod order;
pub(crate) mod page;
pub(crate) mod walk;
