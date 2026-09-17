//! What this build knows about Bedrock's models before asking Bedrock.
//!
//! Three sources, each honest about what it is. `bedrock/windows.toml` is
//! read from AWS's model cards by `cargo xtask bedrock-windows` and carries
//! the two token limits, because no Bedrock API states them. [`MODELS`] is
//! the small compiled table of what the cards do not say either: which
//! models refuse a temperature and which take no tools. And the listing
//! parsers below read what `ListFoundationModels` and
//! `ListInferenceProfiles` do say, which is names, modalities and which ids
//! can be called at all.
//!
//! Ids on Bedrock are `vendor.model-version` (`amazon.nova-pro-v1:0`), often
//! behind an inference-profile prefix that names where the request may be
//! routed (`us.amazon.nova-pro-v1:0`, `global.anthropic.claude-sonnet-5`).
//! Most current models can only be called through a profile, so the profile
//! id is what a blueprint names and what the listing here reports.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::Deserialize;

use crate::capabilities::{LimitsSource, Match, ModelCapabilities, ModelMime, Row, lookup};
use crate::learned::LearnedModel;

/// Which vendor's model an id names, by its first segment after any
/// profile prefix. Decides the request details that differ by vendor: cache
/// points, tool-result status, which reasoning field applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Vendor {
    /// `anthropic.claude-*`.
    Anthropic,
    /// `amazon.nova-*`.
    Nova,
    /// `meta.llama*`.
    Meta,
    /// `mistral.*`.
    Mistral,
    /// `deepseek.*`.
    DeepSeek,
    /// `openai.gpt-oss-*`.
    OpenAi,
    /// `cohere.command-*`.
    Cohere,
    /// Everyone else Bedrock carries.
    Other,
}

/// The inference-profile prefixes AWS defines: one geography or `global`.
const PROFILE_PREFIXES: &[&str] = &["us", "eu", "apac", "global", "jp", "au", "ca", "us-gov"];

/// The vendor segments Bedrock's catalogue uses, so an id can be recognised
/// as Bedrock-shaped by spelling alone.
const VENDOR_SEGMENTS: &[&str] = &[
    "anthropic",
    "amazon",
    "meta",
    "mistral",
    "deepseek",
    "openai",
    "cohere",
    "ai21",
    "qwen",
    "google",
    "minimax",
    "moonshotai",
    "nvidia",
    "writer",
    "xai",
    "zai",
    "stability",
    "twelvelabs",
];

/// `model` without its inference-profile prefix, if it has one.
///
/// `us.anthropic.claude-sonnet-5` and `anthropic.claude-sonnet-5` are the
/// same model reached two ways; the card, the price file and Anthropic's
/// own routes all know it by the bare form.
pub(crate) fn bare_id(model: &str) -> &str {
    match model.split_once('.') {
        Some((prefix, rest)) if PROFILE_PREFIXES.contains(&prefix) => rest,
        _ => model,
    }
}

/// The vendor segment of a bare id, if it has one.
fn vendor_segment(model: &str) -> Option<&str> {
    bare_id(model).split_once('.').map(|(vendor, _)| vendor)
}

/// Which vendor `model` belongs to.
pub(super) fn vendor_of(model: &str) -> Vendor {
    match vendor_segment(model) {
        Some("anthropic") => Vendor::Anthropic,
        Some("amazon") => Vendor::Nova,
        Some("meta") => Vendor::Meta,
        Some("mistral") => Vendor::Mistral,
        Some("deepseek") => Vendor::DeepSeek,
        Some("openai") => Vendor::OpenAi,
        Some("cohere") => Vendor::Cohere,
        _ => Vendor::Other,
    }
}

