//! What this machine can route to: the models, the providers behind them, and
//! the tools a run may call.
//!
//! These are catalogues, sized by what the operator configured rather than by
//! what has accumulated, so they are plain lists. The house rule is the REST
//! surface's: collections that grow without bound are paged, and bounded ones
//! stay lists.

use async_graphql::{Enum, SimpleObject};

use super::super::scalars::{Decimal, Timestamp};

/// Where a model's token limits came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ModelLimitsSource {
    /// What the provider's own API reports.
    Api,
    /// This build's compiled table, matched on the model name.
    Builtin,
    /// A config override.
    Override,
    /// The daemon reported something this build does not know. The limits are
    /// still whatever it sent; only their provenance is unrecognised.
    Unknown,
}

impl From<&str> for ModelLimitsSource {
    fn from(label: &str) -> Self {
        match label {
            "api" => Self::Api,
            "builtin" => Self::Builtin,
            "override" => Self::Override,
            _ => Self::Unknown,
        }
    }
}

/// What one model costs per million tokens.
///
/// Absent where the price file has no entry for the model, which is why a run
/// can report an unpriced call rather than a cost of zero.
#[derive(Debug, SimpleObject)]
pub(crate) struct ModelPricing {
    /// Input tokens, per million.
    pub(crate) input_per_mtok: Decimal,
    /// Input tokens served from the provider's cache, per million.
    pub(crate) cached_input_per_mtok: Decimal,
    /// Tokens written to that cache, per million.
    pub(crate) cache_write_per_mtok: Decimal,
    /// Output tokens, per million.
    pub(crate) output_per_mtok: Decimal,
}

/// One model this machine can route to.
#[derive(Debug, SimpleObject)]
pub(crate) struct Model {
    /// The model id, as the provider names it.
    ///
    /// Not unique on this machine and so not a `Node` id: `openai` and `codex`
    /// both answer to `gpt-5.5` and bill to different places. Key on `provider`
    /// and this together, which is also how `provider/model` is spelled
    /// wherever one string has to name a model.
    pub(crate) id: String,
    /// The provider that serves it. Two providers can serve the same id and
    /// bill to different places, which is why this is part of the answer.
    pub(crate) provider: String,
    /// The name to show, when the provider gives one.
    pub(crate) display_name: Option<String>,
    /// Context window in tokens.
    pub(crate) max_context_tokens: i32,
    /// The most one reply may be, in tokens.
    pub(crate) max_output_tokens: i32,
    /// Where those two limits came from.
    pub(crate) limits_source: ModelLimitsSource,
    /// Whether the model takes tool definitions.
    pub(crate) supports_tools: bool,
    /// Whether it takes a sampling temperature.
    pub(crate) supports_temperature: bool,
    /// Whether the limits were learned from a live call rather than declared.
    pub(crate) learned: bool,
    /// When the model was released, when the provider says.
    pub(crate) released: Option<Timestamp>,
    /// When it retires, as the provider writes it.
    pub(crate) retires: Option<String>,
    /// What it costs, when the price file knows.
    pub(crate) pricing: Option<ModelPricing>,
    /// Mime type patterns it takes.
    pub(crate) input_types: Vec<String>,
    /// Mime type patterns it produces.
    pub(crate) output_types: Vec<String>,
}

impl From<&super::super::super::types::ModelEntry> for Model {
    fn from(entry: &super::super::super::types::ModelEntry) -> Self {
        Self {
            id: entry.id.clone(),
            provider: entry.provider.clone(),
            display_name: entry.display_name.clone(),
            max_context_tokens: tokens(entry.max_context_tokens),
            max_output_tokens: tokens(entry.max_output_tokens),
            limits_source: entry.limits_source.as_str().into(),
            supports_tools: entry.supports_tools,
            supports_temperature: entry.supports_temperature,
            learned: entry.learned,
            released: entry.released.map(Timestamp),
            retires: entry.retires.clone(),
            pricing: entry.pricing.as_ref().map(|p| ModelPricing {
                input_per_mtok: Decimal(p.input_per_mtok),
                cached_input_per_mtok: Decimal(p.cached_input_per_mtok),
                cache_write_per_mtok: Decimal(p.cache_write_per_mtok),
                output_per_mtok: Decimal(p.output_per_mtok),
            }),
            input_types: entry.input_types.clone(),
            output_types: entry.output_types.clone(),
        }
    }
}

