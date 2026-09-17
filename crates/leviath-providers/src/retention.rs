//! What a provider keeps of a request once the reply is back, and whether
//! zero retention can be asked for.
//!
//! Every provider Leviath ships answers a [`RetentionPolicy`] for a model: how
//! long prompts and outputs stay on the provider's side, who can change that,
//! and one sentence a person can act on. The compiled-in table here is what
//! each provider documented on 2026-09-14; a provider that can read its own
//! account setting (Bedrock) answers from that instead, through
//! [`crate::Provider::live_retention`]. The registry then lays the operator's
//! settings on top ([`resolve`]): a per-model override, a contract the
//! organisation holds, or the request for zero retention itself.
//!
//! The four kinds of control are the whole story of why this is not one flag:
//!
//! - **Fixed.** Nothing to set. Local inference keeps nothing; Meshy must keep
//!   a task's outputs so they can be downloaded; a subscription transport
//!   follows the account behind it.
//! - **Per request.** OpenRouter routes a request only to endpoints with a
//!   zero-retention policy when asked (`provider.zdr`), and refuses one that
//!   has none rather than routing it elsewhere.
//! - **Account.** Bedrock keeps a data retention mode on the account
//!   (`GET`/`PUT /data-retention`), ordered `none < default < aws_review`, and
//!   each model says which modes it allows.
//! - **Agreement.** OpenAI, Anthropic and Google keep an abuse-monitoring copy
//!   for a fixed number of days unless the organisation holds a zero data
//!   retention contract with them. No API reads or sets it, so the operator
//!   declares it.
//!
//! Some models retain regardless: Claude Fable 5 and Mythos 5 keep prompts and
//! outputs 30 days for safety review on every platform, and are not available
//! under a zero-retention agreement without the provider's express say-so.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// How long a provider keeps prompts and outputs after a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    /// Nothing is written to durable storage once the reply is returned.
    Zero,
    /// Kept for this many days: an abuse-monitoring log, a safety review, a
    /// task's downloadable outputs.
    Days(u32),
    /// Kept until the user deletes it, or with no stated limit.
    Indefinite,
    /// The provider has not said, or Leviath cannot tell from here.
    Unknown,
}

impl Retention {
    /// The word a config file spells it with: `zero`, `30d`, `indefinite`,
    /// `unknown`.
    pub fn as_word(self) -> String {
        match self {
            Retention::Zero => "zero".to_string(),
            Retention::Days(n) => format!("{n}d"),
            Retention::Indefinite => "indefinite".to_string(),
            Retention::Unknown => "unknown".to_string(),
        }
    }

    /// Read the word back. `30d`, `30 days` and `30` all mean thirty days.
    pub fn parse(word: &str) -> Result<Self, String> {
        let w = word.trim().to_ascii_lowercase();
        match w.as_str() {
            "zero" | "none" | "0" | "0d" => return Ok(Retention::Zero),
            "indefinite" | "forever" => return Ok(Retention::Indefinite),
            "unknown" => return Ok(Retention::Unknown),
            _ => {}
        }
        let digits: String = w.chars().take_while(|c| c.is_ascii_digit()).collect();
        let rest = w.trim_start_matches(|c: char| c.is_ascii_digit()).trim();
        if !digits.is_empty() && matches!(rest, "" | "d" | "day" | "days") {
            return digits
                .parse::<u32>()
                .map(Retention::Days)
                .map_err(|_| format!("'{word}' is too many days"));
        }
        Err(format!(
            "'{word}' is not a retention: zero, <N>d (30d), indefinite, or unknown"
        ))
    }

    /// How it reads to a person.
    pub fn describe(self) -> String {
        match self {
            Retention::Zero => "zero".to_string(),
            Retention::Days(1) => "1 day".to_string(),
            Retention::Days(n) => format!("{n} days"),
            Retention::Indefinite => "indefinite".to_string(),
            Retention::Unknown => "unknown".to_string(),
        }
    }
}

