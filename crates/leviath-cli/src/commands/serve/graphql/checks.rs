//! The four pure checks, and what they answer with.
//!
//! Each of these takes text and gives a verdict: nothing is written, nothing is
//! dialled, nothing is run. They are `Query` fields for that reason, so a
//! caching client, a `GET`-only proxy or a reader with no write access can use
//! them. The types live here rather than beside the writes they usually precede,
//! because a check is not half of a mutation.

use async_graphql::SimpleObject;

/// What a validation found.
///
/// `valid` is the verdict; the lists say why. A manifest can be valid and still
/// carry warnings, which is the common case for a blueprint that works but names
/// something the engine has retired.
#[derive(Debug, SimpleObject)]
pub(crate) struct ValidationReport {
    /// Whether the manifest would install and run.
    pub(crate) valid: bool,
    /// What makes it invalid. Empty when it is.
    pub(crate) errors: Vec<String>,
    /// What is worth knowing about it anyway.
    pub(crate) warnings: Vec<String>,
}

/// One call to decide about, as a yolo test sends it.
#[derive(Debug, async_graphql::InputObject)]
pub(crate) struct YoloTestInput {
    /// The profile to test.
    pub(crate) profile: String,
    /// The tool the model would call.
    pub(crate) tool: String,
    /// The command line, for the shell.
    pub(crate) command: Option<String>,
    /// The call's arguments, for a tool that is not the shell.
    pub(crate) arguments: Option<super::scalars::Json>,
    /// The working directory the run would have. The server's own, when left out.
    pub(crate) workdir: Option<String>,
    /// What the config resolves the tool to. Read from the config in force when
    /// left out.
    pub(crate) configured: Option<String>,
    /// The tool's kind: `builtin`, `subagent`, `script` or `mcp`. Guessed from the
    /// name when left out.
    pub(crate) kind: Option<String>,
    /// Decide as if the tool had been named in `--allow`.
    pub(crate) allowed: Option<bool>,
}

/// Whether a key looks like one of that provider's.
#[derive(Debug, async_graphql::SimpleObject)]
pub(crate) struct KeyVerdict {
    /// Whether the format is right. Not whether the key works: nothing was
    /// dialled.
    pub(crate) valid: bool,
    /// What is wrong with it, when something is.
    pub(crate) message: Option<String>,
}

/// Whether a script compiles.
#[derive(Debug, async_graphql::SimpleObject)]
pub(crate) struct ScriptVerdict {
    /// Whether the compiler for this kind accepts the text.
    pub(crate) valid: bool,
    /// The compiler's complaint, when it does not.
    pub(crate) error: Option<String>,
}

/// What a yolo profile would do with one call.
#[derive(Debug, async_graphql::SimpleObject)]
pub(crate) struct YoloDecision {
    /// The profile that decided.
    pub(crate) profile: String,
    /// The tool the call was for.
    pub(crate) tool: String,
    /// What the config layers resolve the tool to, before the profile.
    pub(crate) configured: String,
    /// What the profile makes of it: `allow`, `ask` or `deny`.
    pub(crate) policy: String,
    /// Why, in words, when the decision has a reason to give.
    pub(crate) reason: Option<String>,
}

/// One string out of the decision the command and the API share.
///
/// The shared decider answers in the shape `lev yolo test --json` prints, so this
/// reads that shape rather than a second one built for here.
pub(super) fn field_of(decided: &serde_json::Value, key: &str) -> String {
    decided
        .get(key)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}
