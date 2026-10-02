//! A run's file, typed: the spec it started from, its state at any step, the
//! steps themselves, and its graph with the edges it took.
//!
//! Every type here is a conversion from the runtime's own, so what a client
//! reads is what the run file holds, field for field. The graph a run was
//! given is the one exception: it is the caller's own document, written in the
//! same JSON `spawnRun` takes it in, so it travels as `JSON`.

pub(crate) mod context;
pub(crate) mod delta;
pub(crate) mod files;
pub(crate) mod graph;
pub(crate) mod journal;
pub(crate) mod read;
pub(crate) mod spec;
pub(crate) mod state;
pub(crate) mod values;

use super::super::scalars::BigInt;

/// A count the runtime keeps as `u32`, as a GraphQL `Int`. No count a run
/// keeps comes near the cap, and one that did would read as the cap.
pub(crate) fn saturating(n: u32) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

/// A count the runtime keeps as `u64`, as a `BigInt`.
pub(crate) fn big(n: u64) -> BigInt {
    BigInt(i64::try_from(n).unwrap_or(i64::MAX))
}

/// A run graph as the JSON a raw spawn request carries it in.
pub(crate) fn json_of(graph: &leviath_runtime::spec::graph::RunGraph) -> serde_json::Value {
    serde_json::to_value(graph).expect("a run graph is plain data")
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