impl Serialize for Retention {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_word())
    }
}

impl<'de> Deserialize<'de> for Retention {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let word = String::deserialize(d)?;
        Retention::parse(&word).map_err(serde::de::Error::custom)
    }
}

/// Who can turn zero retention on for a provider, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    /// Nothing to set: the answer is what it is.
    Fixed,
    /// A field on each request, sent when zero retention is asked for.
    PerRequest,
    /// An account-level setting the provider's API reads and writes.
    Account,
    /// A contract with the provider, declared in
    /// `[providers] zero_retention_agreements`.
    Agreement,
}

impl Control {
    /// How it reads to a person.
    pub fn describe(self) -> &'static str {
        match self {
            Control::Fixed => "fixed",
            Control::PerRequest => "per request",
            Control::Account => "account setting",
            Control::Agreement => "by agreement",
        }
    }
}

/// Where a policy's answer came from, most trusted last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The table compiled into this build.
    Builtin,
    /// Read from the provider's account just now (or at start-up).
    Live,
    /// Zero retention was asked for and this provider grants it per request.
    Requested,
    /// The operator declared an agreement with the provider.
    Declared,
    /// A `retention` key on a `[model_capabilities]` or `[model_providers]`
    /// entry.
    Override,
}

/// What one provider keeps of one model's requests, and what can be done
/// about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// How long prompts and outputs stay with the provider.
    pub retention: Retention,
    /// Who can change that.
    pub control: Control,
    /// Where this answer came from.
    pub source: Source,
    /// One sentence a person can act on: what is kept, why, and what turns
    /// it off.
    pub note: String,
}

impl RetentionPolicy {
    fn new(retention: Retention, control: Control, note: impl Into<String>) -> Self {
        Self {
            retention,
            control,
            source: Source::Builtin,
            note: note.into(),
        }
    }

    /// Whether nothing is kept.
    pub fn is_zero(&self) -> bool {
        self.retention == Retention::Zero
    }

    /// One line: `30 days (by agreement, builtin)`.
    pub fn summary(&self) -> String {
        format!(
            "{} ({}, {})",
            self.retention.describe(),
            self.control.describe(),
            match self.source {
                Source::Builtin => "documented",
                Source::Live => "read from the account",
                Source::Requested => "requested per request",
                Source::Declared => "declared agreement",
                Source::Override => "config override",
            }
        )
    }
}

/// Whether `model` is one of the Claude models that keep prompts and outputs
/// 30 days for safety review wherever they are served: Claude Fable 5 and
/// 5.1, Claude Mythos 5 and 5.1. Matched on the id, which every platform
/// spells with the family name in it (`claude-fable-5-1`,
/// `anthropic.claude-fable-5`, `us.anthropic.claude-mythos-5-1`).
pub fn is_covered_claude(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("claude-fable-5") || m.contains("claude-mythos-5")
}

