//! What this machine can route to: the models, the providers behind them, and
//! the tools a run may call.
//!
//! Each of the three is a root listing, and a root listing is a connection
//! whatever its size: a client that learns the shape once uses it everywhere,
//! and a catalogue that is small today is not small on every machine. The one
//! thing here that stays a bare list is `toolGroups`, which is a closed set
//! this build compiles in.

use async_graphql::{Enum, ID, SimpleObject};
use leviath_graphql_derive::mirror;

use super::super::connection::Paged;
use super::super::filter::scalars::StringFilter;
use super::super::filter::{
    Acc, BoxFuture, Confirm, CursorKey, Filterable, MatchCx, Mirror, Nullable, OrderDirection,
    OrderField, Orderable, Parts, Term, Tri, object_confirm, object_test, sort_key,
};
use super::super::scalars::{Decimal, Timestamp};

/// Where a model's token limits came from.
#[mirror]
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
#[mirror]
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

/// One provider's node id: `provider:<name>`.
///
/// One place mints it, so the id `providers` hands out, the id a model points
/// back with, and the id `node` looks up cannot drift apart.
pub(crate) fn provider_id(name: &str) -> ID {
    ID(format!("{PROVIDER_TAG}:{name}"))
}

/// One model's node id: `model:<provider>/<modelId>`.
///
/// The provider is part of the identity because the model id alone is not one:
/// `openai` and `codex` both answer to `gpt-5.5` and bill to different places.
pub(crate) fn model_id(provider: &str, model: &str) -> ID {
    ID(format!("{MODEL_TAG}:{provider}/{model}"))
}

/// The tag on a provider's id.
pub(crate) const PROVIDER_TAG: &str = "provider";

/// The tag on a model's id.
pub(crate) const MODEL_TAG: &str = "model";

/// One model this machine can route to.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct Model {
    /// `model:<provider>/<modelId>`.
    ///
    /// The provider is in it because the provider's own id is not unique here:
    /// `openai` and `codex` both answer to `gpt-5.5` and bill to different
    /// places, so the pair is the key and `modelId` is half of it.
    #[graphql(owned)]
    #[filter(orderable)]
    pub(crate) id: ID,
    /// The model id, as the provider names it. This is the half that goes into
    /// `provider/model` wherever one string has to name a model.
    #[filter(orderable)]
    pub(crate) model_id: String,
    /// The provider that serves it, as `provider:<name>`: read it back with
    /// `node`, or match it against `ProviderOutput.id`.
    #[graphql(owned)]
    #[filter(orderable)]
    pub(crate) provider_id: ID,
    /// The registry name of that provider, which is what a blueprint writes and
    /// what the left half of `provider/model` spells.
    #[filter(orderable)]
    pub(crate) provider_name: String,
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

impl Paged for Model {
    const NAME: &'static str = "Model";
}

