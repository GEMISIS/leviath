//! The manifest in detail: everything a blueprint declares beyond its outline.
//!
//! Split from `types/blueprint.rs` by subject rather than by size. A stage's
//! model block, its tool routing, its checkpoints and its output shape are
//! four unrelated things that happen to hang off one struct, and reading one of
//! them should not mean scrolling past the other three.
//!
//! Two rules hold across every type here, because the manifest is a document
//! rather than a database.
//!
//! A reference the manifest guarantees resolves is an object: an edge's target
//! stage exists, because a blueprint naming a stage it does not declare is
//! refused at load. A reference that may dangle stays a name: a gate may name a
//! region a later edit removed, and a stage may name a tool this machine does
//! not have. Resolving those into objects would drop them, and a gate that
//! quietly disappears from a client's view reads as a gate that was never
//! written.
//!
//! A setting the manifest leaves out is null rather than its default. What the
//! author wrote and what the daemon resolved are different questions, and
//! `Stage.effective` answers the second.

pub(crate) mod dependency;
pub(crate) mod interaction;
pub(crate) mod mime;
pub(crate) mod model;
pub(crate) mod output;
pub(crate) mod region;
pub(crate) mod runtime;
pub(crate) mod stage;
pub(crate) mod tools;
pub(crate) mod transition;

/// Narrow a count to the 32 bits GraphQL's `Int` carries.
///
/// Saturating rather than wrapping: these are manifest numbers, so a value near
/// `i32::MAX` is a typo, and the largest representable number reads as one
/// where a negative would read as a different setting.
pub(crate) fn count(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
#[path = "stage_tests.rs"]
mod stage_tests;

#[cfg(test)]
#[path = "blueprint_detail_tests.rs"]
mod blueprint_detail_tests;

#[cfg(test)]
#[path = "conversion_tests.rs"]
mod conversion_tests;