/// One provider this machine can reach.
#[derive(Debug, SimpleObject)]
pub(crate) struct Provider {
    /// The registry name a blueprint would use.
    pub(crate) id: String,
    /// The name to show.
    pub(crate) display: String,
    /// Whether the config has it turned on.
    ///
    /// Separate from `signedIn`: the two are set by different routes, and
    /// either can be true on its own.
    pub(crate) enabled: bool,
    /// Whether a credential is stored for it.
    pub(crate) signed_in: bool,
    /// The account, when the credential names one.
    pub(crate) account: Option<String>,
    /// The subscription tier, when the credential names one.
    pub(crate) plan: Option<String>,
    /// When the access token lapses.
    ///
    /// Not a deadline for anybody: it is refreshed well before this. It is
    /// here so a console can show the session is live rather than implying
    /// somebody must act.
    pub(crate) expires_at: Option<Timestamp>,
}

impl From<&super::super::super::providers::ProviderInfo> for Provider {
    fn from(info: &super::super::super::providers::ProviderInfo) -> Self {
        Self {
            id: info.id.clone(),
            display: info.display.clone(),
            enabled: info.enabled,
            signed_in: info.signed_in,
            account: info.account.clone(),
            plan: info.plan.clone(),
            expires_at: info
                .expires_at
                .map(|at| Timestamp(i64::try_from(at).unwrap_or(i64::MAX))),
        }
    }
}

/// One tool a run on this machine can be given.
///
/// An interface rather than one type with nullable extras: a script tool always
/// has a file and a built-in never does, and a schema that says so lets a client
/// read the file without checking whether it is there.
#[derive(Debug, async_graphql::Interface)]
#[graphql(field(
    name = "name",
    ty = "&String",
    desc = "The name the model calls it by."
))]
// Spelled the long way because clippy reads two `ty = "&String"` in one
// `#[graphql]` as a duplicated attribute, and this repo allows no `#[allow]`.
#[graphql(field(
    name = "description",
    ty = "&std::string::String",
    desc = "What it does, in the words the model is given."
))]
#[graphql(field(
    name = "arguments",
    ty = "&super::super::scalars::Json",
    desc = "The JSON Schema of its arguments, as the model is given it."
))]
#[graphql(field(
    name = "origin",
    ty = "&ToolOrigin",
    desc = "What kind of thing offers it, for a client that would rather branch \
            on a value than on a type."
))]
pub(crate) enum Tool {
    /// Compiled into this build.
    Builtin(BuiltinTool),
    /// A sub-agent tool.
    Subagent(SubagentTool),
    /// A `.rhai` script.
    Script(ScriptTool),
}

/// A tool compiled into this build of Leviath.
#[derive(Debug, SimpleObject)]
pub(crate) struct BuiltinTool {
    /// The name the model calls it by.
    pub(crate) name: String,
    /// What it does, in the words the model is given.
    pub(crate) description: String,
    /// The JSON Schema of its arguments.
    pub(crate) arguments: super::super::scalars::Json,
    /// Always `BUILTIN`.
    pub(crate) origin: ToolOrigin,
}

/// A sub-agent tool, offered to an agent that may spawn children.
#[derive(Debug, SimpleObject)]
pub(crate) struct SubagentTool {
    /// The name the model calls it by.
    pub(crate) name: String,
    /// What it does, in the words the model is given.
    pub(crate) description: String,
    /// The JSON Schema of its arguments.
    pub(crate) arguments: super::super::scalars::Json,
    /// Always `SUBAGENT`.
    pub(crate) origin: ToolOrigin,
}

