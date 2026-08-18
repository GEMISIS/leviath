//! Stage graphs on a canvas.
//!
//! An agent blueprint is a graph: stages are nodes, transitions are edges,
//! and a run walks it. Every surface that shows that shape draws it through
//! this module, so the stage explorer, the detail view's graph band, the
//! new-run preview and `lev validate --graph` agree on what a stage looks
//! like and where it sits.
//!
//! - [`model`] reads a [`leviath_core::Blueprint`] into a [`StageGraph`]:
//!   both manifest shapes, fan-out hand-offs, self-loops as badges.
//! - [`layout`] places it on layers, deterministically.
//! - [`content`] draws a node and picks an edge stroke.
//! - [`view`] wraps the rataflow canvas: keys, mouse, and the live overlay
//!   that paints a run onto the blueprint.
//! - [`text`] renders the same canvas once into plain text.

pub(crate) mod content;
pub(crate) mod layout;
pub(crate) mod model;
pub(crate) mod text;
pub(crate) mod view;

pub(crate) use content::{NodeStyle, RunPhase, WorkerCounts};
pub(crate) use model::StageGraph;
pub(crate) use view::{FlowView, LiveOverlay, Selection, StageLive};
