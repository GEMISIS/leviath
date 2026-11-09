//! `[model_providers.<name>]`: a Rhai script provider's overrides, or an
//! OpenAI-shaped endpoint reached natively.
//!
//! `ModelProviderConfig` hand-writes its `Debug` for the reason
//! [`ProviderConfig`](super::ProviderConfig) does: a key, a header value or a forwarded extra must
//! not reach a log through a derived one.

use super::provider_config::redacted;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// What backs a `[model_providers.<name>]` entry.
///
/// Absent from the file means [`Self::Script`], which is what every entry
/// written before the field existed is. Spelled out here rather than inferred
/// from which fields are set, so a config says what it means and a typo in
/// the value is a load error instead of a silently different provider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelProviderKind {
    /// A Rhai provider script in `~/.leviath/providers/`.
    #[default]
    Script,
    /// A server speaking OpenAI's chat API, reached natively with no script:
    /// llama.cpp, vLLM, LM Studio, or a gateway.
    OpenaiCompatible,
    /// OpenAI's own API (the Responses route) at another host, under a name
    /// of its own: an Azure resource or an API gateway in front of OpenAI.
    /// Several can sit side by side, each with its own key.
    Openai,
}

impl ModelProviderKind {
    /// The spelling the config file uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::OpenaiCompatible => "openai-compatible",
            Self::Openai => "openai",
        }
    }

    /// The kind a config file spelling names, if it is one.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "script" => Some(Self::Script),
            "openai-compatible" => Some(Self::OpenaiCompatible),
            "openai" => Some(Self::Openai),
            _ => None,
        }
    }
}

/// A `[model_providers.<name>]` entry: a Rhai script provider's overrides, or
/// an OpenAI-compatible endpoint.
///
/// Every field is optional. For a script, keys not recognized below flow into
/// [`Self::extra`] and are forwarded to the script's `initialize(config)`
/// alongside `base_url` and `api_key`. For an endpoint, `base_url` is required
/// and `headers` and `models` are read; a key that would land in `extra` is
/// refused at load, since nothing would read it.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct ModelProviderConfig {
    /// What backs the entry. Absent means a script, so every existing config
    /// reads as it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ModelProviderKind>,

    /// Script filename stem or path. Defaults to `<name>.rhai` in the providers
    /// directory (`~/.leviath/providers/`).
    #[serde(default)]
    pub script: Option<String>,

    /// API key forwarded to the script as `config.api_key` (a script may instead
    /// read its own environment variable).
    #[serde(default)]
    pub api_key: Option<String>,

    /// Base URL forwarded to the script as `config.base_url`.
    #[serde(default)]
    pub base_url: Option<String>,

    /// Rate limit enforced by the Rust wrapper (requests/tokens per minute).
    #[serde(default)]
    pub rate_limit: Option<leviath_providers::RateLimitConfig>,

    /// Model ids this provider serves, so a blueprint entry naming one of them
    /// with no provider can resolve here.
    ///
    /// Only needed by a script with no `list_models`: one that has it is asked
    /// directly, and its answer is preferred over this list. Without either,
    /// the provider claims no models and can only be reached by a blueprint
    /// that pins it, which leaves a local model unreachable however the machine
    /// sets `default_provider`.
    /// `None` when the file does not mention it, `Some` when it does - including
    /// `Some(vec![])` for an explicit `serves = []`.
    ///
    /// The two are different states, and collapsing them into one empty `Vec`
    /// makes a stale `serves = []` unremovable: a save-back writes whatever the
    /// field holds, and an empty `Vec` writes as `serves = []` however it got
    /// there. `None` writes nothing, which is what lets the
    /// `stale-empty-serves` migration take the line out.
    ///
    /// Skipping `None` does not break the invariant `Config::unknown_config_keys`
    /// rests on - "a field they set is a field that serializes". A field they set
    /// is `Some`, and `Some` always serializes, `serves = []` included. `None` is
    /// the field they did *not* set, which was never in the file to be reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serves: Option<Vec<String>>,

    /// Extra headers on every request to an OpenAI-compatible endpoint, as
    /// `Name = "value"`. A gateway that wants an organisation or routing header
    /// is the usual reason. Not read for a script.
    ///
    /// A `BTreeMap` so the file, the debug line and the API all list them in
    /// one order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, String>>,

    /// The model ids an OpenAI-compatible endpoint serves, for a server that
    /// does not answer `GET /models`. Read only when detection fails: a server
    /// that lists its models is believed over this. Not read for a script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,

    /// Any additional keys, forwarded verbatim into the script's `initialize`.
    #[serde(flatten)]
    pub extra: HashMap<String, toml::Value>,
}

