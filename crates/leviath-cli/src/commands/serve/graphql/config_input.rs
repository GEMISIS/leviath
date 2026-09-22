//! What a config write takes.
//!
//! Three states per setting, and all three are visible in the schema rather
//! than hidden in a nullable field: what `set` names is set, what `clear` names
//! is taken back to nothing, and what neither mentions is left alone. A
//! provider's key has the same three states written as `key` and `clearKey`,
//! because "send null" is not something a schema can teach a client and a list
//! of clearable settings is.
//!
//! Every shape here mirrors the config a read answers with, one for one, so a
//! settings screen renders what it saves. A key is the exception and always
//! will be: it is written here and reads back as `hasKey`.

use async_graphql::{Enum, InputObject};

use super::super::config_types::{GatewayWrite as GatewayWriteReq, WriteConfigReq};
use super::super::core::error::ServeError;
use super::inputs::KeyValueWrite;
use super::types::machine::{CodexReasoningEffort, CodexVerbosity, GatewayKind};

/// A setting a request can take back to nothing.
///
/// Only the settings where having none is a state somebody chooses. A provider
/// key is cleared by `clearKey` on the provider instead, and a list is cleared
/// by sending an empty one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ConfigClearable {
    /// The model pinned ahead of every stage's own list. Cleared, each
    /// blueprint picks its own model again.
    OverrideModel,
    /// The model tried after every model a stage names.
    FallbackModel,
}

impl ConfigClearable {
    /// The name this setting travels under, for a refusal that names it.
    fn name(self) -> &'static str {
        match self {
            Self::OverrideModel => "OVERRIDE_MODEL",
            Self::FallbackModel => "FALLBACK_MODEL",
        }
    }
}

/// A custom model gateway to write into the config.
///
/// Merged field by field into whatever entry already has this name, so a
/// gateway's address can be edited without resending its key.
#[derive(Debug, InputObject)]
pub(crate) struct GatewayWrite {
    /// The name a blueprint references, and the table key. An entry is created
    /// when nothing has this name.
    pub(crate) name: String,
    /// What backs it. Left out, an entry that already exists keeps its kind and
    /// a new one is a script, as in the file.
    pub(crate) kind: Option<GatewayKind>,
    /// Where the gateway lives, for an endpoint-backed one.
    pub(crate) base_url: Option<String>,
    /// The script that serves it, for a script-backed one.
    pub(crate) script: Option<String>,
    /// Its API key. Written, and read back only as `hasApiKey`.
    pub(crate) api_key: Option<String>,
    /// Extra headers every request carries, replacing the set that is there.
    pub(crate) headers: Option<Vec<KeyValueWrite>>,
    /// The models it serves, when the gateway cannot be asked.
    pub(crate) models: Option<Vec<String>>,
}

impl GatewayWrite {
    /// The same gateway edit as the REST route's own request.
    fn into_request(self) -> GatewayWriteReq {
        GatewayWriteReq {
            name: self.name,
            kind: self.kind.map(|kind| kind.as_wire().to_string()),
            base_url: self.base_url,
            script: self.script,
            api_key: self.api_key,
            headers: self.headers.map(|headers| {
                headers
                    .into_iter()
                    .map(|entry| (entry.key, entry.value))
                    .collect()
            }),
            models: self.models,
        }
    }
}

/// The Codex transport's own settings.
#[derive(Debug, InputObject)]
pub(crate) struct CodexOptionsWrite {
    /// How hard it thinks.
    pub(crate) reasoning_effort: Option<CodexReasoningEffort>,
    /// How much it writes.
    pub(crate) verbosity: Option<CodexVerbosity>,
    /// Whether a turn's opaque reasoning token is sent back on the next one.
    pub(crate) replays_reasoning: Option<bool>,
}

/// One provider's settings, as a write sends them.
///
/// Which fields a provider accepts depends on the provider, and sending one it
/// has no use for is refused rather than dropped: a setting that saves and then
/// does nothing is the failure this shape exists to avoid.
#[derive(Debug, InputObject)]
pub(crate) struct ProviderConfigWrite {
    /// Which provider, by the id `ProviderConfigOutput.id` carries.
    pub(crate) provider: String,
    /// Its API key, for a provider that takes one. Never read back.
    pub(crate) key: Option<String>,
    /// Take the stored key away, which takes the provider out of this install.
    /// Refused together with `key`, because the two say opposite things.
    #[graphql(default = false)]
    pub(crate) clear_key: bool,
    /// Whether a run may route to it, for a provider with a switch of its own.
    pub(crate) is_enabled: Option<bool>,
    /// Where it is, for a provider that lives at an address this machine
    /// chooses.
    pub(crate) base_url: Option<String>,
    /// The region it is called in, for a provider that has regions.
    pub(crate) region: Option<String>,
    /// The Codex transport's own settings.
    pub(crate) codex: Option<CodexOptionsWrite>,
}

