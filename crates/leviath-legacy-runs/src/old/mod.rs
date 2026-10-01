//! The parsed form of an old `agent.leviath` manifest, and how it reads as a
//! run graph.
//!
//! [`parse_manifest`](crate::manifest::parse_manifest) reads a manifest into a
//! [`Blueprint`](blueprint::Blueprint), and [`graph::from_blueprint`] reads
//! that as the [`RunGraph`](leviath_runtime::spec::graph::RunGraph) a run is
//! made from.

pub(crate) mod blueprint;
pub(crate) mod graph;
pub(crate) mod layout;
