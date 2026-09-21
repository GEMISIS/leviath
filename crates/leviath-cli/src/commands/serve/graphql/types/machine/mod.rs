//! The machine this server is on: how it is configured, whether it is healthy,
//! and what is installed on it.
//!
//! These are the answers a settings screen and a diagnostics view need. None of
//! them is a run, and none grows without bound, so they are plain values and
//! plain lists.
//!
//! One file per concern: `config` for the daemon's own configuration and its
//! diagnostics, `mcp` for the servers it can reach, `yolo` for the unattended
//! profiles, `mime` for the type registry, `script` for the registered
//! scripts, `journal` for what the daemon has recorded, and `directory` for
//! the file picker. Every type is re-exported here under its old name, so a
//! resolver elsewhere still reads `types::machine::X`.

pub(crate) mod config;
pub(crate) mod directory;
pub(crate) mod journal;
pub(crate) mod mcp;
pub(crate) mod mime;
pub(crate) mod script;
pub(crate) mod yolo;

pub(crate) use config::*;
pub(crate) use directory::*;
pub(crate) use journal::*;
pub(crate) use mcp::*;
pub(crate) use mime::*;
pub(crate) use script::*;
pub(crate) use yolo::*;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