/// The policy compiled in for `provider` (a registry name) and `model`,
/// before any account reading, declaration, override or request is laid on.
pub fn builtin(provider: &str, model: &str) -> RetentionPolicy {
    match provider {
        "anthropic" => {
            if is_covered_claude(model) {
                RetentionPolicy::new(
                    Retention::Days(30),
                    Control::Fixed,
                    "Anthropic keeps this model's prompts and outputs 30 days for safety \
                     review on every platform; it is not available under a zero data \
                     retention agreement unless Anthropic expressly authorises it",
                )
            } else {
                RetentionPolicy::new(
                    Retention::Days(30),
                    Control::Agreement,
                    "Anthropic's commercial API keeps prompts and outputs up to 30 days \
                     for trust and safety, never for training; a zero data retention \
                     agreement with Anthropic ends that (declare it in [providers] \
                     zero_retention_agreements)",
                )
            }
        }
        "openai" => RetentionPolicy::new(
            Retention::Days(30),
            Control::Agreement,
            "OpenAI keeps API prompts and outputs up to 30 days for abuse monitoring, \
             never for training; with zero retention requested Leviath sends store=false \
             so no stored-completion copy is kept, and only a Zero Data Retention \
             agreement with OpenAI removes the abuse log (declare it in [providers] \
             zero_retention_agreements)",
        ),
        "google" => RetentionPolicy::new(
            Retention::Days(55),
            Control::Agreement,
            "the paid Gemini API keeps prompts 55 days for abuse monitoring and never \
             trains on them; zero retention is granted per project on request to \
             Google (declare it in [providers] zero_retention_agreements). The free \
             tier trains on prompts",
        ),
        "openrouter" => RetentionPolicy::new(
            Retention::Unknown,
            Control::PerRequest,
            "OpenRouter keeps nothing itself unless prompt logging is on; the endpoint \
             it routes to keeps whatever that vendor keeps. With zero retention \
             requested Leviath sends provider.zdr=true and data_collection=deny, so a \
             request goes only to an endpoint with a zero-retention policy and a model \
             with none is refused rather than routed elsewhere",
        ),
        "bedrock" => {
            if is_covered_claude(model) {
                RetentionPolicy::new(
                    Retention::Days(30),
                    Control::Account,
                    "this model requires the account's data retention mode to be \
                     aws_review: AWS keeps prompts and outputs up to 30 days inside AWS \
                     for the human review Anthropic requires, and never shares them \
                     with Anthropic. Under mode none the model is unavailable",
                )
            } else {
                RetentionPolicy::new(
                    Retention::Zero,
                    Control::Account,
                    "Amazon Bedrock keeps nothing for a model that allows mode none, \
                     whatever the account's data retention mode; the mode itself is \
                     read and set with `lev providers retention` (GET/PUT \
                     /data-retention), and model invocation logging is off unless \
                     your account turned it on",
                )
            }
        }
        "meshy" => RetentionPolicy::new(
            Retention::Days(3),
            Control::Fixed,
            "Meshy keeps an API task's inputs and outputs (model files, previews, \
             textures) 3 days so they can be downloaded, indefinitely on an \
             enterprise plan; nothing is used for training, and there is no \
             zero-retention option because the outputs are the product",
        ),
        "ollama" | "llama-cpp" | "lm-studio" => RetentionPolicy::new(
            Retention::Zero,
            Control::Fixed,
            "local inference: nothing leaves the machine",
        ),
        "xai" => RetentionPolicy::new(
            Retention::Days(30),
            Control::Agreement,
            "xAI keeps API requests and responses 30 days for abuse monitoring, never \
             for training; Leviath never asks it to store a response, and zero data \
             retention is an arrangement with xAI for a team or enterprise (declare it in \
             [providers] zero_retention_agreements)",
        ),
        "grok" => RetentionPolicy::new(
            Retention::Unknown,
            Control::Fixed,
            "billed to a Grok subscription, so that account's terms apply rather than \
             the API's; the account's coding data retention setting is shown by `lev \
             providers retention`, and xAI does not say whether it covers these requests",
        ),
        "meta" => {
            if model.to_ascii_lowercase().contains("-contributor") {
                RetentionPolicy::new(
                    Retention::Indefinite,
                    Control::Fixed,
                    "a contributor-tier model: Meta prices it lower in exchange for the \
                     right to train future models on its prompts and completions",
                )
            } else {
                RetentionPolicy::new(
                    Retention::Unknown,
                    Control::Fixed,
                    "Meta does not train on a standard-tier model's prompts or \
                     completions and publishes no retention window for them",
                )
            }
        }
        "codex" => RetentionPolicy::new(
            Retention::Unknown,
            Control::Fixed,
            "billed to a ChatGPT subscription, so the terms of that account apply, \
             not the API's: no zero data retention agreement covers it and there is \
             no API control",
        ),
        "claude-code" => RetentionPolicy::new(
            Retention::Unknown,
            Control::Fixed,
            "the account behind the claude CLI decides: an API key from a Commercial \
             organisation inherits that organisation's arrangement (zero data \
             retention included); a consumer subscription follows consumer terms",
        ),
        _ => RetentionPolicy::new(
            Retention::Unknown,
            Control::Fixed,
            "nothing is known about this host's retention; set `retention` on its \
             [model_providers] entry if you know its policy",
        ),
    }
}