impl From<&super::super::super::types::ModelEntry> for Model {
    fn from(entry: &super::super::super::types::ModelEntry) -> Self {
        Self {
            id: model_id(&entry.provider, &entry.id),
            model_id: entry.id.clone(),
            provider_id: provider_id(&entry.provider),
            provider_name: entry.provider.clone(),
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
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct Provider {
    /// `provider:<name>`. A machine holds one provider per registry name, so
    /// the name is the whole key.
    #[graphql(owned)]
    #[filter(orderable)]
    pub(crate) id: ID,
    /// The registry name a blueprint would use, and the left half of
    /// `provider/model`.
    #[filter(orderable)]
    pub(crate) name: String,
    /// The name to show.
    #[filter(orderable)]
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

impl Paged for Provider {
    const NAME: &'static str = "Provider";
}

impl From<&super::super::super::providers::ProviderInfo> for Provider {
    fn from(info: &super::super::super::providers::ProviderInfo) -> Self {
        Self {
            id: provider_id(&info.id),
            name: info.id.clone(),
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
    /// Compiled into this build, which covers the sub-agent tools as well:
    /// `origin` tells the two apart.
    Builtin(BuiltinTool),
    /// A `.rhai` script.
    Script(ScriptTool),
}

/// A tool compiled into this build of Leviath.
///
/// The sub-agent tools are here too. They carry exactly these four fields and
/// nothing else, so a second type for them said nothing a client could act on;
/// `origin` is `BUILTIN` or `SUBAGENT` and that is the whole difference.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct BuiltinTool {
    /// The name the model calls it by.
    pub(crate) name: String,
    /// What it does, in the words the model is given.
    pub(crate) description: String,
    /// The JSON Schema of its arguments.
    pub(crate) arguments: super::super::scalars::Json,
    /// `BUILTIN`, or `SUBAGENT` for a tool that spawns a child run.
    pub(crate) origin: ToolOrigin,
}

/// A tool backed by a `.rhai` script on this machine.
#[mirror]
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
            _ => Self::Builtin(BuiltinTool {
                name: entry.name,
                description: entry.description,
                arguments,
                origin,
            }),
        }
    }

    /// The name the model calls this tool by, whichever kind it is.
    pub(crate) fn tool_name(&self) -> &str {
        match self {
            Self::Builtin(tool) => &tool.name,
            Self::Script(tool) => &tool.name,
        }
    }

    /// What it does, in the words the model is given.
    pub(crate) fn tool_description(&self) -> &str {
        match self {
            Self::Builtin(tool) => &tool.description,
            Self::Script(tool) => &tool.description,
        }
    }

    /// What kind of thing offers it.
    pub(crate) fn tool_origin(&self) -> ToolOrigin {
        match self {
            Self::Builtin(tool) => tool.origin,
            Self::Script(tool) => tool.origin,
        }
    }
}

impl Paged for Tool {
    const NAME: &'static str = "Tool";
}

/// Which tools a listing wants, over the fields every tool has.
///
/// Hand-written because `Tool` is an interface, and what a client may ask about
/// is what the interface promises: the fields only a script tool has are read
/// through `... on ScriptToolOutput`, not filtered on. Everything it does is
/// the same delegation `#[mirror]` writes for an object.
#[derive(Debug, Default, async_graphql::InputObject)]
#[graphql(name = "ToolInput")]
pub(crate) struct ToolFilter {
    /// Filter on `Tool.name`. The name the model calls it by.
    pub(crate) name: Option<Box<StringFilter>>,
    /// Filter on `Tool.description`. What it does, in the words the model is
    /// given.
    pub(crate) description: Option<Box<StringFilter>>,
    /// Filter on `Tool.origin`. What kind of thing offers it.
    pub(crate) origin: Option<Box<ToolOriginFilter>>,
    /// Every filter in this list has to hold.
    pub(crate) and: Option<Vec<ToolFilter>>,
    /// At least one filter in this list has to hold.
    pub(crate) or: Option<Vec<ToolFilter>>,
    /// This filter must not hold.
    pub(crate) not: Option<Box<ToolFilter>>,
    /// Match where the value itself is absent.
    ///
    /// `true` matches only where there is no value, `false` only where there is
    /// one.
    pub(crate) is_null: Option<bool>,
}

impl Nullable for ToolFilter {
    fn is_null(&self) -> Option<bool> {
        self.is_null
    }
}

impl Mirror for ToolFilter {
    type Target = Tool;

    fn parts(&self) -> Parts<'_, Self> {
        Parts::new(
            self.and.as_deref(),
            self.or.as_deref(),
            self.not.as_deref(),
            self.is_null,
        )
    }

    fn cheap(&self, target: &Self::Target, cx: &MatchCx<'_>, acc: &mut Acc) {
        acc.field(self.name.as_deref(), &target.tool_name(), cx);
        acc.field(self.description.as_deref(), &target.tool_description(), cx);
        acc.field(self.origin.as_deref(), &target.tool_origin(), cx);
    }

    fn io<'mirror>(
        &'mirror self,
        target: &'mirror Self::Target,
        cx: &'mirror MatchCx<'mirror>,
    ) -> BoxFuture<'mirror, bool> {
        Box::pin(async move {
            let mut confirm = Confirm::new(cx);
            confirm
                .field(self.name.as_deref(), &target.tool_name())
                .await;
            confirm
                .field(self.description.as_deref(), &target.tool_description())
                .await;
            confirm
                .field(self.origin.as_deref(), &target.tool_origin())
                .await;
            confirm.finish()
        })
    }
}