/// The keys an OpenAI-compatible endpoint entry reads, for the refusal above
/// to list. `script` is included because the struct accepts it on any entry,
/// though an endpoint never runs one.
const ENDPOINT_KEYS: &[&str] = &[
    "kind",
    "script",
    "api_key",
    "base_url",
    "rate_limit",
    "serves",
    "headers",
    "models",
    "retention",
    "zero_retention_request",
    "auth_header",
];

/// The keys an endpoint entry reads off `extra`, where a `flatten` puts them:
/// what the host keeps of its requests, which built-in provider's
/// zero-retention request field it takes, and the header its key goes in.
const ENDPOINT_EXTRA_KEYS: &[&str] = &["retention", "zero_retention_request", "auth_header"];

impl ModelProviderConfig {
    /// The kind this entry is, with absent read as a script.
    pub fn kind(&self) -> ModelProviderKind {
        self.kind.unwrap_or_default()
    }

    /// Whether this entry is a host Leviath reaches natively (either
    /// OpenAI-shaped kind) rather than a script.
    pub fn is_endpoint(&self) -> bool {
        self.kind() != ModelProviderKind::Script
    }

    /// Whether this entry is OpenAI's own API at a host of its own.
    pub fn is_openai(&self) -> bool {
        self.kind() == ModelProviderKind::Openai
    }

    /// The header the key goes in instead of `Authorization: Bearer`, when
    /// the entry names one (`api-key` for an Azure key, or
    /// `Ocp-Apim-Subscription-Key` for an API Management gateway).
    pub fn auth_header(&self) -> Option<String> {
        self.extra
            .get("auth_header")
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    }

    /// What is wrong with this entry, if anything, named against `name`.
    ///
    /// Checked at config load rather than at the first inference: an endpoint
    /// with nowhere to send a request is a config that cannot work, and the
    /// message should name the table to fix while the file is still in front
    /// of the person who wrote it.
    pub fn validate(&self, name: &str) -> Result<(), String> {
        let kind = self.kind().as_str();
        if self.is_openai()
            && self
                .base_url
                .as_deref()
                .is_none_or(|url| url.trim().is_empty())
        {
            return Err(format!(
                "[model_providers.{name}] has kind = \"openai\" but no base_url; set \
                 base_url to the host's API root, such as \
                 \"https://<resource>.openai.azure.com/openai/v1\""
            ));
        }
        if self.is_openai()
            && self
                .api_key
                .as_deref()
                .is_none_or(|key| key.trim().is_empty())
        {
            return Err(format!(
                "[model_providers.{name}] has kind = \"openai\" but no api_key; set the \
                 key the host issued (add auth_header = \"api-key\" when the host wants \
                 it in that header rather than as a bearer token)"
            ));
        }
        if self.is_endpoint()
            && self
                .base_url
                .as_deref()
                .is_none_or(|url| url.trim().is_empty())
        {
            return Err(format!(
                "[model_providers.{name}] has kind = \"openai-compatible\" but no \
                 base_url; set base_url to where the server listens, such as \
                 \"http://localhost:8080/v1\""
            ));
        }
        if self.is_endpoint()
            && self.extra.contains_key("auth_header")
            && self.auth_header().is_none()
        {
            return Err(format!(
                "[model_providers.{name}] auth_header must name a header, such as \"api-key\""
            ));
        }
        // `extra` exists to reach a script's `initialize`. An endpoint has no
        // script, so a key landing there is one the endpoint will never read:
        // `modles` leaves it with no catalogue and `heaeders` sends nothing,
        // and both would otherwise load clean. `Config::unknown_config_keys`
        // cannot catch them either, because `flatten` writes them straight
        // back.
        let mut keys: Vec<&str> = self
            .extra
            .keys()
            .map(String::as_str)
            .filter(|key| !ENDPOINT_EXTRA_KEYS.contains(key))
            .collect();
        if self.is_endpoint() && !keys.is_empty() {
            keys.sort_unstable();
            return Err(format!(
                "[model_providers.{name}] has kind = \"{kind}\" and \
                 unknown key(s) {}; an endpoint reads only {}",
                keys.join(", "),
                ENDPOINT_KEYS.join(", ")
            ));
        }
        Ok(())
    }

