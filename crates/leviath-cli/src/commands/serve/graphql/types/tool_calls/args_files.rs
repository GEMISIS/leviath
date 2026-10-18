//! Arguments for the tools that touch files, the shell and the tool directory.
//!
//! Every field here mirrors one key of the tool's own declared schema, name for
//! name, and is optional here exactly where the schema leaves it out of
//! `required`. A test holds the two in step, because a field that drifted would
//! quietly turn every call of that tool into an untyped one.

use async_graphql::SimpleObject;
use leviath_graphql_derive::mirror;
use serde::Deserialize;

/// Arguments for the `read_file` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct ReadFileArgs {
    /// Path to the file, relative to the working directory.
    pub(crate) path: String,
}

/// Arguments for the `write_file` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct WriteFileArgs {
    /// Path to the file, relative to the working directory.
    pub(crate) path: String,
    /// The content to write: the whole file, or with `append` the part to add.
    pub(crate) content: String,
    /// Add to the end of the file instead of replacing it. Left out means no.
    #[serde(default)]
    pub(crate) append: Option<bool>,
}

/// Arguments for the `edit_file` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct EditFileArgs {
    /// Path to the file, relative to the working directory.
    pub(crate) path: String,
    /// The exact string to replace, which must appear once in the file.
    pub(crate) old_str: String,
    /// What replaces it.
    pub(crate) new_str: String,
}

/// Arguments for the `list_dir` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct ListDirArgs {
    /// Path to the directory, relative to the working directory. Left out means
    /// the working directory itself.
    #[serde(default)]
    pub(crate) path: Option<String>,
}

/// Arguments for the `read_files` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct ReadFilesArgs {
    /// The file paths, relative to the working directory.
    pub(crate) paths: Vec<String>,
}

/// Arguments for the `shell` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct ShellArgs {
    /// The command, as the model wrote it.
    pub(crate) command: String,
}

/// Arguments for the `which_command` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct WhichCommandArgs {
    /// The program name to look up, such as `git` or `python3`.
    pub(crate) command: String,
}

/// Arguments for `install_self_tool`, which writes into the blueprint's own
/// `tools/` directory.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct InstallSelfToolArgs {
    /// The tool's name, which must match the script's own `@tool` directive.
    pub(crate) name: String,
    /// The whole Rhai source of the script.
    pub(crate) source: String,
    /// Replace a script of the same name. Left out means no.
    #[serde(default)]
    pub(crate) overwrite: Option<bool>,
}

/// Arguments for `install_global_tool`, which writes into the machine-wide
/// tools directory. The same three arguments; what differs is who sees the
/// result.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct InstallGlobalToolArgs {
    /// The tool's name, which must match the script's own `@tool` directive.
    pub(crate) name: String,
    /// The whole Rhai source of the script.
    pub(crate) source: String,
    /// Replace a script of the same name. Left out means no.
    #[serde(default)]
    pub(crate) overwrite: Option<bool>,
}

/// Arguments for the `search_files` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct SearchFilesArgs {
    /// What to look for: a regular expression, or plain text when `regex` is
    /// false.
    pub(crate) pattern: String,
    /// The directory or file to search, relative to the working directory.
    /// Left out means the working directory itself.
    #[serde(default)]
    pub(crate) path: Option<String>,
    /// Only files whose path matches this glob are searched.
    #[serde(default)]
    pub(crate) glob: Option<String>,
    /// Read `pattern` as a regular expression. Left out means yes.
    #[serde(default)]
    pub(crate) regex: Option<bool>,
    /// Match regardless of case. Left out means no.
    #[serde(default)]
    pub(crate) case_insensitive: Option<bool>,
    /// Lines shown before and after each match. Left out means none.
    #[serde(default)]
    pub(crate) context_lines: Option<i32>,
    /// Most matching lines returned.
    #[serde(default)]
    pub(crate) max_results: Option<i32>,
    /// Also search what `.gitignore`, `.ignore` or a leading dot hides.
    #[serde(default)]
    pub(crate) include_ignored: Option<bool>,
}

/// Arguments for the `glob` tool.
#[mirror(no_filter)]
#[derive(Debug, Deserialize, SimpleObject)]
pub(crate) struct GlobArgs {
    /// The glob each path under `path` is matched against.
    pub(crate) pattern: String,
    /// The directory to look under, relative to the working directory. Left
    /// out means the working directory itself.
    #[serde(default)]
    pub(crate) path: Option<String>,
    /// Most paths returned.
    #[serde(default)]
    pub(crate) max_results: Option<i32>,
    /// Also return what `.gitignore`, `.ignore` or a leading dot hides.
    #[serde(default)]
    pub(crate) include_ignored: Option<bool>,
}
