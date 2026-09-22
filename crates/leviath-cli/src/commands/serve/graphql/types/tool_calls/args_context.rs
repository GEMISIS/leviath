//! Arguments for the tools that work on the run's own context window.
//!
//! The checklist tools live here too: they write into a region like the rest,
//! and their ids are positions in one.

use async_graphql::SimpleObject;
use leviath_graphql_derive::mirror;
use serde::Deserialize;

/// Arguments for the `context_write` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct ContextWriteArgs {
    /// The region written to, by name.
    pub(crate) region: String,
    /// The entry's key. An entry of the same key is replaced.
    #[serde(default)]
    pub(crate) key: Option<String>,
    /// What was stored.
    pub(crate) content: String,
}

/// Arguments for the `context_attach` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct ContextAttachArgs {
    /// The region the part goes in, by name.
    pub(crate) region: String,
    /// The file, relative to the working directory.
    pub(crate) path: String,
    /// The entry's key. An entry of the same key is replaced.
    #[serde(default)]
    pub(crate) key: Option<String>,
    /// Text stored beside the part, describing it.
    #[serde(default)]
    pub(crate) caption: Option<String>,
    /// The mime type the model named, for a file whose bytes and name do not say.
    #[serde(default, rename = "type")]
    #[graphql(name = "type")]
    pub(crate) mime_type: Option<String>,
    /// How the part reaches the model: `native`, `text` or `stand_in`. Kept as
    /// the word the model sent rather than a checked value, because a model that
    /// invents a fourth one is a thing worth being able to see.
    #[serde(default)]
    pub(crate) deliver: Option<String>,
}

/// Arguments for the `context_export` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct ContextExportArgs {
    /// The part's file name, or the start of its sha256.
    pub(crate) name: String,
    /// Where it was written, relative to the working directory. Left out means
    /// the part's own file name.
    #[serde(default)]
    pub(crate) path: Option<String>,
}

/// Arguments for the `context_append` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct ContextAppendArgs {
    /// The region appended to, by name.
    pub(crate) region: String,
    /// A name for this entry, which is what lets it be released later.
    #[serde(default)]
    pub(crate) key: Option<String>,
    /// What was appended.
    pub(crate) content: String,
}

/// Arguments for the `context_read` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct ContextReadArgs {
    /// The region read, by name.
    pub(crate) region: String,
    /// One entry's key, where the model asked for a single entry.
    #[serde(default)]
    pub(crate) key: Option<String>,
    /// One entry's position instead, counting from the oldest.
    #[serde(default)]
    pub(crate) index: Option<i32>,
}

/// Arguments for the `context_delete` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct ContextDeleteArgs {
    /// The region released from, by name.
    pub(crate) region: String,
    /// The entry's key, where it has one.
    #[serde(default)]
    pub(crate) key: Option<String>,
    /// The entry's position instead, counting from 0 as the oldest.
    #[serde(default)]
    pub(crate) index: Option<i32>,
    /// Release this many of the oldest entries, instead of naming one.
    #[serde(default)]
    pub(crate) oldest: Option<i32>,
}

/// Arguments for the `context_list` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct ContextListArgs {
    /// One region to list, or left out for all of them.
    #[serde(default)]
    pub(crate) region: Option<String>,
}

/// Arguments for the `todo_add` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct TodoAddArgs {
    /// The checklist region, by name.
    pub(crate) region: String,
    /// What needs doing, in one line.
    pub(crate) item: String,
}

/// Arguments for the `todo_done` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct TodoDoneArgs {
    /// The checklist region, by name.
    pub(crate) region: String,
    /// The item's id, as `todo_add` gave it.
    pub(crate) id: i32,
}

/// Arguments for the `todo_note` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct TodoNoteArgs {
    /// The checklist region, by name.
    pub(crate) region: String,
    /// The item's id.
    pub(crate) id: i32,
    /// The note recorded against it.
    pub(crate) note: String,
}
