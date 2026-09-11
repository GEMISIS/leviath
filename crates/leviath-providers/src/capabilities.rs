//! What a model can do, and the per-model corrections a user can apply.
//!
//! Separate from [`crate::provider`] for the same reason [`crate::pricing`] is:
//! that module is how a provider is CALLED, this one describes the model it
//! calls. They meet where a provider answers [`crate::Provider::capabilities`].

use serde::{Deserialize, Serialize};

/// How one row of a built-in capability table recognises a model name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Match {
    /// The name starts with this.
    Prefix(&'static str),
    /// The name contains this anywhere.
    Contains(&'static str),
    /// The name starts with the first and contains the second.
    PrefixAnd(&'static str, &'static str),
}

impl Match {
    /// Whether `model` is what this pattern describes.
    fn hits(self, model: &str) -> bool {
        match self {
            Self::Prefix(p) => model.starts_with(p),
            Self::Contains(c) => model.contains(c),
            Self::PrefixAnd(p, c) => model.starts_with(p) && model.contains(c),
        }
    }

    /// A model name this pattern recognises, for the table tests.
    #[cfg(test)]
    fn example(self) -> String {
        match self {
            Self::Prefix(p) | Self::Contains(p) => p.to_string(),
            Self::PrefixAnd(p, c) => format!("{p}{c}"),
        }
    }
}

/// One row of a provider's built-in capability table.
///
/// Every model any table knows streams and takes a system prompt, so a row
/// records only what varies: whether it takes a temperature, whether it
/// calls tools, and its two token limits. A row matches when any of its
/// patterns does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Row {
    /// Any of these recognises the model.
    pub(crate) matches: &'static [Match],
    /// See [`ModelCapabilities::supports_temperature`].
    pub(crate) temperature: bool,
    /// See [`ModelCapabilities::supports_tools`].
    pub(crate) tools: bool,
    /// See [`ModelCapabilities::max_context_tokens`].
    pub(crate) context: usize,
    /// See [`ModelCapabilities::max_output_tokens`].
    pub(crate) output: usize,
}

impl Row {
    /// Whether any of this row's patterns recognises `model`.
    fn hits(&self, model: &str) -> bool {
        self.matches.iter().any(|m| m.hits(model))
    }

    /// The capabilities this row describes.
    const fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            supports_temperature: self.temperature,
            supports_streaming: true,
            supports_tools: self.tools,
            supports_system_prompt: true,
            max_context_tokens: self.context,
            max_output_tokens: self.output,
            limits_source: LimitsSource::Builtin,
        }
    }
}

/// The first row of `table` that recognises `model`, or `fallback`.
///
/// First hit wins, which is what lets a table put `gpt-5.5` above `gpt-5`
/// and `claude-opus-4-8` above `claude-opus-4`: the specific row comes
/// first and the family row catches the rest. The four providers that used
/// to spell this as a ladder of `if model.starts_with(..) { return .. }`
/// blocks each carried a note about that ordering; now the table is the
/// note.
pub(crate) fn lookup(table: &[Row], model: &str, fallback: ModelCapabilities) -> ModelCapabilities {
    table
        .iter()
        .find(|row| row.hits(model))
        .map_or(fallback, Row::capabilities)
}

/// One model this build names outright, for a listing that cannot ask.
///
/// The tables above recognise a model by a pattern in its name and so cannot
/// enumerate anything; a listing wants rows. Each provider names the handful
/// of models worth showing when its listing cannot be reached, and resolves
/// their capabilities through its own table, so a row here and an inference
/// against the same id cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    /// The provider that serves it.
    pub provider: &'static str,
    /// The id a request names.
    pub id: &'static str,
    /// What the provider calls it.
    pub display_name: &'static str,
    /// What the provider's table says about it.
    pub capabilities: ModelCapabilities,
}

/// Every model this build names outright, across the providers that name any.
///
/// Ollama names none: it serves whatever has been pulled, and nothing compiled
/// in could say what that is. OpenRouter names a sample of the hundreds it
/// fronts. The three closed catalogues (Anthropic, OpenAI, Google) name their
/// current line-up, which is what the `unknown-model` lint checks a blueprint
/// against when no listing has been read.
pub fn builtin_catalog() -> Vec<CatalogEntry> {
    let rows = |provider: &'static str,
                catalog: &'static [(&'static str, &'static str)],
                capabilities: fn(&str) -> ModelCapabilities| {
        catalog.iter().map(move |(id, display_name)| CatalogEntry {
            provider,
            id,
            display_name,
            capabilities: capabilities(id),
        })
    };
    rows(
        "anthropic",
        crate::anthropic::CATALOG,
        crate::anthropic::table_capabilities,
    )
    .chain(rows(
        "openai",
        crate::openai::CATALOG,
        crate::openai::table_capabilities,
    ))
    .chain(rows(
        "google",
        crate::gemini::CATALOG,
        crate::gemini::table_capabilities,
    ))
    .chain(rows(
        "openrouter",
        crate::openrouter::CATALOG,
        crate::openrouter::table_capabilities,
    ))
    .chain(rows(
        "claude-code",
        crate::claude_code::CATALOG,
        crate::claude_code::table_capabilities,
    ))
    .collect()
}