/// Whether `model` is spelled the way Bedrock spells a model: a known vendor
/// segment, a dot, a model name, with or without a profile prefix; or a
/// Bedrock ARN.
///
/// This is what lets `bedrock/…` be left off a model key that could not be
/// anything else, and what keeps a bare `claude-sonnet-5` routed to the
/// Anthropic provider: the same model on Bedrock bills a different account.
pub(crate) fn is_bedrock_id(model: &str) -> bool {
    model.starts_with("arn:aws:bedrock:")
        || vendor_segment(model).is_some_and(|vendor| VENDOR_SEGMENTS.contains(&vendor))
}

/// The id after the vendor segment: `claude-sonnet-5` for
/// `us.anthropic.claude-sonnet-5`. What the vendor's own tables key on.
pub(super) fn vendor_model(model: &str) -> &str {
    bare_id(model)
        .split_once('.')
        .map_or(model, |(_, rest)| rest)
}

/// The rows of `bedrock/windows.toml`, parsed once.
///
/// A parse failure is a panic on first use rather than an error: the file is
/// compiled in, so a malformed one is a build of this crate that cannot size
/// any Bedrock model, and the tests below catch it before it ships.
static WINDOWS: LazyLock<WindowTable> = LazyLock::new(|| {
    toml::from_str(include_str!("../../bedrock/windows.toml"))
        .expect("bedrock/windows.toml is well-formed; `cargo xtask bedrock-windows` writes it")
});

/// The shape of `bedrock/windows.toml`.
#[derive(Debug, Deserialize)]
struct WindowTable {
    /// The day the rows were last refreshed, `YYYY-MM-DD`.
    read_on: String,
    /// Every row, in file order.
    #[serde(default)]
    model: Vec<WindowRow>,
}

/// One model as its AWS card describes it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WindowRow {
    /// The bare model id, without any profile prefix.
    pub id: String,
    /// What AWS calls it.
    pub name: String,
    /// The context window, in tokens.
    pub context: usize,
    /// The most it can say in one reply, in tokens.
    pub output: usize,
    /// Whether the card has a Reasoning line.
    #[serde(default)]
    pub reasoning: bool,
    /// Whether the card lists CountTokens as supported on `bedrock-runtime`.
    #[serde(default)]
    pub count_tokens: bool,
    /// The geo and global inference-profile ids the card names.
    #[serde(default)]
    pub profiles: Vec<String>,
    /// `aws-model-card` for a row the refresh wrote, `manual` for one a
    /// person wrote.
    pub source: String,
}

/// The day the limits in `windows.toml` were last read from AWS.
pub fn windows_read_on() -> &'static str {
    &WINDOWS.read_on
}

/// The card row for `model`: its bare id exactly, else the longest row id
/// the bare id starts with (a dated snapshot of a family the card lists
/// once).
pub fn window_for(model: &str) -> Option<&'static WindowRow> {
    let bare = bare_id(model);
    WINDOWS.model.iter().find(|row| row.id == bare).or_else(|| {
        WINDOWS
            .model
            .iter()
            .filter(|row| bare.starts_with(row.id.as_str()))
            .max_by_key(|row| row.id.len())
    })
}

/// The models this build names when the listing cannot be read: one id per
/// card row, the first profile the card names or the bare id when it names
/// none, with AWS's display name.
///
/// Leaked once so it has the `'static` shape every other provider's compiled
/// catalogue has; the table is read from the binary and lives as long as it.
pub(crate) static CATALOG: LazyLock<Vec<(&'static str, &'static str)>> = LazyLock::new(|| {
    WINDOWS
        .model
        .iter()
        .map(|row| {
            let id = row.profiles.first().unwrap_or(&row.id);
            (
                &*Box::leak(id.clone().into_boxed_str()),
                &*Box::leak(row.name.clone().into_boxed_str()),
            )
        })
        .collect()
});