/// The settings that belong to no one provider.
#[derive(Debug, InputObject)]
pub(crate) struct ConfigWrite {
    /// The provider a bare model name resolves on.
    pub(crate) default_provider: Option<String>,
    /// Providers allowed to serve a bare model name, best first. A list
    /// replaces the order whole, and an empty one takes it back to
    /// `defaultProvider` alone.
    pub(crate) provider_order: Option<Vec<String>>,
    /// The model every stage that permits it starts on, ahead of its own list.
    /// Name it in `clear` to remove the pin; an empty string is refused rather
    /// than read as a clear.
    pub(crate) override_model: Option<String>,
    /// The model tried after every model a stage names, with the same clear.
    pub(crate) fallback_model: Option<String>,
    /// Whether providers may be handed files rather than text.
    pub(crate) allows_file_uploads: Option<bool>,
}

impl ConfigWrite {
    /// An edit that sets nothing, which is what a request that only clears or
    /// only touches providers amounts to.
    fn nothing() -> Self {
        Self {
            default_provider: None,
            provider_order: None,
            override_model: None,
            fallback_model: None,
            allows_file_uploads: None,
        }
    }
}

/// What `updateConfig` takes.
///
/// Four lists over one file, applied in one write: what to set, what to clear,
/// which providers to change, and which gateways to add or take away. Every
/// refusal happens before anything is written, so a request that is going to
/// fail leaves the file as it was.
#[derive(Debug, InputObject)]
pub(crate) struct UpdateConfigRequest {
    /// The settings that belong to no one provider.
    pub(crate) set: Option<ConfigWrite>,
    /// Settings to take back to nothing.
    pub(crate) clear: Option<Vec<ConfigClearable>>,
    /// The providers to change, each named by its id.
    pub(crate) providers: Option<Vec<ProviderConfigWrite>>,
    /// Gateways to add or change, each merged field by field.
    pub(crate) upsert_gateways: Option<Vec<GatewayWrite>>,
    /// Gateways to remove, by name. Removals run after the edits above, so one
    /// request that does both does not depend on the order.
    pub(crate) delete_gateways: Option<Vec<String>>,
}

/// A request that changes nothing, which every field then fills in.
fn nothing() -> WriteConfigReq {
    WriteConfigReq {
        default_provider: None,
        provider_order: None,
        override_model: None,
        fallback_model: None,
        anthropic_key: None,
        openai_key: None,
        google_key: None,
        openrouter_key: None,
        bedrock_key: None,
        xai_key: None,
        meta_key: None,
        bedrock_region: None,
        ollama_base_url: None,
        ollama_enabled: None,
        codex_enabled: None,
        grok_enabled: None,
        file_uploads: None,
        codex_reasoning_effort: None,
        codex_verbosity: None,
        codex_replay_reasoning: None,
        gateways: None,
        remove_gateways: None,
    }
}

/// Which provider a write names.
///
/// Read once, so every refusal below can say what this provider does and does
/// not have rather than repeating a string comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Named {
    /// Anthropic's API.
    Anthropic,
    /// OpenAI's API.
    Openai,
    /// Google's API.
    Google,
    /// OpenRouter.
    Openrouter,
    /// Amazon Bedrock, which is the one provider with a region.
    Bedrock,
    /// xAI's API.
    Xai,
    /// Meta's API.
    Meta,
    /// Ollama on this machine, which is the one provider with an address.
    Ollama,
    /// The Codex transport, which is the one provider with options.
    Codex,
    /// Grok billed to a subscription.
    Grok,
}

impl Named {
    /// The provider an id names, if this build has one.
    fn parse(id: &str) -> Option<Self> {
        match id {
            "anthropic" => Some(Self::Anthropic),
            "openai" => Some(Self::Openai),
            "google" => Some(Self::Google),
            "openrouter" => Some(Self::Openrouter),
            "bedrock" => Some(Self::Bedrock),
            "xai" => Some(Self::Xai),
            "meta" => Some(Self::Meta),
            "ollama" => Some(Self::Ollama),
            "codex" => Some(Self::Codex),
            "grok" => Some(Self::Grok),
            _ => None,
        }
    }

    /// Whether it takes an API key.
    fn takes_key(self) -> bool {
        !matches!(self, Self::Ollama | Self::Codex | Self::Grok)
    }

    /// Whether it has a switch of its own, rather than being on exactly when a
    /// key is stored.
    fn takes_enabled(self) -> bool {
        matches!(self, Self::Ollama | Self::Codex | Self::Grok)
    }
}