/// Capabilities supported by a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    /// Whether the model supports temperature sampling
    pub supports_temperature: bool,

    /// Whether the model supports streaming responses
    pub supports_streaming: bool,

    /// Whether the model supports tool/function calling
    pub supports_tools: bool,

    /// Whether the model supports a system prompt
    pub supports_system_prompt: bool,

    /// Maximum number of context (input) tokens
    pub max_context_tokens: usize,

    /// Maximum number of output tokens
    pub max_output_tokens: usize,

    /// Where the two token limits above came from.
    ///
    /// Carried beside them because they are published - `GET /api/models` and
    /// `lev models` both print them - and a number read from the provider and a
    /// number matched off a substring of the model's name look identical once
    /// printed. They are not worth the same: percentage region budgets resolve
    /// against `max_context_tokens`, so a guess that is wrong by 2x makes every
    /// region in the run wrong by 2x, and nothing downstream can tell.
    #[serde(default)]
    pub limits_source: LimitsSource,
}

/// How a model's token limits were arrived at.
///
/// Ordered by how much it is worth trusting, least first, so `max` picks the
/// better of two answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitsSource {
    /// Matched from the model's name against a table compiled into this build.
    ///
    /// The default, and the honest answer for a provider whose API does not
    /// report limits at all: OpenAI's `/v1/models` returns an id, a date and
    /// an owner and nothing about size. (Anthropic's does report
    /// `max_input_tokens` and `max_tokens`, and `crate::anthropic` reads
    /// them.) It is also the answer
    /// when a provider that *could* say has not been asked yet, since
    /// `prime_capabilities` runs at daemon start-up and a short-lived command
    /// may not have waited for it.
    #[default]
    Builtin,

    /// Read from the provider's own API for this model.
    Api,

    /// Set by the operator in `[model_capabilities]`.
    ///
    /// Ranked above the API because someone who wrote the number down is
    /// correcting something, and the usual something is an API answer that did
    /// not match what the endpoint actually served.
    Override,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 8192,
            max_output_tokens: 4096,
            limits_source: LimitsSource::Builtin,
        }
    }
}

/// What mime a model takes and produces, as mime type patterns.
///
/// Kept beside [`ModelCapabilities`] rather than inside it so the compiled
/// tables, which build that struct in `const` context, stay as they are and a
/// provider answers this the same three-layered way: its table, then what its
/// listing said, then the operator's `[model_capabilities]` row.
///
/// A pattern is `type/subtype` or `type/*`. `text/*` is listed explicitly,
/// because text is not assumed: a text-to-speech or image model may take no
/// text at all, and a listing that omits it is saying so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelMime {
    /// Patterns the model accepts in a request.
    pub input: Vec<String>,
    /// Patterns the model can hand back in a reply.
    pub output: Vec<String>,
}

impl Default for ModelMime {
    fn default() -> Self {
        Self::text_only()
    }
}

impl ModelMime {
    /// Text in, text out: the answer for a model nothing has described.
    pub fn text_only() -> Self {
        Self::new(&["text/*"], &["text/*"])
    }

