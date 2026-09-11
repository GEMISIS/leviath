//! The wire shape of a blueprint's per-stage typed-content routing.
//!
//! In a file of its own rather than beside the rest of the blueprint response
//! types so `types.rs` stays under its production-line cap.

use serde::Serialize;

/// One mime pattern routed to a region, in a [`StageRoutingInfo`].
#[derive(Debug, Serialize)]
pub(super) struct RoutePair {
    /// The mime pattern the model's produced parts are matched against
    /// (`image/*`, `application/pdf`, `*/*`).
    pub(super) pattern: String,
    /// The region a matching part is written to.
    pub(super) region: String,
}

/// A stage's typed-content routing, so a console need not parse the manifest
/// to show or check it. Only stages that route produced parts or reset a
/// region on entry appear; a stage with neither is left out.
#[derive(Debug, Serialize)]
pub(super) struct StageRoutingInfo {
    /// The stage's name.
    pub(super) stage: String,
    /// `output_routing`: where the model's produced parts go, by mime pattern,
    /// ordered by pattern. Empty (and omitted) when the stage routes none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) output_routing: Vec<RoutePair>,
    /// `context.reset`: the regions the stage empties on entry. Empty (and
    /// omitted) when it resets none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) context_reset: Vec<String>,
}
