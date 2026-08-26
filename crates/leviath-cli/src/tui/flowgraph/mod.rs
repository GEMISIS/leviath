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
//! - [`path`] turns a run's visit timeline into a graph of its own: one node
//!   per visit, chained in the order they happened.
//! - [`layout`] places it on layers, deterministically, or snakes a path
//!   across rows.
//! - [`content`] draws a node and picks an edge stroke.
//! - [`snake`] is the snaking layout's geometry: row width, box spacing,
//!   where an edge attaches.
//! - [`view`] wraps the rataflow canvas: keys, mouse, and the live overlay
//!   that paints a run onto the blueprint.
//! - [`text`] renders the same canvas once into plain text.

pub(crate) mod content;
pub(crate) mod layout;
pub(crate) mod model;
pub(crate) mod path;
pub(crate) mod snake;
pub(crate) mod text;
pub(crate) mod view;

pub(crate) use content::{RunPhase, WorkerCounts};
pub(crate) use layout::Direction;
pub(crate) use model::StageGraph;
pub(crate) use snake::{snake_per_row, snake_row_pitch};
pub(crate) use view::{CanvasEvent, FlowView, LiveOverlay, MenuTarget, Selection, StageLive};