    /// From two pattern lists.
    pub fn new(input: &[&str], output: &[&str]) -> Self {
        Self {
            input: input.iter().map(|s| s.to_string()).collect(),
            output: output.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Whether a part of `mime_type` may go to the model.
    pub fn accepts(&self, mime_type: &leviath_core::mime::MimeType) -> bool {
        mime_type.matches_any(&self.input)
    }

    /// Whether the model may hand back a part of `mime_type`.
    pub fn produces(&self, mime_type: &leviath_core::mime::MimeType) -> bool {
        mime_type.matches_any(&self.output)
    }

    /// Whether the model emits images, so its request is built for image
    /// generation (the prompt in the user turn) rather than for a text reply.
    pub fn produces_images(&self) -> bool {
        self.output
            .iter()
            .any(|p| p == "image/*" || p.starts_with("image/"))
    }

    /// Whether every pattern in `wanted` is covered by an input pattern.
    ///
    /// `image/png` is covered by `image/png`, `image/*` or `*/*`; `image/*`
    /// only by `image/*` or `*/*`.
    pub fn covers(&self, wanted: &[String]) -> bool {
        wanted
            .iter()
            .all(|w| self.input.iter().any(|have| pattern_covers(have, w)))
    }

    /// Whether the model takes anything beyond text.
    pub fn takes_mime(&self) -> bool {
        self.input.iter().any(|p| !p.starts_with("text/"))
    }
}

/// Whether the pattern `have` covers everything `want` matches.
pub fn pattern_covers(have: &str, want: &str) -> bool {
    let have = have.trim().to_ascii_lowercase();
    let want = want.trim().to_ascii_lowercase();
    if have == "*/*" || have == want {
        return true;
    }
    match (have.split_once('/'), want.split_once('/')) {
        (Some((hk, "*")), Some((wk, _))) => hk == wk,
        _ => false,
    }
}

/// A `[model_capabilities]` entry: the fields an operator chose to change.
///
/// Every field is optional and unset means "leave it alone", so an entry names
/// only what it is correcting. The alternative - deserializing straight into
/// [`ModelCapabilities`] - has two failure modes. Without field defaults a
/// partial table fails to deserialize and the override is dropped in
/// silence. With `#[serde(default)]` it succeeds
/// and quietly substitutes [`ModelCapabilities::default`] for everything the
/// operator did not mention, so correcting one boolean would drop a 400 000
/// token window to 8 192.
///
/// Merging onto the provider's own answer for that model is the only reading
/// that matches what the table looks like it does.
// No `Eq`: rates are `f64`, which has no total equality.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelCapabilityOverride {
    /// See [`ModelCapabilities::supports_temperature`].
    pub supports_temperature: Option<bool>,
    /// See [`ModelCapabilities::supports_streaming`].
    pub supports_streaming: Option<bool>,
    /// See [`ModelCapabilities::supports_tools`].
    pub supports_tools: Option<bool>,
    /// See [`ModelCapabilities::supports_system_prompt`].
    pub supports_system_prompt: Option<bool>,
    /// See [`ModelCapabilities::max_context_tokens`].
    pub max_context_tokens: Option<usize>,
    /// See [`ModelCapabilities::max_output_tokens`].
    pub max_output_tokens: Option<usize>,

    /// USD per million fresh input tokens. Overrides the shipped rate table,
    /// and is the only place a negotiated price can be recorded - no public
    /// pricing page shows one. See `crate::pricing::published_rates`.
    #[serde(default)]
    pub input_per_mtok: Option<f64>,
    /// USD per million cached input tokens. Defaults to the input rate.
    #[serde(default)]
    pub cached_input_per_mtok: Option<f64>,
    /// USD per million tokens written to cache. Defaults to the input rate.
    #[serde(default)]
    pub cache_write_per_mtok: Option<f64>,
    /// USD per million output tokens.
    #[serde(default)]
    pub output_per_mtok: Option<f64>,

    /// Mime type patterns the model accepts, replacing what the provider
    /// reports: `["text/*", "image/*"]` for a local vision model.
    #[serde(default)]
    pub input_types: Option<Vec<String>>,
    /// Mime type patterns the model can hand back.
    #[serde(default)]
    pub output_types: Option<Vec<String>>,
}

impl ModelCapabilityOverride {
    /// `base` mime lists with the ones this entry names replaced.
    pub fn apply_mime(&self, base: ModelMime) -> ModelMime {
        ModelMime {
            input: self.input_types.clone().unwrap_or(base.input),
            output: self.output_types.clone().unwrap_or(base.output),
        }
    }

    /// `base` with every field this entry names replaced.
    pub fn apply_to(&self, base: ModelCapabilities) -> ModelCapabilities {
        ModelCapabilities {
            supports_temperature: self
                .supports_temperature
                .unwrap_or(base.supports_temperature),
            supports_streaming: self.supports_streaming.unwrap_or(base.supports_streaming),
            supports_tools: self.supports_tools.unwrap_or(base.supports_tools),
            supports_system_prompt: self
                .supports_system_prompt
                .unwrap_or(base.supports_system_prompt),
            max_context_tokens: self.max_context_tokens.unwrap_or(base.max_context_tokens),
            max_output_tokens: self.max_output_tokens.unwrap_or(base.max_output_tokens),
            // Only a limit the operator actually named makes this theirs. An
            // entry that corrects `supports_temperature` and says nothing about
            // size would otherwise relabel an API-read window as hand-set, and
            // the label is there to say how much the number is worth.
            limits_source: match self.names_a_limit() {
                true => LimitsSource::Override,
                false => base.limits_source,
            },
        }
    }

