//! Leviath CLI library - re-exports for integration tests.
//!
//! `missing_docs` applies here like everywhere else in the workspace. This crate
//! ships the `lev` binary rather than a library anyone calls, so the case for
//! exempting it was real - and the case against turned out to be stronger: the
//! `pub` surface is what the integration tests drive, and a wire type like
//! `commands::serve::ServerEvent` is read by clients that never see this
//! source. An undocumented field there is a gap for somebody.
//!
//! What the rule asks for is a sentence saying something the name does not. A
//! clap `Args` field already carries its user-facing text for `--help`; the
//! struct around it says which command it belongs to.

pub(crate) mod approvals;
pub(crate) mod blueprint_edit;
pub(crate) mod bundled;
pub mod commands;
pub mod config;
pub mod credentials;
pub mod daemon;
pub mod dispatch;
pub(crate) mod held_checkpoints;
pub(crate) mod lint;
pub mod logging;
pub(crate) mod read_path_report;
pub(crate) mod render;
pub mod runstate;
pub(crate) mod shell_keys;
#[cfg(test)]
mod test_support;
pub(crate) mod tool_inventory;
pub(crate) mod tools;
pub(crate) mod tui;
pub mod ui_state;
pub mod workdir_guard;
