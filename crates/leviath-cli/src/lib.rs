//! Leviath CLI library - re-exports for integration tests.
//!
//! # Why `missing_docs` is off here
//!
//! It is on for the whole workspace, because the other crates publish a library
//! whose docs render on docs.rs and get called by people who did not write them.
//! This crate publishes the `lev` **binary**. Its `pub` surface is `pub` so the
//! integration tests in `tests/` can reach it, not because anything outside
//! calls it - `execute(args: AddArgs)` is `lev add`, and a doc comment saying so
//! restates the module path.
//!
//! The 166 items that would be flagged here are mostly clap `Args` fields, which
//! already carry their user-facing text in `#[arg(help = ...)]` or a doc comment
//! clap consumes. Requiring a second description for the rustdoc reader who does
//! not exist is the noise this repo's comment policy is written to keep out.
//!
//! Tracked in #290 if that reasoning stops holding.
#![allow(missing_docs)]

pub mod approvals;
pub mod bundled;
pub mod commands;
pub mod config;
pub mod credentials;
pub mod daemon;
pub mod dispatch;
pub mod held_checkpoints;
pub mod lint;
pub mod logging;
pub mod read_path_report;
pub mod render;
pub mod runstate;
pub mod shell_keys;
#[cfg(test)]
mod test_support;
pub mod tools;
pub mod tui;
pub mod workdir_guard;