/// What a model no table recognises is assumed to do: text with tools and a
/// temperature, and a window small enough to be wrong in the safe direction.
pub(crate) const FALLBACK_CAPABILITIES: ModelCapabilities = ModelCapabilities {
    supports_temperature: true,
    supports_streaming: true,
    supports_tools: true,
    supports_system_prompt: true,
    max_context_tokens: 32_000,
    max_output_tokens: 4_096,
    limits_source: LimitsSource::Builtin,
};

/// What the cards do not say: which models refuse a temperature and which
/// take no tools. Most specific first. The limits here are the fallback for
/// a model `windows.toml` has no row for; a row there wins.
pub(crate) const MODELS: &[Row] = &[
    // The Claude 5 line and the newest Opus 4.x take no temperature, as the
    // Anthropic table says of the same models.
    Row {
        matches: &[
            Match::Contains("anthropic.claude-opus-5"),
            Match::Contains("anthropic.claude-sonnet-5"),
            Match::Contains("anthropic.claude-fable-5"),
            Match::Contains("anthropic.claude-mythos-5"),
            Match::Contains("anthropic.claude-opus-4-8"),
            Match::Contains("anthropic.claude-opus-4-7"),
        ],
        temperature: false,
        tools: true,
        context: 1_000_000,
        output: 128_000,
    },
    Row {
        matches: &[Match::Contains("anthropic.claude-")],
        temperature: true,
        tools: true,
        context: 200_000,
        output: 64_000,
    },
    Row {
        matches: &[
            Match::Contains("amazon.nova-premier"),
            Match::Contains("amazon.nova-2-"),
        ],
        temperature: true,
        tools: true,
        context: 1_000_000,
        output: 32_000,
    },
    Row {
        matches: &[
            Match::Contains("amazon.nova-pro"),
            Match::Contains("amazon.nova-lite"),
        ],
        temperature: true,
        tools: true,
        context: 300_000,
        output: 5_000,
    },
    Row {
        matches: &[Match::Contains("amazon.nova-micro")],
        temperature: true,
        tools: true,
        context: 128_000,
        output: 5_000,
    },
    Row {
        matches: &[Match::Contains("meta.llama4-scout")],
        temperature: true,
        tools: true,
        context: 3_500_000,
        output: 8_000,
    },
    Row {
        matches: &[Match::Contains("meta.llama4-maverick")],
        temperature: true,
        tools: true,
        context: 1_000_000,
        output: 8_000,
    },
    Row {
        matches: &[Match::Contains("meta.llama")],
        temperature: true,
        tools: true,
        context: 128_000,
        output: 8_000,
    },
    Row {
        matches: &[Match::Contains("mistral.")],
        temperature: true,
        tools: true,
        context: 128_000,
        output: 8_000,
    },
    // DeepSeek-R1 on Bedrock takes no tool definitions.
    Row {
        matches: &[Match::Contains("deepseek.r1")],
        temperature: true,
        tools: false,
        context: 128_000,
        output: 32_000,
    },
    Row {
        matches: &[Match::Contains("deepseek.")],
        temperature: true,
        tools: true,
        context: 164_000,
        output: 8_000,
    },
    Row {
        matches: &[Match::Contains("openai.gpt-oss")],
        temperature: true,
        tools: true,
        context: 128_000,
        output: 16_000,
    },
    Row {
        matches: &[Match::Contains("cohere.command")],
        temperature: true,
        tools: true,
        context: 128_000,
        output: 4_000,
    },
];

/// What this build says about `model`: the flags from [`MODELS`] and the
/// limits from the card, for a caller with no provider in hand.
pub(crate) fn table_capabilities(model: &str) -> ModelCapabilities {
    let flags = lookup(MODELS, model, FALLBACK_CAPABILITIES);
    match window_for(model) {
        Some(row) => ModelCapabilities {
            max_context_tokens: row.context,
            max_output_tokens: row.output,
            ..flags
        },
        None => flags,
    }
}

/// Text, images, video and PDFs: what the Nova models take.
const NOVA_INPUT: &[&str] = &["text/*", "image/*", "video/*", "application/pdf"];

