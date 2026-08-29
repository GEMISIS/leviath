//! Editing an agent blueprint (`agent.leviath`) as a document.
//!
//! The dashboard's agent editor needs to change one thing about a manifest at
//! a time and hand back a file the author would still recognise: their
//! comments, their key order, their formatting. Nothing in the tree could do
//! that. `parse_manifest` reads a manifest into a [`Blueprint`], but there is
//! no writer for one, and `toml` re-emits from values, so every comment a
//! bundled agent carries would vanish on the first edit. So the editing model
//! keeps the manifest as a `toml_edit` document, the one source of truth, and
//! derives typed views from it on demand: [`ManifestDoc::stages`],
//! [`ManifestDoc::edges`], [`ManifestDoc::effective_regions`]. Keys the views
//! do not surface (fan-out policies the editor leaves alone, `[compaction]`,
//! table-shaped seeds, custom gates) round-trip untouched.
//!
//! The mutators refuse rather than produce a broken document: a name that is
//! not a name, a duplicate, a transition to a stage that does not exist, the
//! last stage deleted, all come back as an [`EditError`] and leave the
//! document as it was. What they cannot refuse (a stage with no model, an
//! unreachable stage) is [`check`]'s job, which runs the same parse, validate
//! and lint pass `lev validate` and the daemon's `POST /api/blueprints/validate`
//! run.
//!
//! The rules match The Lair's editor field for field (`blueprint/editable.ts`
//! in the leviath.dev repo), so an agent built here and one built there are
//! the same file: an empty string deletes a key rather than writing `""`; a
//! `direct` transform is written as absent; a new path is `hint = "Continue
//! here when appropriate"`; a new region is `pinned`, `5%`, `4000` tokens.
//!
//! [`Blueprint`]: leviath_core::Blueprint

pub(crate) mod catalog;
pub(crate) mod check;
mod doc;
mod edges;
mod layout_store;
mod order;
mod regions;
mod stages;
mod tables;
pub(crate) mod templates;

pub(crate) use doc::{
    EdgeKind, EdgeView, ManifestDoc, RegionView, StageModeView, TransformKind, WorkerKind,
};
#[cfg(test)]
pub(crate) use doc::{FanOutView, ToolRouting};
pub(crate) use edges::Rule;
pub(crate) use layout_store::{LayoutStore, Positions};
pub(crate) use regions::{RegionField, RegionScope, RegionValue};
pub(crate) use stages::{FanOutField, StageText};

/// Why an edit was refused. The document is untouched whenever one of these
/// comes back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum EditError {
    /// The text is not TOML.
    #[error("not valid TOML: {0}")]
    Toml(String),
    /// The manifest has no `[agent]` table.
    #[error("the manifest has no [agent] table")]
    NoAgent,
    /// The manifest has no `[stages.<name>]` table at all.
    #[error("the manifest has no stages")]
    NoStages,
    /// A stage, region or agent name outside the runtime's charset.
    #[error("\"{0}\" will not work as a name: letters, digits, `.`, `_` and `-` only")]
    BadName(String),
    /// A stage or region of that name already exists where it would go.
    #[error("\"{0}\" is already taken")]
    Taken(String),
    /// No stage of that name.
    #[error("there is no stage named \"{0}\"")]
    NoSuchStage(String),
    /// No region of that name in that layout.
    #[error("there is no region named \"{0}\"")]
    NoSuchRegion(String),
    /// No transition from the first stage to the second.
    #[error("there is no path from \"{0}\" to \"{1}\"")]
    NoSuchEdge(String, String),
    /// Deleting this stage would leave the agent with none.
    #[error("an agent needs at least one stage")]
    LastStage,
    /// The key exists but holds something other than a table, and the editor
    /// will not clobber what it cannot display.
    #[error("`{0}` is not a table this editor can write into")]
    NotATable(String),
    /// A number outside what the key accepts.
    #[error("{0}")]
    OutOfRange(String),
}

/// Whether `name` can be a stage, region or agent name: the charset the
/// runtime accepts, and what The Lair's editor enforces.
pub(crate) fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A name check that says why it failed.
pub(crate) fn require_name(name: &str) -> Result<(), EditError> {
    if is_valid_name(name) {
        Ok(())
    } else {
        Err(EditError::BadName(name.to_string()))
    }
}

#[cfg(test)]
mod tests;
