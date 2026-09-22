//! One directory listing, for a file picker.

use async_graphql::SimpleObject;
use leviath_graphql_derive::mirror;

/// One directory, for a file picker.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct Directory {
    /// The absolute directory that was listed.
    pub(crate) path: String,
    /// Where "up one level" goes. Null at the filesystem root, and at the
    /// workdir root: a picker is never led above the fence.
    pub(crate) parent: Option<String>,
    /// The user's home directory, for a "home" shortcut.
    pub(crate) home: String,
    /// This server's own working directory, for a "here" shortcut.
    pub(crate) cwd: String,
    /// The directories inside, by name.
    pub(crate) entries: Vec<String>,
}
