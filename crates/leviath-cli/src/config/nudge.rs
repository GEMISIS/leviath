//! `[nudge]`: the operator's defaults for the empty-response nudge.

use leviath_runtime::spec::graph::NudgeDef;
use serde::{Deserialize, Serialize};

/// Settings for the empty-response nudge: the `[System]` message injected when
/// a stage's model replies with text before making any tool call.
///
/// Every field is optional. A field left unset cascades stage → graph → this
/// table and finally to the built-in default, so a stage's `nudge` only has to
/// name what it wants to change. An empty table is inert.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NudgeSettings {
    /// Whether the nudge fires at all. When unset at every level, the default
    /// is on - except for a stage with interaction points, whose text response
    /// is its work product and which is left alone. Setting this explicitly at
    /// any level overrides that implicit rule in both directions.
    #[serde(default)]
    pub enabled: Option<bool>,

    /// How many text-only responses to nudge before accepting the text as
    /// final. Defaults to [`DEFAULT_MAX_NUDGES`](leviath_runtime::spec::graph::DEFAULT_MAX_NUDGES).
    #[serde(default)]
    pub max: Option<usize>,

    /// The nudge text. Defaults to [`DEFAULT_NUDGE_TEXT`](leviath_runtime::spec::graph::DEFAULT_NUDGE_TEXT).
    /// Supports `{stage}` (the stage's name) and `{regions}` (comma-separated
    /// names of the stage's required context regions) placeholders.
    #[serde(default)]
    pub text: Option<String>,
}

impl NudgeSettings {
    /// These settings as the graph's nudge, the level below a graph's own.
    pub fn def(&self) -> NudgeDef {
        NudgeDef {
            enabled: self.enabled,
            max: self.max.map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
            text: self.text.clone(),
        }
    }
}
