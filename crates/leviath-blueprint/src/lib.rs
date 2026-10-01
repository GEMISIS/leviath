//! # Leviath Blueprint
//!
//! A blueprint is a named run graph, kept in an `agent.toml` beside the files
//! it uses. It sits on top of the runtime: a spawn request either carries a
//! whole [`RunGraph`](leviath_runtime::spec::graph::RunGraph) or names a
//! blueprint, and a blueprint's graph is that same type. The only way a caller
//! changes a blueprint's run is through the inputs its graph declares.
//!
//! ```toml
//! [blueprint]
//! name = "coder"
//! version = "1.2.0"
//! description = "Plans, writes and checks a change."
//!
//! [graph]
//! # a RunGraph, exactly as serde writes it
//! ```
//!
//! - [`load`] and [`validate`] read one file; [`find`] finds an installed
//!   blueprint by name, for a host's `ResolveEnv::blueprint`.
//! - [`expand`] turns a blueprint reference and its inputs into a request.
//! - [`lint`] is what the checks beyond validation report in.

mod expand;
mod file;
pub mod lint;
mod load;
mod write;

pub use expand::expand;
pub use file::{BlueprintFile, BlueprintMeta, FILE_NAME};
pub use leviath_runtime::spec::env::LoadedBlueprint;
pub use load::{BlueprintError, find, installed, load, validate};

#[cfg(test)]
mod tests;