/// Text and images.
const VISION: &[&str] = &["text/*", "image/*"];

/// What `model` takes and produces, by vendor and name, for a caller with
/// no listing in hand. The listing's `inputModalities` corrects it.
pub(crate) fn mime_for(model: &str) -> ModelMime {
    let name = vendor_model(model).to_ascii_lowercase();
    if super::media::is_image_model(model) {
        return ModelMime::new(VISION, &["image/*"]);
    }
    match vendor_of(model) {
        Vendor::Anthropic => crate::mime_tables::anthropic(&name),
        Vendor::Nova if name.contains("nova-micro") => ModelMime::text_only(),
        Vendor::Nova => ModelMime::new(NOVA_INPUT, crate::mime_tables::TEXT),
        Vendor::Meta
            if name.contains("llama4")
                || name.contains("llama3-2-11b")
                || name.contains("llama3-2-90b") =>
        {
            ModelMime::new(VISION, crate::mime_tables::TEXT)
        }
        Vendor::Mistral if name.contains("pixtral") => {
            ModelMime::new(VISION, crate::mime_tables::TEXT)
        }
        _ => ModelMime::text_only(),
    }
}

/// One entry of `ListFoundationModels`, as much of it as matters here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FoundationModel {
    /// The bare model id.
    pub(super) id: String,
    /// What AWS calls it.
    pub(super) name: Option<String>,
    /// Mime patterns for its `inputModalities`.
    pub(super) input_types: Vec<String>,
    /// Mime patterns for its `outputModalities`: text, or images.
    pub(super) output_types: Vec<String>,
    /// Whether the bare id can be called as it is. A model that is
    /// `INFERENCE_PROFILE` only has to be reached through a profile id.
    pub(super) on_demand: bool,
}

/// A `modelSummaries` entry as a [`FoundationModel`], or `None` for one that
/// does not answer in text or is no longer active.
pub(super) fn parse_foundation_model(entry: &serde_json::Value) -> Option<FoundationModel> {
    let id = entry.get("modelId").and_then(|v| v.as_str())?.to_string();
    let words = |key: &str| -> Vec<String> {
        entry
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|w| w.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    // Text through Converse, or images through InvokeModel for the image
    // models this provider runs; embeddings and video are neither.
    let output_types: Vec<String> = words("outputModalities")
        .iter()
        .filter_map(|w| match w.as_str() {
            "TEXT" => Some("text/*"),
            "IMAGE" if super::media::is_image_model(&id) => Some("image/*"),
            _ => None,
        })
        .map(str::to_string)
        .collect();
    if output_types.is_empty() {
        return None;
    }
    if entry
        .pointer("/modelLifecycle/status")
        .and_then(|v| v.as_str())
        .is_some_and(|status| status != "ACTIVE")
    {
        return None;
    }
    let mut input_types: Vec<String> = words("inputModalities")
        .iter()
        .filter_map(|w| crate::mime_tables::modality_pattern(w))
        .map(str::to_string)
        .collect();
    // Converse takes a `document` block for Claude and Nova, which the
    // listing's modality words do not mention.
    if matches!(vendor_of(&id), Vendor::Anthropic | Vendor::Nova)
        && !input_types.iter().any(|t| t == "application/pdf")
    {
        input_types.push("application/pdf".to_string());
    }
    Some(FoundationModel {
        id,
        name: entry
            .get("modelName")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        input_types,
        output_types,
        on_demand: words("inferenceTypesSupported")
            .iter()
            .any(|w| w == "ON_DEMAND"),
    })
}

/// One entry of `ListInferenceProfiles`, as much of it as matters here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InferenceProfile {
    /// The profile id a request names.
    pub(super) id: String,
    /// The bare id of the model it routes to, when the ARN says.
    pub(super) model_id: Option<String>,
    /// What AWS calls it.
    pub(super) name: Option<String>,
}