    /// The headers as an ordered list, for a provider constructor.
    pub fn header_pairs(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// Hand-written for the same reason [`ProviderConfig`](super::ProviderConfig)'s
/// is, and with one extra
/// hazard: `extra` is forwarded verbatim into the script's `initialize`, which
/// is exactly where a second credential goes when a gateway wants one under its
/// own name, and an endpoint's `headers` carry the same thing. A derived `Debug`
/// would print `api_key` and every value in both, so this reports whether the
/// key is set and the *names* in `extra` and `headers` and nothing else - the
/// same shape `GatewayInfo` puts on the wire.
impl std::fmt::Debug for ModelProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Sorted: a `HashMap` iterates differently between two calls, and a
        // debug line that reorders itself is one nobody can diff.
        let mut extra_keys: Vec<&str> = self.extra.keys().map(String::as_str).collect();
        extra_keys.sort_unstable();
        // Header values are the same hazard as `extra`: an endpoint's second
        // credential is a header. Names only, for the same reason.
        let header_names: Vec<&str> = self
            .headers
            .iter()
            .flatten()
            .map(|(name, _)| name.as_str())
            .collect();
        f.debug_struct("ModelProviderConfig")
            .field("kind", &self.kind())
            .field("script", &self.script)
            .field("api_key", &redacted(&self.api_key))
            .field("base_url", &self.base_url)
            .field("rate_limit", &self.rate_limit)
            .field("header_names", &header_names)
            .field("models", &self.models)
            .field("extra_keys", &extra_keys)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gateway's key and its `extra` table are both credential-carrying:
    /// `extra` is forwarded verbatim into a provider script's `initialize`,
    /// which is where a second credential goes when the gateway wants one under
    /// its own name. Names, never values.
    #[test]
    fn debug_never_prints_the_keys() {
        let gateway = ModelProviderConfig {
            script: Some("groq.rhai".to_string()),
            api_key: Some("gsk-SECRET-VALUE".to_string()),
            base_url: Some("https://api.groq.example".to_string()),
            extra: HashMap::from([
                (
                    "org_token".to_string(),
                    toml::Value::String("org-SECRET-VALUE".to_string()),
                ),
                ("region".to_string(), toml::Value::String("eu".to_string())),
            ]),
            ..Default::default()
        };
        let rendered = format!("{gateway:?}");
        assert!(!rendered.contains("SECRET-VALUE"), "key leaked: {rendered}");
        assert!(rendered.contains("api_key: \"<set>\""), "{rendered}");
        // Sorted, so two runs of the same daemon print the same line.
        assert!(
            rendered.contains("extra_keys: [\"org_token\", \"region\"]"),
            "{rendered}"
        );
        // What is not a secret is still worth reading.
        assert!(rendered.contains("api.groq.example"), "{rendered}");

        let bare = format!("{:?}", ModelProviderConfig::default());
        assert!(bare.contains("api_key: \"<unset>\""), "{bare}");
        assert!(bare.contains("kind: Script"), "{bare}");
    }

    /// An endpoint's headers are where its second credential goes, so the
    /// debug line names them and never prints them.
    #[test]
    fn an_endpoints_header_values_never_reach_the_debug_line() {
        let endpoint = ModelProviderConfig {
            kind: Some(ModelProviderKind::OpenaiCompatible),
            base_url: Some("http://localhost:8080/v1".to_string()),
            headers: Some(
                [("X-Api-Key".to_string(), "hdr-SECRET-VALUE".to_string())]
                    .into_iter()
                    .collect(),
            ),
            models: Some(vec!["llama-3".to_string()]),
            ..Default::default()
        };
        let rendered = format!("{endpoint:?}");
        assert!(
            !rendered.contains("SECRET-VALUE"),
            "header leaked: {rendered}"
        );
        assert!(rendered.contains("kind: OpenaiCompatible"), "{rendered}");
        assert!(
            rendered.contains("header_names: [\"X-Api-Key\"]"),
            "{rendered}"
        );
        assert!(rendered.contains("llama-3"), "{rendered}");
    }

    /// The kind is spelled the way the file spells it, both ways.
    #[test]
    fn a_kind_round_trips_through_its_spelling() {
        for kind in [
            ModelProviderKind::Script,
            ModelProviderKind::OpenaiCompatible,
            ModelProviderKind::Openai,
        ] {
            assert_eq!(ModelProviderKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(ModelProviderKind::parse("vllm"), None);
        assert_eq!(
            ModelProviderConfig::default().kind(),
            ModelProviderKind::Script
        );
        assert!(!ModelProviderConfig::default().is_endpoint());
    }

    /// The headers reach a provider constructor as an ordered list.
    #[test]
    fn header_pairs_are_listed_in_name_order_and_empty_when_unset() {
        assert!(ModelProviderConfig::default().header_pairs().is_empty());
        let endpoint = ModelProviderConfig {
            headers: Some(
                [
                    ("X-Org".to_string(), "research".to_string()),
                    ("Authorization".to_string(), "Bearer t".to_string()),
                ]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        };
        assert_eq!(
            endpoint.header_pairs(),
            vec![
                ("Authorization".to_string(), "Bearer t".to_string()),
                ("X-Org".to_string(), "research".to_string()),
            ]
        );
    }

    /// The entry `text` is, as `[model_providers.<name>]` holds it.
    fn entry(text: &str) -> ModelProviderConfig {
        toml::from_str(text).expect("the entry parses")
    }

    /// A script entry is never wrong for what it lacks, and forwards whatever
    /// it is given; a well-formed endpoint of either kind passes, with the
    /// keys it reads off the flatten.
    #[test]
    fn a_script_and_a_well_formed_endpoint_validate() {
        assert_eq!(
            entry("script = \"groq.rhai\"\norg = \"research\"\n").validate("groq"),
            Ok(())
        );
        assert_eq!(
            entry("kind = \"openai-compatible\"\nbase_url = \"http://h/v1\"\n").validate("mock"),
            Ok(())
        );
        let azure = entry(
            "kind = \"openai\"\nbase_url = \"https://east.openai.azure.com/openai/v1\"\n\
             api_key = \"east\"\nauth_header = \"api-key\"\nretention = \"zero\"\n\
             zero_retention_request = \"openai\"\n",
        );
        assert_eq!(azure.validate("azure"), Ok(()));
        assert!(azure.is_endpoint() && azure.is_openai());
        assert_eq!(azure.auth_header().as_deref(), Some("api-key"));
        assert_eq!(entry("script = \"s\"\n").auth_header(), None);
    }

    /// Each refusal names the entry and what to add: an endpoint with nowhere
    /// to send a request, OpenAI's API with no key, a blank auth header.
    #[test]
    fn an_endpoint_that_cannot_work_is_refused_by_name() {
        let refusal = |text: &str| entry(text).validate("a").expect_err("refused");

        let e = refusal("kind = \"openai\"\napi_key = \"k\"\n");
        assert!(e.contains("[model_providers.a]"), "{e}");
        assert!(e.contains("kind = \"openai\" but no base_url"), "{e}");

        let e = refusal("kind = \"openai\"\nbase_url = \"https://h/v1\"\n");
        assert!(e.contains("no api_key"), "{e}");
        // A blank key is no key.
        let e = refusal("kind = \"openai\"\nbase_url = \"https://h/v1\"\napi_key = \" \"\n");
        assert!(e.contains("no api_key"), "{e}");

        let e = refusal("kind = \"openai-compatible\"\napi_key = \"k\"\n");
        assert!(
            e.contains("kind = \"openai-compatible\" but no base_url"),
            "{e}"
        );
        // A blank address is no address.
        let e = refusal("kind = \"openai-compatible\"\nbase_url = \"  \"\n");
        assert!(
            e.contains("kind = \"openai-compatible\" but no base_url"),
            "{e}"
        );

        let e = refusal(
            "kind = \"openai\"\nbase_url = \"https://h/v1\"\napi_key = \"k\"\nauth_header = \" \"\n",
        );
        assert!(e.contains("auth_header must name a header"), "{e}");
    }

    /// An endpoint has no script to forward `extra` to, so a key it does not
    /// read is a misspelling that would otherwise load clean and do nothing.
    /// The refusal names the keys, in order, and the ones an endpoint reads.
    #[test]
    fn an_endpoint_with_keys_it_does_not_read_is_refused() {
        let e = entry(
            "kind = \"openai-compatible\"\nbase_url = \"http://localhost:8080/v1\"\n\
             modles = [\"qwen3-8b\"]\nheaeders = { X-Org = \"research\" }\n",
        )
        .validate("llama")
        .expect_err("refused");
        assert!(e.contains("[model_providers.llama]"), "{e}");
        assert!(e.contains("unknown key(s) heaeders, modles"), "{e}");
        assert!(e.contains("headers, models"), "{e}");
    }
}