    /// Whether this entry sets either token limit.
    pub fn names_a_limit(&self) -> bool {
        self.max_context_tokens.is_some() || self.max_output_tokens.is_some()
    }
}

impl From<ModelCapabilities> for ModelCapabilityOverride {
    /// Every field named, for a caller that already has a complete set.
    fn from(c: ModelCapabilities) -> Self {
        Self {
            supports_temperature: Some(c.supports_temperature),
            supports_streaming: Some(c.supports_streaming),
            supports_tools: Some(c.supports_tools),
            supports_system_prompt: Some(c.supports_system_prompt),
            max_context_tokens: Some(c.max_context_tokens),
            max_output_tokens: Some(c.max_output_tokens),
            // Capabilities describe what a model can do, not what it charges,
            // so a set converted from them names no rates.
            input_per_mtok: None,
            cached_input_per_mtok: None,
            cache_write_per_mtok: None,
            output_per_mtok: None,
            // Nor what mime it takes; that is a separate answer.
            input_types: None,
            output_types: None,
        }
    }
}

#[cfg(test)]
mod mime_tests {
    use super::*;
    use leviath_core::mime::MimeType;

    fn mt(s: &str) -> MimeType {
        MimeType::parse(s).unwrap()
    }

    #[test]
    fn text_only_is_the_default_and_takes_no_mime() {
        let m = ModelMime::default();
        assert_eq!(m, ModelMime::text_only());
        assert!(m.accepts(&mt("text/markdown")));
        assert!(!m.accepts(&mt("image/png")));
        assert!(m.produces(&mt("text/plain")));
        assert!(!m.produces(&mt("image/png")));
        assert!(!m.takes_mime());
        assert!(ModelMime::new(&["text/*", "image/*"], &["text/*"]).takes_mime());
        // An image generator is known by its output, so its request is built
        // for drawing (prompt in the user turn) rather than for a text reply.
        assert!(!m.produces_images());
        assert!(ModelMime::new(&["text/*"], &["text/*", "image/*"]).produces_images());
        assert!(ModelMime::new(&["text/*", "image/*"], &["image/png"]).produces_images());
    }

    #[test]
    fn covers_compares_patterns_not_only_types() {
        let vision = ModelMime::new(&["text/*", "image/*"], &["text/*"]);
        assert!(vision.covers(&["image/png".to_string()]));
        assert!(vision.covers(&["image/*".to_string(), "text/plain".to_string()]));
        assert!(!vision.covers(&["audio/*".to_string()]));
        assert!(!vision.covers(&["application/pdf".to_string()]));
        let exact = ModelMime::new(&["image/png"], &[]);
        assert!(exact.covers(&["image/png".to_string()]));
        assert!(!exact.covers(&["image/*".to_string()]));
        let any = ModelMime::new(&["*/*"], &[]);
        assert!(any.covers(&["video/mp4".to_string(), "audio/*".to_string()]));
        assert!(any.covers(&[]));
        assert!(pattern_covers("Image/*", "image/PNG"));
        assert!(!pattern_covers("image", "image/png"));
        assert!(!pattern_covers("image/png", "image/jpeg"));
    }

