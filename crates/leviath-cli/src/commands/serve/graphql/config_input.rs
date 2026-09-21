//! What a config write takes.
//!
//! Three states per setting, which is what makes this its own module rather than
//! a bag of `Option`s: a field left out leaves the setting alone, `null` clears
//! it, and a value sets it. GraphQL spells that `MaybeUndefined`, so the
//! difference between "not sent" and "sent as null" survives the wire, which an
//! ordinary nullable field cannot carry.

use async_graphql::{InputObject, MaybeUndefined};

use super::super::config_types::{GatewayWrite, WriteConfigReq};

/// One name and value, for a gateway's headers.
#[derive(InputObject)]
pub(crate) struct EnvEntryInput {
    /// The header or variable name.
    pub(crate) name: String,
    /// Its value.
    pub(crate) value: String,
}

/// A custom model gateway to write into the config.
#[derive(InputObject)]
pub(crate) struct GatewayInput {
    /// The name a blueprint references, and the table key.
    pub(crate) name: String,
    /// What backs it: `script`, `openai-compatible` or `openai`.
    pub(crate) kind: Option<String>,
    /// Where the gateway lives, for an endpoint-backed one.
    pub(crate) base_url: Option<String>,
    /// The script that serves it, for a script-backed one.
    pub(crate) script: Option<String>,
    /// Its API key. Written, never read back.
    pub(crate) api_key: Option<String>,
    /// Extra headers every request carries.
    pub(crate) headers: Option<Vec<EnvEntryInput>>,
    /// The models it serves, when the gateway cannot be asked.
    pub(crate) models: Option<Vec<String>>,
}

/// A partial edit of the machine's config.
///
/// Every field is optional. A key with three states says so in its own
/// description: those are the ones where clearing the setting is a thing a
/// person does, and where `null` is how they say it.
#[derive(InputObject)]
pub(crate) struct ConfigInput {
    /// The provider a bare model name resolves on.
    pub(crate) default_provider: Option<String>,
    /// Providers allowed to serve a bare model name, best first. A list replaces
    /// the order whole, and an empty one clears it back to `defaultProvider`
    /// alone.
    pub(crate) provider_order: Option<Vec<String>>,
    /// The model every stage that permits it starts on, ahead of its own list.
    /// Three states: absent leaves the pin, `null` removes it so each blueprint
    /// picks its own model again, a value pins that one. An empty string is
    /// refused rather than read as a clear.
    pub(crate) override_model: MaybeUndefined<String>,
    /// The model tried after every model a stage names. Same three states.
    pub(crate) fallback_model: MaybeUndefined<String>,
    /// The Anthropic key. Three states: absent leaves it, `null` clears it and
    /// takes the provider out of this install, a value sets it. An empty string
    /// is refused rather than stored as a key that authenticates as nobody.
    pub(crate) anthropic_key: MaybeUndefined<String>,
    /// The OpenAI key, with the same three states.
    pub(crate) openai_key: MaybeUndefined<String>,
    /// The Google key, with the same three states.
    pub(crate) google_key: MaybeUndefined<String>,
    /// The OpenRouter key, with the same three states.
    pub(crate) openrouter_key: MaybeUndefined<String>,
    /// The Bedrock key, sent as a bearer token. Same three states.
    pub(crate) bedrock_key: MaybeUndefined<String>,
    /// The xAI key, which starts `xai-`. Same three states.
    pub(crate) xai_key: MaybeUndefined<String>,
    /// The Meta key, with the same three states.
    pub(crate) meta_key: MaybeUndefined<String>,
    /// The AWS region Bedrock is called in.
    pub(crate) bedrock_region: Option<String>,
    /// Where Ollama is.
    pub(crate) ollama_base_url: Option<String>,
    /// Whether the Ollama provider is on.
    pub(crate) ollama_enabled: Option<bool>,
    /// Whether the Codex transport is on.
    pub(crate) codex_enabled: Option<bool>,
    /// Whether the Grok transport is on.
    pub(crate) grok_enabled: Option<bool>,
    /// Whether providers may be handed files rather than text.
    pub(crate) file_uploads: Option<bool>,
    /// How hard Codex thinks: `none`, `minimal`, `low`, `medium`, `high` or
    /// `xhigh`. A word the provider does not know is refused rather than saved,
    /// because it would be saved and then ignored.
    pub(crate) codex_reasoning_effort: Option<String>,
    /// How much Codex writes: `low`, `medium` or `high`.
    pub(crate) codex_verbosity: Option<String>,
    /// Whether Codex is sent its own reasoning back.
    pub(crate) codex_replay_reasoning: Option<bool>,
    /// Gateways to add or change. Each is merged field by field, so a gateway can
    /// be edited without resending its key.
    pub(crate) gateways: Option<Vec<GatewayInput>>,
    /// Gateways to remove, by name. Removals run after the edits above, so one
    /// request that both edits and deletes does not depend on the order.
    pub(crate) remove_gateways: Option<Vec<String>>,
}

impl ConfigInput {
    /// The same edit as the REST route's own request, so one writer applies both.
    pub(crate) fn into_request(self) -> WriteConfigReq {
        WriteConfigReq {
            default_provider: self.default_provider,
            provider_order: self.provider_order,
            override_model: three_state(self.override_model),
            fallback_model: three_state(self.fallback_model),
            anthropic_key: three_state(self.anthropic_key),
            openai_key: three_state(self.openai_key),
            google_key: three_state(self.google_key),
            openrouter_key: three_state(self.openrouter_key),
            bedrock_key: three_state(self.bedrock_key),
            xai_key: three_state(self.xai_key),
            meta_key: three_state(self.meta_key),
            bedrock_region: self.bedrock_region,
            ollama_base_url: self.ollama_base_url,
            ollama_enabled: self.ollama_enabled,
            codex_enabled: self.codex_enabled,
            grok_enabled: self.grok_enabled,
            file_uploads: self.file_uploads,
            codex_reasoning_effort: self.codex_reasoning_effort,
            codex_verbosity: self.codex_verbosity,
            codex_replay_reasoning: self.codex_replay_reasoning,
            gateways: self.gateways.map(|gateways| {
                gateways
                    .into_iter()
                    .map(|gateway| GatewayWrite {
                        name: gateway.name,
                        kind: gateway.kind,
                        base_url: gateway.base_url,
                        script: gateway.script,
                        api_key: gateway.api_key,
                        headers: gateway.headers.map(|headers| {
                            headers
                                .into_iter()
                                .map(|entry| (entry.name, entry.value))
                                .collect()
                        }),
                        models: gateway.models,
                    })
                    .collect()
            }),
            remove_gateways: self.remove_gateways,
        }
    }
}

/// GraphQL's three states as the request's own.
///
/// `MaybeUndefined` and `Option<Option<T>>` are the same idea twice, and this is
/// the one place the two meet.
fn three_state(value: MaybeUndefined<String>) -> Option<Option<String>> {
    match value {
        MaybeUndefined::Undefined => None,
        MaybeUndefined::Null => Some(None),
        MaybeUndefined::Value(value) => Some(Some(value)),
    }
}
