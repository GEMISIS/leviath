//! The run graph: stages, the edges between them, and the context layout.
//!
//! This is the vocabulary a spawn is written in. It lives beside the ECS world
//! that runs it, above the leaf values in `leviath-core` that tools and
//! providers share.

pub mod env;
pub mod graph;
pub mod inputs;
pub mod issues;
pub mod launch;
pub mod names;
pub mod readable;
pub mod request;
pub mod run_spec;
pub mod summary;