/// The operator's settings, laid over the compiled-in table by [`resolve`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionSettings {
    /// `[providers] zero_retention`: ask every provider for zero retention
    /// and refuse a model that cannot give it.
    pub zero_requested: bool,
    /// `[providers] zero_retention_agreements`: providers this organisation
    /// holds a zero data retention contract with.
    pub agreements: Vec<String>,
    /// `retention` on a `[model_capabilities.<model>]` entry, by model id.
    pub model_overrides: HashMap<String, Retention>,
    /// `retention` on a `[model_providers.<name>]` entry, by provider name.
    pub provider_declarations: HashMap<String, Retention>,
    /// `zero_retention_request` on a `[model_providers.<name>]` entry, by
    /// provider name: the built-in provider whose per-request zero-retention
    /// fields ([`request_knobs`]) this host takes. An OpenAI-shaped gateway
    /// or an Azure OpenAI deployment names `openai` here and is sent
    /// `store = false` with the switch on.
    pub request_knob_aliases: HashMap<String, String>,
    /// `[providers] file_uploads`: put a large part in the vendor's file
    /// storage once and name it by id after. Never done under zero retention,
    /// whatever this says; see [`Self::uploads_allowed`].
    pub file_uploads: bool,
}

impl RetentionSettings {
    /// Whether a part may be uploaded to a vendor's file storage: the switch
    /// is on and zero retention is not asked for. An upload is data the
    /// vendor keeps, which zero retention rules out.
    pub fn uploads_allowed(&self) -> bool {
        self.file_uploads && !self.zero_requested
    }

    /// Why nothing is uploaded, in words for a stand-in, or empty when uploads
    /// are allowed.
    pub fn why_inline(&self) -> &'static str {
        match (self.zero_requested, self.file_uploads) {
            (true, _) => "zero data retention is on, so nothing is uploaded",
            (false, false) => "[providers] file_uploads is off",
            (false, true) => "",
        }
    }

    /// The provider whose request fields `provider` is sent: its alias when
    /// one is declared, else itself.
    pub fn knob_provider<'a>(&'a self, provider: &'a str) -> &'a str {
        self.request_knob_aliases
            .get(provider)
            .map_or(provider, String::as_str)
    }
}

/// `base` with the operator's settings applied, in the order they are
/// trusted: a per-model override wins outright; a provider declaration
/// answers for a host the table knows nothing about; a declared agreement
/// turns an agreement-controlled provider to zero (never a model that
/// retains regardless); and a request for zero retention turns a
/// per-request provider to zero.
pub fn resolve(
    base: RetentionPolicy,
    provider: &str,
    model: &str,
    settings: &RetentionSettings,
) -> RetentionPolicy {
    if let Some(retention) = settings.model_overrides.get(model) {
        return RetentionPolicy {
            retention: *retention,
            control: base.control,
            source: Source::Override,
            note: format!(
                "[model_capabilities.\"{model}\"] retention = \"{}\"",
                retention.as_word()
            ),
        };
    }
    if let Some(retention) = settings.provider_declarations.get(provider) {
        return RetentionPolicy {
            retention: *retention,
            control: base.control,
            source: Source::Override,
            note: format!(
                "[model_providers.{provider}] retention = \"{}\"",
                retention.as_word()
            ),
        };
    }
    if base.control == Control::Agreement && settings.agreements.iter().any(|a| a == provider) {
        return RetentionPolicy {
            retention: Retention::Zero,
            control: base.control,
            source: Source::Declared,
            note: format!(
                "zero data retention agreement with {provider}, declared in [providers] \
                 zero_retention_agreements"
            ),
        };
    }
    if settings.zero_requested && base.control == Control::PerRequest {
        return RetentionPolicy {
            retention: Retention::Zero,
            control: base.control,
            source: Source::Requested,
            note: base.note,
        };
    }
    base
}

