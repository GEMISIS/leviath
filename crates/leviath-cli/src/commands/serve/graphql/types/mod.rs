//! The object types the schema exposes.
//!
//! One module per concept, and one type per concept: a region is a region
//! whether it is read from a blueprint or from a live run, and a blueprint is
//! a blueprint whether it is the installed definition or the copy a run
//! executed.

pub(crate) mod blueprint;
pub(crate) mod run;