impl Filterable for Tool {
    type Filter = ToolFilter;

    fn test(&self, filter: &Self::Filter, cx: &MatchCx<'_>) -> Tri {
        object_test(self, filter, cx)
    }

    fn confirm<'mirror>(
        &'mirror self,
        filter: &'mirror Self::Filter,
        cx: &'mirror MatchCx<'mirror>,
    ) -> BoxFuture<'mirror, bool> {
        object_confirm(self, filter, cx)
    }
}

/// The one thing a tool listing sorts by.
///
/// Its name: every tool has one, it is unique within an inventory, and it is
/// the order an editor offering a tool list wants to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
#[graphql(name = "ToolOrderField")]
pub(crate) enum ToolOrderField {
    /// Sort by `ToolOutput.name`.
    Name,
}

impl ToolOrderField {
    /// Every sort key this type offers, in declaration order.
    ///
    /// What the mirror writes for a generated order enum, written by hand for
    /// this one, so the tool listing's sort keys are measured the same way
    /// every other listing's are.
    #[cfg(test)]
    pub(crate) const ALL: &'static [Self] = &[Self::Name];
}

impl OrderField for ToolOrderField {
    fn wire(self) -> &'static str {
        match self {
            Self::Name => "name",
        }
    }
}

impl<'mirror> Orderable<MatchCx<'mirror>> for Tool {
    type Field = ToolOrderField;

    fn key(&self, field: Self::Field, _cx: &MatchCx<'mirror>) -> CursorKey {
        match field {
            ToolOrderField::Name => sort_key(self.tool_name()),
        }
    }
}

/// One sort key for a tool listing, and the direction it runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::InputObject)]
#[graphql(name = "ToolOrder")]
pub(crate) struct ToolOrder {
    /// Which field to order by.
    pub(crate) field: ToolOrderField,
    /// Which way it runs. Newest or largest first unless you say otherwise.
    #[graphql(default_with = "OrderDirection::Desc")]
    pub(crate) direction: OrderDirection,
}

impl ToolOrder {
    /// This term, as the walk compares on it.
    pub(crate) fn term(self) -> Term<ToolOrderField> {
        Term {
            field: self.field,
            direction: self.direction,
        }
    }
}

/// What kind of thing offers a tool.
///
/// A closed set, and the whole of it: this inventory is what a run on this
/// machine can be given, and an MCP server's tools are not in it. Read
/// `mcpServers` for those.
#[mirror]
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
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolGroup {
    /// The token itself, such as `@builtin`.
    pub(crate) name: String,
    /// What the token stands for.
    pub(crate) description: String,
}

/// A `.rhai` file that was found and could not be offered as a tool.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct SkippedTool {
    /// The file that was skipped.
    pub(crate) path: String,
    /// Why it could not be offered.
    pub(crate) reason: String,
}

/// What a tool listing says beyond the tools themselves.
///
/// Flattened into the connection, so `skipped` sits beside `results` rather
/// than inside a wrapper a client has to unpack.
#[derive(Debug, SimpleObject)]
pub(crate) struct ToolSkips {
    /// Scripts that were found and could not be offered, with the reason.
    ///
    /// Reported rather than dropped: a tool an author believes exists and that
    /// silently is not there is the failure this prevents. Not paged with the
    /// tools, because it is not a page of them: it is what the same walk of the
    /// same directories could not use.
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