/// The request fields that ask `provider` for zero retention, to merge into
/// a request's extra parameters. Only the providers that take one per
/// request have any.
pub fn request_knobs(provider: &str) -> Option<serde_json::Value> {
    match provider {
        // No stored-completion copy. The abuse-monitoring log is an
        // organisation-level matter OpenAI settles by agreement, and under an
        // approved one `store` is forced to false anyway.
        "openai" => Some(serde_json::json!({ "store": false })),
        // Route only to endpoints with a zero-retention policy, and to none
        // that train on or keep inputs.
        "openrouter" => Some(serde_json::json!({
            "provider": { "zdr": true, "data_collection": "deny" }
        })),
        _ => None,
    }
}

/// Merge [`request_knobs`] for `provider` into `extra`, which is `Null` or an
/// object. A key the stage's own parameters already set is left alone: the
/// operator wrote it on purpose.
pub fn apply_request_knobs(provider: &str, extra: &mut serde_json::Value) {
    let Some(serde_json::Value::Object(knobs)) = request_knobs(provider) else {
        return;
    };
    if !extra.is_object() {
        *extra = serde_json::Value::Object(serde_json::Map::new());
    }
    let target = extra.as_object_mut().expect("just made it an object");
    for (key, value) in knobs {
        target.entry(key).or_insert(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An endpoint that declared whose request fields it takes is answered
    /// with that provider's; one that did not is answered with itself.
    #[test]
    fn a_request_knob_alias_names_whose_fields_a_host_takes() {
        let settings = RetentionSettings {
            request_knob_aliases: HashMap::from([("azure".to_string(), "openai".to_string())]),
            ..Default::default()
        };
        assert_eq!(settings.knob_provider("azure"), "openai");
        assert_eq!(settings.knob_provider("openrouter"), "openrouter");
        assert!(request_knobs(settings.knob_provider("azure")).is_some());
        assert!(request_knobs(settings.knob_provider("gw")).is_none());
    }

    #[test]
    fn a_retention_round_trips_through_its_word() {
        for (word, expected) in [
            ("zero", Retention::Zero),
            ("none", Retention::Zero),
            ("0", Retention::Zero),
            ("30d", Retention::Days(30)),
            ("30 days", Retention::Days(30)),
            ("1 day", Retention::Days(1)),
            ("55", Retention::Days(55)),
            ("indefinite", Retention::Indefinite),
            ("Forever", Retention::Indefinite),
            ("unknown", Retention::Unknown),
        ] {
            assert_eq!(Retention::parse(word), Ok(expected), "{word}");
        }
        for bad in ["", "soon", "30x", "99999999999d"] {
            assert!(Retention::parse(bad).is_err(), "{bad}");
        }
        for r in [
            Retention::Zero,
            Retention::Days(30),
            Retention::Indefinite,
            Retention::Unknown,
        ] {
            assert_eq!(Retention::parse(&r.as_word()), Ok(r));
            let json = serde_json::to_string(&r).unwrap();
            let back: Retention = serde_json::from_str(&json).unwrap();
            assert_eq!(back, r);
        }
        assert!(serde_json::from_str::<Retention>("\"later\"").is_err());
        assert!(
            serde_json::from_str::<Retention>("3").is_err(),
            "a number is not a word"
        );
        assert!(
            serde_json::from_str::<Retention>("3").is_err(),
            "a number is not a word"
        );
        assert!(
            serde_json::from_str::<Retention>("3").is_err(),
            "a number is not a word"
        );
        assert_eq!(Retention::Days(1).describe(), "1 day");
        assert_eq!(Retention::Days(30).describe(), "30 days");
        assert_eq!(Retention::Zero.describe(), "zero");
        assert_eq!(Retention::Indefinite.describe(), "indefinite");
        assert_eq!(Retention::Unknown.describe(), "unknown");
    }

    #[test]
    fn the_table_answers_for_every_shipped_provider() {
        assert_eq!(
            builtin("anthropic", "claude-sonnet-5").retention,
            Retention::Days(30)
        );
        assert_eq!(
            builtin("anthropic", "claude-sonnet-5").control,
            Control::Agreement
        );
        // The covered models retain regardless of any agreement.
        assert_eq!(
            builtin("anthropic", "claude-fable-5-1").control,
            Control::Fixed
        );
        assert_eq!(
            builtin("anthropic", "claude-mythos-5").retention,
            Retention::Days(30)
        );
        assert_eq!(builtin("openai", "gpt-5.5").retention, Retention::Days(30));
        assert_eq!(
            builtin("google", "gemini-3.5-flash").retention,
            Retention::Days(55)
        );
        assert_eq!(builtin("openrouter", "x").control, Control::PerRequest);
        assert_eq!(
            builtin("bedrock", "amazon.nova-2").retention,
            Retention::Zero
        );
        assert_eq!(
            builtin("bedrock", "amazon.nova-2").control,
            Control::Account
        );
        assert_eq!(
            builtin("bedrock", "us.anthropic.claude-fable-5").retention,
            Retention::Days(30)
        );
        assert_eq!(
            builtin("meshy", "image-to-3d").retention,
            Retention::Days(3)
        );
        for local in ["ollama", "llama-cpp", "lm-studio"] {
            assert!(builtin(local, "qwen3.5:9b").is_zero(), "{local}");
        }
        assert_eq!(
            builtin("codex", "gpt-5.6-sol").retention,
            Retention::Unknown
        );
        assert_eq!(builtin("claude-code", "opus").retention, Retention::Unknown);
        assert_eq!(
            builtin("cerebras", "gpt-oss-120b").retention,
            Retention::Unknown
        );
        assert!(is_covered_claude("Anthropic.Claude-Fable-5-1"));
        assert!(!is_covered_claude("claude-opus-5"));
        let p = builtin("openai", "gpt-5.5");
        assert_eq!(p.summary(), "30 days (by agreement, documented)");
        for c in [
            Control::Fixed,
            Control::PerRequest,
            Control::Account,
            Control::Agreement,
        ] {
            assert!(!c.describe().is_empty());
        }
    }

    #[test]
    fn settings_are_laid_over_the_table_in_trust_order() {
        let mut settings = RetentionSettings::default();
        // Nothing set: the table stands.
        assert_eq!(
            resolve(builtin("openai", "gpt-5.5"), "openai", "gpt-5.5", &settings).source,
            Source::Builtin
        );
        // A request for zero retention only moves a per-request provider.
        settings.zero_requested = true;
        let or = resolve(builtin("openrouter", "m"), "openrouter", "m", &settings);
        assert!(or.is_zero());
        assert_eq!(or.source, Source::Requested);
        assert_eq!(or.summary(), "zero (per request, requested per request)");
        let oa = resolve(builtin("openai", "gpt-5.5"), "openai", "gpt-5.5", &settings);
        assert!(!oa.is_zero());
        // A declared agreement turns an agreement provider to zero, but never
        // a model that retains regardless.
        settings.agreements = vec!["openai".to_string(), "anthropic".to_string()];
        let oa = resolve(builtin("openai", "gpt-5.5"), "openai", "gpt-5.5", &settings);
        assert!(oa.is_zero());
        assert_eq!(oa.source, Source::Declared);
        assert_eq!(oa.summary(), "zero (by agreement, declared agreement)");
        let covered = resolve(
            builtin("anthropic", "claude-fable-5-1"),
            "anthropic",
            "claude-fable-5-1",
            &settings,
        );
        assert_eq!(covered.retention, Retention::Days(30));
        assert_eq!(covered.source, Source::Builtin);
        // A provider declaration answers for a host the table cannot.
        settings
            .provider_declarations
            .insert("cerebras".to_string(), Retention::Zero);
        let cb = resolve(builtin("cerebras", "m"), "cerebras", "m", &settings);
        assert!(cb.is_zero());
        assert_eq!(cb.source, Source::Override);
        assert!(
            cb.note.contains("[model_providers.cerebras]"),
            "{}",
            cb.note
        );
        // A per-model override wins over everything, either way.
        settings
            .model_overrides
            .insert("gpt-5.5".to_string(), Retention::Indefinite);
        let oa = resolve(builtin("openai", "gpt-5.5"), "openai", "gpt-5.5", &settings);
        assert_eq!(oa.retention, Retention::Indefinite);
        assert_eq!(oa.source, Source::Override);
        assert_eq!(oa.summary(), "indefinite (by agreement, config override)");
        let live = RetentionPolicy {
            source: Source::Live,
            ..builtin("bedrock", "amazon.nova-2")
        };
        assert_eq!(
            live.summary(),
            "zero (account setting, read from the account)"
        );
    }

    #[test]
    fn request_knobs_reach_only_the_providers_that_take_them() {
        let mut extra = serde_json::Value::Null;
        apply_request_knobs("anthropic", &mut extra);
        assert!(extra.is_null());
        apply_request_knobs("openai", &mut extra);
        assert_eq!(extra, serde_json::json!({ "store": false }));
        // A key the stage set itself is kept.
        let mut extra = serde_json::json!({ "store": true, "top_p": 0.9 });
        apply_request_knobs("openai", &mut extra);
        assert_eq!(extra["store"], serde_json::json!(true));
        assert_eq!(extra["top_p"], serde_json::json!(0.9));
        let mut extra = serde_json::json!({ "seed": 7 });
        apply_request_knobs("openrouter", &mut extra);
        assert_eq!(extra["provider"]["zdr"], serde_json::json!(true));
        assert_eq!(
            extra["provider"]["data_collection"],
            serde_json::json!("deny")
        );
        assert_eq!(extra["seed"], serde_json::json!(7));
        assert!(request_knobs("bedrock").is_none());
    }
}

#[cfg(test)]
mod upload_tests {
    use super::*;

    #[test]
    fn uploads_need_the_switch_and_no_zero_retention_and_say_why_not() {
        let mut settings = RetentionSettings {
            file_uploads: true,
            ..Default::default()
        };
        assert!(settings.uploads_allowed());
        assert_eq!(settings.why_inline(), "");
        settings.zero_requested = true;
        assert!(!settings.uploads_allowed());
        assert!(settings.why_inline().contains("zero data retention"));
        settings.zero_requested = false;
        settings.file_uploads = false;
        assert!(!settings.uploads_allowed());
        assert!(settings.why_inline().contains("file_uploads"));
    }
}

#[cfg(test)]
mod new_provider_tests {
    use super::*;

    #[test]
    fn xai_grok_and_meta_each_say_what_they_keep() {
        assert_eq!(builtin("xai", "grok-4.3").retention, Retention::Days(30));
        assert_eq!(builtin("xai", "grok-4.3").control, Control::Agreement);
        assert_eq!(builtin("grok", "grok-4.6").retention, Retention::Unknown);
        assert_eq!(
            builtin("meta", "muse-spark-1.3-contributor").retention,
            Retention::Indefinite
        );
        assert_eq!(
            builtin("meta", "muse-spark-1.3").retention,
            Retention::Unknown
        );
    }
}
