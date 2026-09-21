//! The object types the schema exposes.
//!
//! One module per concept, and one type per concept: a region is a region
//! whether it is read from a blueprint or from a live run, and a blueprint is
//! a blueprint whether it is the installed definition or the copy a run
//! executed.

pub(crate) mod blueprint;
pub(crate) mod catalog;
pub(crate) mod context_change;
pub(crate) mod execution;
pub(crate) mod inference;
pub(crate) mod interaction;
pub(crate) mod machine;
pub(crate) mod manifest;
pub(crate) mod run;
pub(crate) mod run_detail;
pub(crate) mod run_files;
pub(crate) mod tool_calls;
pub(crate) mod update;

#[cfg(test)]
#[path = "execution_tests.rs"]
mod execution_tests;

#[cfg(test)]
#[path = "run_files_tests.rs"]
mod run_files_tests;