/// A tool backed by a `.rhai` script on this machine.
#[derive(Debug, SimpleObject)]
pub(crate) struct ScriptTool {
    /// The name the model calls it by.
    pub(crate) name: String,
    /// What it does, taken from the script's `@description`.
    pub(crate) description: String,
    /// The JSON Schema of its arguments, built from its `@param` lines.
    pub(crate) arguments: super::super::scalars::Json,
    /// `BLUEPRINT_SCRIPT` or `GLOBAL_SCRIPT`, which is the difference between a
    /// tool one blueprint carries and one every run here has.
    pub(crate) origin: ToolOrigin,
    /// The file behind it.
    pub(crate) path: String,
    /// The blueprint whose directory it came from, for a blueprint-scoped
    /// script.
    pub(crate) blueprint: Option<String>,
    /// Platform capabilities it declares with `@requires`. A tool the platform
    /// cannot satisfy is not offered at all, so an entry here is one this
    /// machine meets.
    pub(crate) requires: Vec<String>,
}

impl Tool {
    /// Read one inventory entry as the kind of tool it is.
    ///
    /// A script entry without a path cannot happen - discovery only makes one
    /// from a file it read - and is carried as a built-in rather than dropped,
    /// because a tool missing from the listing is worse than one in the wrong
    /// arm of it.
    pub(crate) fn of(entry: crate::tool_inventory::ToolEntry) -> Self {
        use crate::tool_inventory::ToolSource;
        let origin = ToolOrigin::from(entry.source);
        let arguments = super::super::scalars::Json(entry.arguments);
        match (entry.source, entry.path) {
            (ToolSource::Agent | ToolSource::Global, Some(path)) => Self::Script(ScriptTool {
                name: entry.name,
                description: entry.description,
                arguments,
                origin,
                path: path.display().to_string(),
                blueprint: entry.agent,
                requires: entry.requires,
            }),
            (ToolSource::Subagent, _) => Self::Subagent(SubagentTool {
                name: entry.name,
                description: entry.description,
                arguments,
                origin,
            }),
            _ => Self::Builtin(BuiltinTool {
                name: entry.name,
                description: entry.description,
                arguments,
                origin,
            }),
        }
    }
}

/// What kind of thing offers a tool.
///
/// A closed set, and the whole of it: this inventory is what a run on this
/// machine can be given, and an MCP server's tools are not in it. Read
/// `mcpServers` for those.
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum ToolOrigin {
    /// Compiled into this build. Every run has it.
    Builtin,
    /// A sub-agent tool, offered to an agent that may spawn children.
    Subagent,
    /// A `.rhai` script in one blueprint's own `tools/`, so it travels with that
    /// blueprint and no other.
    BlueprintScript,
    /// A `.rhai` script in the machine-wide tools directory, so every run
    /// here gets it.
    GlobalScript,
}

impl From<crate::tool_inventory::ToolSource> for ToolOrigin {
    fn from(source: crate::tool_inventory::ToolSource) -> Self {
        use crate::tool_inventory::ToolSource;
        match source {
            ToolSource::Builtin => Self::Builtin,
            ToolSource::Subagent => Self::Subagent,
            ToolSource::Agent => Self::BlueprintScript,
            ToolSource::Global => Self::GlobalScript,
        }
    }
}

/// One group token an `available_tools` list may name in place of tool names.
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolGroup {
    /// The token itself, such as `@builtin`.
    pub(crate) name: String,
    /// What the token stands for.
    pub(crate) description: String,
}

/// A `.rhai` file that was found and could not be offered as a tool.
#[derive(Debug, SimpleObject)]
pub(crate) struct SkippedTool {
    /// The file that was skipped.
    pub(crate) path: String,
    /// Why it could not be offered.
    pub(crate) reason: String,
}

/// The tool inventory: what a run on this machine can call.
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolInventory {
    /// The tools themselves.
    pub(crate) tools: Vec<Tool>,
    /// The group tokens a blueprint may name instead of individual tools.
    pub(crate) groups: Vec<ToolGroup>,
    /// Scripts that were found and could not be offered, with the reason.
    ///
    /// Reported rather than dropped: a tool an author believes exists and that
    /// silently is not there is the failure this prevents.
    pub(crate) skipped: Vec<SkippedTool>,
}

/// Narrow a token limit to the 32 bits GraphQL's `Int` carries.
///
/// Context windows are in the millions at most, so this is a formality; it
/// saturates rather than wraps so an implausible number reads as implausible.
fn tokens(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