    #[test]
    fn an_override_replaces_only_the_lists_it_names() {
        let base = ModelMime::new(&["text/*"], &["text/*"]);
        let none = ModelCapabilityOverride::default();
        assert_eq!(none.apply_mime(base.clone()), base);
        let input_only = ModelCapabilityOverride {
            input_types: Some(vec!["text/*".into(), "image/*".into()]),
            ..Default::default()
        };
        let merged = input_only.apply_mime(base.clone());
        assert!(merged.accepts(&mt("image/png")));
        assert_eq!(merged.output, base.output);
        let output_only = ModelCapabilityOverride {
            output_types: Some(vec!["image/*".into()]),
            ..Default::default()
        };
        let merged = output_only.apply_mime(base.clone());
        assert_eq!(merged.input, base.input);
        assert!(merged.produces(&mt("image/png")));
        let from_caps: ModelCapabilityOverride = ModelCapabilities::default().into();
        assert!(from_caps.input_types.is_none() && from_caps.output_types.is_none());
        let json = serde_json::to_string(&merged).unwrap();
        assert!(json.contains("\"output\":[\"image/*\"]"));
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;

    /// A catalogue id that fell through to its provider's fallback would be a
    /// row whose numbers are a guess dressed as a fact, which is the drift
    /// this catalogue replaced.
    #[test]
    fn every_catalogue_id_hits_a_real_row_of_its_providers_table() {
        let catalog = builtin_catalog();
        assert!(catalog.len() > 30);
        let fallbacks = [
            ("anthropic", ModelCapabilities::default()),
            ("openai", ModelCapabilities::default()),
            ("openrouter", crate::openrouter::FALLBACK_CAPABILITIES),
        ];
        let strays: Vec<&str> = catalog
            .iter()
            .filter(|e| {
                fallbacks
                    .iter()
                    .any(|(p, f)| *p == e.provider && e.capabilities == *f)
            })
            .map(|e| e.id)
            .collect();
        assert!(strays.is_empty(), "on the fallback: {strays:?}");
    }

    #[test]
    fn catalogue_ids_are_unique_per_provider_and_named() {
        let catalog = builtin_catalog();
        let mut seen = std::collections::HashSet::new();
        for entry in &catalog {
            assert!(seen.insert((entry.provider, entry.id)), "{:?}", entry.id);
            assert!(!entry.display_name.is_empty(), "{}", entry.id);
            assert!(
                entry.capabilities.max_context_tokens > entry.capabilities.max_output_tokens,
                "{}",
                entry.id
            );
        }
        for provider in ["anthropic", "openai", "google", "openrouter", "claude-code"] {
            assert!(
                catalog.iter().any(|e| e.provider == provider),
                "{provider} names nothing"
            );
        }
        assert!(
            !catalog.iter().any(|e| e.provider == "ollama"),
            "nothing compiled in can say what has been pulled"
        );
    }
}

#[cfg(test)]
mod table_tests {
    use super::*;

    /// Every provider table, with the fallback its provider uses.
    fn tables() -> Vec<(&'static str, &'static [Row], ModelCapabilities)> {
        vec![
            (
                "openai",
                crate::openai::MODELS,
                ModelCapabilities::default(),
            ),
            (
                "anthropic",
                crate::anthropic::MODELS,
                ModelCapabilities::default(),
            ),
            (
                "ollama",
                crate::ollama::MODELS,
                crate::ollama::FALLBACK_CAPABILITIES,
            ),
            (
                "openrouter",
                crate::openrouter::MODELS,
                crate::openrouter::FALLBACK_CAPABILITIES,
            ),
        ]
    }

    #[test]
    fn every_pattern_in_every_table_resolves_to_the_first_row_that_matches_it() {
        for (name, table, fallback) in tables() {
            for (i, row) in table.iter().enumerate() {
                for m in row.matches {
                    let model = m.example();
                    // A pattern always matches its own example, so the
                    // position is never absent.
                    let first = table
                        .iter()
                        .position(|r| r.hits(&model))
                        .expect("a pattern matches its own example");
                    assert!(
                        first <= i,
                        "{name}: row {i} ({model}) is shadowed by row {first}"
                    );
                    assert_eq!(
                        lookup(table, &model, fallback.clone()),
                        table[first].capabilities(),
                        "{name}: {model}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_name_no_row_recognises_gets_the_fallback() {
        for (name, table, fallback) in tables() {
            assert_eq!(
                lookup(table, "nothing-anyone-has-heard-of", fallback.clone()),
                fallback,
                "{name}"
            );
        }
    }

    #[test]
    fn the_three_pattern_kinds_match_what_they_say() {
        assert!(Match::Prefix("gpt-5").hits("gpt-5-mini"));
        assert!(!Match::Prefix("gpt-5").hits("openai/gpt-5"));
        assert!(Match::Contains("gpt-5").hits("openai/gpt-5"));
        assert!(Match::PrefixAnd("anthropic/", "fable").hits("anthropic/claude-fable-5"));
        assert!(!Match::PrefixAnd("anthropic/", "fable").hits("openai/claude-fable-5"));
        assert!(!Match::PrefixAnd("anthropic/", "fable").hits("anthropic/claude-opus-5"));
    }

    #[test]
    fn a_row_streams_and_takes_a_system_prompt_whatever_else_it_says() {
        let row = Row {
            matches: &[Match::Contains("x")],
            temperature: false,
            tools: false,
            context: 1,
            output: 2,
        };
        let caps = row.capabilities();
        assert!(caps.supports_streaming && caps.supports_system_prompt);
        assert!(!caps.supports_temperature && !caps.supports_tools);
        assert_eq!((caps.max_context_tokens, caps.max_output_tokens), (1, 2));
        assert_eq!(caps.limits_source, LimitsSource::Builtin);
    }
}