/// Refuse a field the named provider has no use for.
fn refuse(provider: &str, field: &str, why: &str) -> ServeError {
    ServeError::BadRequest(format!(
        "provider '{provider}': {field} is not a setting it has, because {why}"
    ))
}

impl ProviderConfigWrite {
    /// Fold one provider's settings into the edit, or say why they are not its
    /// settings.
    fn apply(self, req: &mut WriteConfigReq) -> Result<(), ServeError> {
        let id = self.provider.trim().to_ascii_lowercase();
        let Some(named) = Named::parse(&id) else {
            return Err(ServeError::BadRequest(format!(
                "no provider is called '{}'; read `config.providers` for the ids this build \
                 knows",
                self.provider
            )));
        };
        if self.key.is_some() && self.clear_key {
            return Err(ServeError::BadRequest(format!(
                "provider '{id}': key and clearKey say opposite things; send one of them"
            )));
        }
        if (self.key.is_some() || self.clear_key) && !named.takes_key() {
            return Err(refuse(&id, "key", "its credential is a browser sign-in"));
        }
        if self.is_enabled.is_some() && !named.takes_enabled() {
            return Err(refuse(
                &id,
                "isEnabled",
                "a stored key is what turns it on, so clearKey is how it is turned off",
            ));
        }
        if self.region.is_some() && named != Named::Bedrock {
            return Err(refuse(&id, "region", "it is not called in a region"));
        }
        if self.base_url.is_some() && named != Named::Ollama {
            return Err(refuse(
                &id,
                "baseUrl",
                "it lives at its own address; a server of your own is a gateway",
            ));
        }
        if self.codex.is_some() && named != Named::Codex {
            return Err(refuse(&id, "codex", "those are the Codex transport's own"));
        }

        let key = match (self.key, self.clear_key) {
            (Some(key), _) => Some(Some(key)),
            (None, true) => Some(None),
            (None, false) => None,
        };
        match named {
            Named::Anthropic => req.anthropic_key = key,
            Named::Openai => req.openai_key = key,
            Named::Google => req.google_key = key,
            Named::Openrouter => req.openrouter_key = key,
            Named::Bedrock => {
                req.bedrock_key = key;
                req.bedrock_region = self.region;
            }
            Named::Xai => req.xai_key = key,
            Named::Meta => req.meta_key = key,
            Named::Ollama => {
                req.ollama_enabled = self.is_enabled;
                req.ollama_base_url = self.base_url;
            }
            Named::Codex => {
                req.codex_enabled = self.is_enabled;
                if let Some(codex) = self.codex {
                    req.codex_reasoning_effort = codex
                        .reasoning_effort
                        .map(|effort| effort.as_wire().to_string());
                    req.codex_verbosity = codex
                        .verbosity
                        .map(|verbosity| verbosity.as_wire().to_string());
                    req.codex_replay_reasoning = codex.replays_reasoning;
                }
            }
            Named::Grok => req.grok_enabled = self.is_enabled,
        }
        Ok(())
    }
}

impl UpdateConfigRequest {
    /// The same edit as the REST route's own request, so one writer applies
    /// both surfaces.
    ///
    /// Every refusal is here rather than in the writer: the writer takes a
    /// request that already makes sense, and what makes sense is a property of
    /// this schema's shape.
    pub(crate) fn into_request(self) -> Result<WriteConfigReq, ServeError> {
        let mut req = nothing();
        let cleared = self.clear.unwrap_or_default();
        let set = self.set.unwrap_or_else(ConfigWrite::nothing);

        req.default_provider = set.default_provider;
        req.provider_order = set.provider_order;
        req.file_uploads = set.allows_file_uploads;
        for (what, sent, slot) in [
            (
                ConfigClearable::OverrideModel,
                set.override_model,
                &mut req.override_model,
            ),
            (
                ConfigClearable::FallbackModel,
                set.fallback_model,
                &mut req.fallback_model,
            ),
        ] {
            match (sent, cleared.contains(&what)) {
                (Some(_), true) => {
                    return Err(ServeError::BadRequest(format!(
                        "{} is in both `set` and `clear`; a setting is one or the other",
                        what.name()
                    )));
                }
                (Some(value), false) => *slot = Some(Some(value)),
                (None, true) => *slot = Some(None),
                (None, false) => {}
            }
        }

        for provider in self.providers.unwrap_or_default() {
            provider.apply(&mut req)?;
        }
        req.gateways = self.upsert_gateways.map(|gateways| {
            gateways
                .into_iter()
                .map(GatewayWrite::into_request)
                .collect()
        });
        req.remove_gateways = self.delete_gateways;
        Ok(req)
    }
}
