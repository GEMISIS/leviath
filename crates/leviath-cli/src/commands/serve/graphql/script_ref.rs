//! Naming one registered script, and the two closed vocabularies that say
//! which one.
//!
//! A script is keyed by three things at once: the registry it plugs into, the
//! name it is filed under there, and the blueprint whose directory it came
//! from. None of the three is enough on its own, so the reference carries all
//! three and every field that reads a script takes it.
//!
//! The vocabularies live here rather than beside the output type because both
//! sides of the schema use them: `ScriptOutput.kind` reads one, `ScriptRef` and
//! `validateScript` write one.

use async_graphql::{Enum, InputObject};
use leviath_graphql_derive::mirror;

/// Which registry a script plugs into, and so which compiler decides whether
/// it is valid.
///
/// The words are the `{kind}` path segments the REST routes take, so a client
/// that knows one surface knows the other.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ScriptKind {
    /// A tool the model may call, discovered from a `tools/` directory.
    Tool,
    /// A custom region's `render`, `on_write` or `on_overflow`.
    RegionHook,
    /// A stage lifecycle hook.
    StageHook,
    /// A validator that decides whether an agent's output may be handed back.
    OutputValidator,
    /// A check on the bytes behind a mime type, named by a registry row.
    MimeCheck,
    /// A drop-in model provider, global to the machine.
    Provider,
    /// A `.rhai` file beside a blueprint that nothing has claimed yet. It is
    /// what a listing reports for a file it found and no registry names, and no
    /// compiler accepts it: pick the kind it is meant to be first.
    Candidate,
}

impl ScriptKind {
    /// The wire spelling, which is also the `{kind}` path segment.
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::RegionHook => "region_hook",
            Self::StageHook => "stage_hook",
            Self::OutputValidator => "output_validator",
            Self::MimeCheck => "mime_check",
            Self::Provider => "provider",
            Self::Candidate => "unknown",
        }
    }

    /// Read the word a script listing carries.
    ///
    /// Anything the registries do not claim is a candidate, which is what the
    /// listing's own `unknown` means: a file that is there and belongs to no
    /// registry yet.
    pub(crate) fn from_wire(word: &str) -> Self {
        match word {
            "tool" => Self::Tool,
            "region_hook" => Self::RegionHook,
            "stage_hook" => Self::StageHook,
            "output_validator" => Self::OutputValidator,
            "mime_check" => Self::MimeCheck,
            "provider" => Self::Provider,
            _ => Self::Candidate,
        }
    }
}

/// Whose directory a script came from.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ScriptScope {
    /// The machine-wide directory, so every run here gets it.
    Global,
    /// One blueprint's own directory, so it travels with that blueprint and no
    /// other. `blueprintName` says which.
    Blueprint,
}

impl ScriptScope {
    /// Read the word a script listing carries.
    ///
    /// The listing says `agent` for a blueprint's own and `global` for the
    /// machine's, and those are the only two directories it walks.
    pub(crate) fn from_wire(word: &str) -> Self {
        match word {
            "agent" => Self::Blueprint,
            _ => Self::Global,
        }
    }
}

/// Which registered script an operation is about.
///
/// All three parts, because none of them is a key on its own: one machine can
/// hold a global `tool` called `summarise` and a blueprint's own `tool` of that
/// name, and they are two scripts.
#[derive(Debug, InputObject)]
pub(crate) struct ScriptRef {
    /// Which registry it belongs to.
    pub(crate) kind: ScriptKind,
    /// Its name, unique within that kind and the directory it came from.
    /// `/`-separated for a script in a subdirectory.
    pub(crate) name: String,
    /// The blueprint whose directory it came from. Left out for a script every
    /// run on this machine gets.
    pub(crate) blueprint_name: Option<String>,
}

#[cfg(test)]
#[path = "script_ref_tests.rs"]
mod tests;
