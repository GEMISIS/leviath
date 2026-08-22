//! Shared ratatui widgets for every Leviath TUI surface.
//!
//! Each widget here exists because at least two surfaces were carrying their
//! own copy (popups, help overlays, confirm dialogs, footers, list cursors,
//! line editors) and the copies had already drifted apart. One implementation,
//! one behavior.

pub(crate) mod confirm;
pub(crate) mod footer;
pub(crate) mod help;
pub(crate) mod line_edit;
pub(crate) mod markdown_edit;
pub(crate) mod picker;
pub(crate) mod popup;
pub(crate) mod scroll;
