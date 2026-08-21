//! Leviath CLI library - re-exports for integration tests.
//!
//! `missing_docs` applies here like everywhere else in the workspace. This crate
//! ships the `lev` binary rather than a library anyone calls, so the case for
//! exempting it was real - and the case against turned out to be stronger: the
//! `pub` surface is what the integration tests drive, and a wire type like
//! [`commands::serve::ServerEvent`] is read by clients that never see this
//! source. An undocumented field there is a gap for somebody.
//!
//! What the rule asks for is a sentence saying something the name does not. A
//! clap `Args` field already carries its user-facing text for `--help`; the
//! struct around it says which command it belongs to.

pub mod approvals;
pub mod blueprint_edit;
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
pub mod tool_inventory;
pub mod tools;
pub mod tui;
pub mod ui_state;
pub mod workdir_guard;