/// An `inferenceProfileSummaries` entry as an [`InferenceProfile`], or
/// `None` for one that is not active.
pub(super) fn parse_inference_profile(entry: &serde_json::Value) -> Option<InferenceProfile> {
    let id = entry
        .get("inferenceProfileId")
        .and_then(|v| v.as_str())?
        .to_string();
    if entry
        .get("status")
        .and_then(|v| v.as_str())
        .is_some_and(|status| status != "ACTIVE")
    {
        return None;
    }
    let model_id = entry
        .pointer("/models/0/modelArn")
        .and_then(|v| v.as_str())
        .and_then(|arn| arn.split_once("foundation-model/"))
        .map(|(_, model)| model.to_string());
    Some(InferenceProfile {
        id,
        model_id,
        name: entry
            .get("inferenceProfileName")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// The listing as one record per callable id: every on-demand bare id and
/// every profile id, the profile carrying its model's name and modalities.
///
/// Limits stay `None`: the listing carries none, and a record that named a
/// limit would relabel the card's figure as read from the API.
pub(super) fn merge_listing(
    models: &[FoundationModel],
    profiles: &[InferenceProfile],
) -> HashMap<String, LearnedModel> {
    let record = |name: Option<String>, model: Option<&FoundationModel>| LearnedModel {
        display_name: name,
        max_context_tokens: None,
        max_output_tokens: None,
        supports_temperature: None,
        supports_tools: None,
        explicit_cache_control: None,
        pricing: None,
        released: None,
        retires: None,
        input_types: model.map(|m| m.input_types.clone()),
        output_types: Some(
            model.map_or_else(|| vec!["text/*".to_string()], |m| m.output_types.clone()),
        ),
    };
    let mut learned = HashMap::new();
    for model in models.iter().filter(|m| m.on_demand) {
        learned.insert(model.id.clone(), record(model.name.clone(), Some(model)));
    }
    for profile in profiles {
        let model = profile
            .model_id
            .as_deref()
            .and_then(|id| models.iter().find(|m| m.id == id));
        learned.insert(
            profile.id.clone(),
            record(
                model
                    .and_then(|m| m.name.clone())
                    .or_else(|| profile.name.clone()),
                model,
            ),
        );
    }
    learned
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_profile_prefix_is_stripped_and_a_bare_id_is_left_alone() {
        assert_eq!(
            bare_id("us.anthropic.claude-sonnet-5"),
            "anthropic.claude-sonnet-5"
        );
        assert_eq!(
            bare_id("global.amazon.nova-pro-v1:0"),
            "amazon.nova-pro-v1:0"
        );
        assert_eq!(bare_id("us-gov.anthropic.claude-x"), "anthropic.claude-x");
        assert_eq!(
            bare_id("anthropic.claude-sonnet-5"),
            "anthropic.claude-sonnet-5"
        );
        assert_eq!(bare_id("claude-sonnet-5"), "claude-sonnet-5");
        assert_eq!(
            vendor_model("us.anthropic.claude-sonnet-5"),
            "claude-sonnet-5"
        );
        assert_eq!(vendor_model("openai.gpt-oss-120b-1:0"), "gpt-oss-120b-1:0");
        assert_eq!(vendor_model("plain"), "plain");
    }

    #[test]
    fn every_vendor_is_recognised_by_its_segment() {
        assert_eq!(vendor_of("us.anthropic.claude-sonnet-5"), Vendor::Anthropic);
        assert_eq!(vendor_of("amazon.nova-pro-v1:0"), Vendor::Nova);
        assert_eq!(
            vendor_of("meta.llama4-scout-17b-instruct-v1:0"),
            Vendor::Meta
        );
        assert_eq!(vendor_of("mistral.mistral-large-3"), Vendor::Mistral);
        assert_eq!(vendor_of("us.deepseek.r1-v1:0"), Vendor::DeepSeek);
        assert_eq!(vendor_of("openai.gpt-oss-120b-1:0"), Vendor::OpenAi);
        assert_eq!(vendor_of("cohere.command-r-plus-v1:0"), Vendor::Cohere);
        assert_eq!(vendor_of("qwen.qwen3-32b-v1:0"), Vendor::Other);
        assert_eq!(vendor_of("claude-sonnet-5"), Vendor::Other);
    }

    #[test]
    fn a_bedrock_id_is_known_by_its_vendor_segment_or_arn() {
        assert!(is_bedrock_id("anthropic.claude-sonnet-5"));
        assert!(is_bedrock_id("us.anthropic.claude-sonnet-5"));
        assert!(is_bedrock_id("qwen.qwen3-32b-v1:0"));
        assert!(is_bedrock_id(
            "arn:aws:bedrock:us-east-1:123:inference-profile/x"
        ));
        assert!(!is_bedrock_id("claude-sonnet-5"));
        assert!(!is_bedrock_id("gpt-5.5"));
        assert!(!is_bedrock_id("nobody.model"));
    }

    #[test]
    fn the_windows_table_parses_and_every_row_is_complete() {
        assert!(!windows_read_on().is_empty());
        assert!(!WINDOWS.model.is_empty());
        for row in &WINDOWS.model {
            assert!(row.context > 0 && row.output > 0, "{}", row.id);
            assert!(!row.name.is_empty(), "{}", row.id);
            assert!(!row.id.contains('/'), "{}", row.id);
            assert_eq!(bare_id(&row.id), row.id, "a row id carries no prefix");
            for profile in &row.profiles {
                // A card can name the mantle id (`anthropic.claude-haiku-4-5`)
                // beside dated runtime profiles, so a profile starts with the
                // row id rather than equalling it.
                assert!(
                    bare_id(profile).starts_with(&row.id),
                    "{profile} is a profile of {}",
                    row.id
                );
            }
        }
    }

    #[test]
    fn every_catalog_id_resolves_to_a_card_row() {
        assert!(!CATALOG.is_empty());
        for (id, name) in CATALOG.iter() {
            let row = window_for(id).expect(id);
            assert_eq!(&row.name, name);
        }
    }

    #[test]
    fn the_claude_5_rows_carry_the_million_token_window() {
        let row = window_for("global.anthropic.claude-sonnet-5").unwrap();
        assert_eq!((row.context, row.output), (1_000_000, 128_000));
        assert!(row.reasoning);
        let caps = table_capabilities("us.anthropic.claude-sonnet-5");
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
        assert!(!caps.supports_temperature);
        assert!(caps.supports_tools);
    }

    #[test]
    fn a_window_row_is_found_exactly_or_by_the_longest_prefix() {
        assert_eq!(
            window_for("amazon.nova-pro-v1:0").map(|r| r.output),
            Some(5_000)
        );
        // A dated snapshot of a listed family.
        assert!(window_for("anthropic.claude-sonnet-5-20260630").is_some());
        assert_eq!(window_for("nobody.model"), None);
    }

    #[test]
    fn a_model_without_a_card_row_keeps_the_table_limits_or_the_fallback() {
        let caps = table_capabilities("us.deepseek.r1-future");
        assert!(!caps.supports_tools);
        assert_eq!(caps.max_output_tokens, 32_000);
        let caps = table_capabilities("nobody.model");
        assert_eq!(caps, FALLBACK_CAPABILITIES);
    }

    #[test]
    fn mime_follows_the_vendor_and_the_name() {
        assert!(mime_for("us.anthropic.claude-sonnet-5").takes_mime());
        assert!(
            mime_for("us.amazon.nova-pro-v1:0")
                .input
                .contains(&"video/*".to_string())
        );
        assert_eq!(mime_for("amazon.nova-micro-v1:0"), ModelMime::text_only());
        assert!(mime_for("us.meta.llama4-maverick-17b-instruct-v1:0").takes_mime());
        assert!(mime_for("us.meta.llama3-2-90b-instruct-v1:0").takes_mime());
        assert_eq!(
            mime_for("meta.llama3-3-70b-instruct-v1:0"),
            ModelMime::text_only()
        );
        assert!(mime_for("us.mistral.pixtral-large-2502-v1:0").takes_mime());
        assert_eq!(mime_for("mistral.mistral-large-3"), ModelMime::text_only());
        assert_eq!(mime_for("openai.gpt-oss-120b-1:0"), ModelMime::text_only());
    }

    #[test]
    fn a_foundation_model_entry_is_read_with_its_modalities() {
        let entry = json!({
            "modelId": "amazon.nova-pro-v1:0",
            "modelName": "Nova Pro",
            "inputModalities": ["TEXT", "IMAGE", "VIDEO", "WHATEVER"],
            "outputModalities": ["TEXT"],
            "inferenceTypesSupported": ["ON_DEMAND", "INFERENCE_PROFILE"],
            "modelLifecycle": { "status": "ACTIVE" }
        });
        let model = parse_foundation_model(&entry).unwrap();
        assert_eq!(model.id, "amazon.nova-pro-v1:0");
        assert_eq!(model.name.as_deref(), Some("Nova Pro"));
        assert_eq!(
            model.input_types,
            vec!["text/*", "image/*", "video/*", "application/pdf"]
        );
        assert!(model.on_demand);
        assert_eq!(model.output_types, vec!["text/*"]);

        // An image model this provider runs is kept, answering in images; an
        // image output it does not run is not.
        let canvas = parse_foundation_model(&json!({
            "modelId": "amazon.nova-canvas-v1:0",
            "inputModalities": ["TEXT", "IMAGE"],
            "outputModalities": ["IMAGE"],
            "inferenceTypesSupported": ["ON_DEMAND"]
        }))
        .unwrap();
        assert_eq!(canvas.output_types, vec!["image/*"]);
        assert!(
            parse_foundation_model(&json!({
                "modelId": "amazon.nova-reel-v1:0",
                "outputModalities": ["VIDEO", "IMAGE"]
            }))
            .is_none()
        );
    }

    #[test]
    fn a_profile_only_or_inactive_or_non_text_model_is_marked_or_dropped() {
        let profile_only = parse_foundation_model(&json!({
            "modelId": "anthropic.claude-sonnet-5",
            "inputModalities": ["TEXT"],
            "outputModalities": ["TEXT"],
            "inferenceTypesSupported": ["INFERENCE_PROFILE"]
        }))
        .unwrap();
        assert!(!profile_only.on_demand);
        assert!(profile_only.name.is_none());
        assert_eq!(profile_only.input_types, vec!["text/*", "application/pdf"]);
        assert_eq!(
            parse_foundation_model(&json!({
                "modelId": "amazon.titan-embed-text-v2:0",
                "outputModalities": ["EMBEDDING"]
            })),
            None
        );
        assert_eq!(
            parse_foundation_model(&json!({
                "modelId": "anthropic.claude-v2",
                "outputModalities": ["TEXT"],
                "modelLifecycle": { "status": "LEGACY" }
            })),
            None
        );
        assert_eq!(parse_foundation_model(&json!({ "modelName": "x" })), None);
        // A non-Anthropic, non-Nova model gets no document type added.
        let llama = parse_foundation_model(&json!({
            "modelId": "meta.llama3-3-70b-instruct-v1:0",
            "inputModalities": ["TEXT"],
            "outputModalities": ["TEXT"]
        }))
        .unwrap();
        assert_eq!(llama.input_types, vec!["text/*"]);
    }

    #[test]
    fn an_inference_profile_entry_names_its_model() {
        let entry = json!({
            "inferenceProfileId": "us.amazon.nova-pro-v1:0",
            "inferenceProfileName": "US Nova Pro",
            "status": "ACTIVE",
            "models": [{ "modelArn": "arn:aws:bedrock:us-east-1::foundation-model/amazon.nova-pro-v1:0" }]
        });
        let profile = parse_inference_profile(&entry).unwrap();
        assert_eq!(profile.id, "us.amazon.nova-pro-v1:0");
        assert_eq!(profile.model_id.as_deref(), Some("amazon.nova-pro-v1:0"));
        assert_eq!(profile.name.as_deref(), Some("US Nova Pro"));
        assert_eq!(
            parse_inference_profile(&json!({ "inferenceProfileId": "x", "status": "INACTIVE" })),
            None
        );
        assert_eq!(
            parse_inference_profile(&json!({ "status": "ACTIVE" })),
            None
        );
        let odd = parse_inference_profile(&json!({
            "inferenceProfileId": "y",
            "models": [{ "modelArn": "arn:aws:bedrock:us-east-1:123:custom-model/z" }]
        }))
        .unwrap();
        assert_eq!(odd.model_id, None);
        assert_eq!(odd.name, None);
    }

    #[test]
    fn the_merged_listing_keeps_callable_ids_only() {
        let models = vec![
            FoundationModel {
                id: "amazon.nova-pro-v1:0".to_string(),
                name: Some("Nova Pro".to_string()),
                input_types: vec!["text/*".to_string(), "image/*".to_string()],
                output_types: vec!["text/*".to_string()],
                on_demand: true,
            },
            FoundationModel {
                id: "anthropic.claude-sonnet-5".to_string(),
                name: Some("Claude Sonnet 5".to_string()),
                input_types: vec!["text/*".to_string()],
                output_types: vec!["text/*".to_string()],
                on_demand: false,
            },
            FoundationModel {
                id: "nameless.model-v1:0".to_string(),
                name: None,
                input_types: vec!["text/*".to_string()],
                output_types: vec!["text/*".to_string()],
                on_demand: false,
            },
        ];
        let profiles = vec![
            InferenceProfile {
                id: "us.anthropic.claude-sonnet-5".to_string(),
                model_id: Some("anthropic.claude-sonnet-5".to_string()),
                name: Some("US Claude Sonnet 5".to_string()),
            },
            InferenceProfile {
                id: "us.mystery.model-v1:0".to_string(),
                model_id: None,
                name: Some("US Mystery".to_string()),
            },
            InferenceProfile {
                id: "eu.unnamed.model-v1:0".to_string(),
                model_id: Some("unnamed.model-v1:0".to_string()),
                name: None,
            },
            InferenceProfile {
                id: "us.nameless.model-v1:0".to_string(),
                model_id: Some("nameless.model-v1:0".to_string()),
                name: Some("US Nameless".to_string()),
            },
        ];
        let learned = merge_listing(&models, &profiles);
        assert!(learned.contains_key("amazon.nova-pro-v1:0"));
        assert!(!learned.contains_key("anthropic.claude-sonnet-5"));
        let sonnet = &learned["us.anthropic.claude-sonnet-5"];
        assert_eq!(sonnet.display_name.as_deref(), Some("Claude Sonnet 5"));
        assert_eq!(sonnet.input_types, Some(vec!["text/*".to_string()]));
        assert_eq!(sonnet.max_context_tokens, None);
        assert_eq!(sonnet.output_types, Some(vec!["text/*".to_string()]));
        let mystery = &learned["us.mystery.model-v1:0"];
        assert_eq!(mystery.display_name.as_deref(), Some("US Mystery"));
        assert_eq!(mystery.input_types, None);
        assert_eq!(learned["eu.unnamed.model-v1:0"].display_name, None);
        assert_eq!(
            learned["us.nameless.model-v1:0"].display_name.as_deref(),
            Some("US Nameless")
        );
        assert_eq!(learned.len(), 5);
    }
}
