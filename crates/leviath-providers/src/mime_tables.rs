//! What the built-in vendors' models take and produce, by name.
//!
//! These are the opinionated part: a vendor's API has one way to send an
//! image and one list of models that accept one, and both are published,
//! not discoverable. Everything else in the engine asks the registry and
//! these answers rather than a type name. A listing that says more (as
//! OpenRouter's does) corrects a row here, and an operator's
//! `[model_capabilities]` entry corrects both.
//!
//! Two layers answer for a vendor whose API stays quiet. The precise one is
//! `mime/modalities.toml`, a table refreshed from OpenRouter's catalogue by
//! `cargo xtask modalities`, so a specific model's real lists (`claude-3-haiku`
//! takes images but not PDFs, say) are used when the catalogue names it. Under
//! it is the name heuristic below, the floor for a model the catalogue does not
//! carry - a model published nowhere, or one shaped so differently (a
//! text-to-speech or image model) that its name is the only signal.

use std::sync::LazyLock;

use serde::Deserialize;

use crate::capabilities::ModelMime;

/// Text in, text out.
pub(crate) const TEXT: &[&str] = &["text/*"];

/// Text, images and PDFs in.
pub(crate) const VISION_DOC: &[&str] = &["text/*", "image/*", "application/pdf"];

/// Everything Gemini's chat models take.
pub(crate) const GEMINI_INPUT: &[&str] =
    &["text/*", "image/*", "audio/*", "video/*", "application/pdf"];

/// A lowercase copy for matching.
fn lower(model: &str) -> String {
    model.to_ascii_lowercase()
}

/// Every Claude model reads images and PDFs and writes text, unless the
/// catalogue names one that takes less (`claude-3-haiku` has no PDF).
pub(crate) fn anthropic(model: &str) -> ModelMime {
    published_modality("anthropic", model).unwrap_or_else(|| ModelMime::new(VISION_DOC, TEXT))
}

/// OpenAI: the chat and reasoning models read images and PDFs; the audio
/// models also take and return audio; the image models return images.
///
/// The name decides the structurally distinct classes - image, audio, speech,
/// transcription - because the catalogue does not serve them and their name is
/// the only signal. A chat or reasoning model takes the catalogue's published
/// lists when it names this one, and the vision-family default otherwise.
pub(crate) fn openai(model: &str) -> ModelMime {
    let m = lower(model);
    if m.starts_with("gpt-image") || m.starts_with("dall-e") {
        return ModelMime::new(&["text/*", "image/*"], &["image/*"]);
    }
    if m.contains("audio") || m.contains("realtime") {
        return ModelMime::new(&["text/*", "audio/*"], &["text/*", "audio/*"]);
    }
    if m.contains("tts") {
        return ModelMime::new(TEXT, &["audio/*"]);
    }
    if m.contains("transcribe") || m.starts_with("whisper") {
        return ModelMime::new(&["audio/*"], TEXT);
    }
    if let Some(mime) = published_modality("openai", model) {
        return mime;
    }
    let vision = ["gpt-4.1", "gpt-4o", "gpt-5", "o3", "o4", "o1", "chatgpt"];
    if vision.iter().any(|p| m.starts_with(p)) {
        return ModelMime::new(VISION_DOC, TEXT);
    }
    ModelMime::text_only()
}

/// Gemini: the chat models take text, images, audio, video and PDFs; the
/// image models return images; Imagen and Veo are generators.
///
/// As with OpenAI, the name settles the generator and speech models, and the
/// catalogue's lists refine a chat model the refresh has named.
pub(crate) fn gemini(model: &str) -> ModelMime {
    let m = lower(model);
    if m.starts_with("imagen") {
        return ModelMime::new(TEXT, &["image/*"]);
    }
    if m.starts_with("veo") {
        return ModelMime::new(&["text/*", "image/*"], &["video/*"]);
    }
    if m.contains("-image") {
        return ModelMime::new(&["text/*", "image/*"], &["text/*", "image/*"]);
    }
    if m.contains("-tts") {
        return ModelMime::new(TEXT, &["audio/*"]);
    }
    if m.contains("embedding") {
        return ModelMime::text_only();
    }
    published_modality("google", model).unwrap_or_else(|| ModelMime::new(GEMINI_INPUT, TEXT))
}

/// Codex: the Responses API takes images beside text.
pub(crate) fn codex(_model: &str) -> ModelMime {
    ModelMime::new(&["text/*", "image/*"], TEXT)
}

/// A local model, by the names the vision builds are published under.
pub(crate) fn ollama(model: &str) -> ModelMime {
    let m = lower(model);
    let vision = [
        "llava",
        "vision",
        "-vl",
        "minicpm-v",
        "moondream",
        "bakllava",
        "gemma3",
        "granite3.2-vision",
        "qwen2.5vl",
    ];
    if vision.iter().any(|p| m.contains(p)) {
        return ModelMime::new(&["text/*", "image/*"], TEXT);
    }
    ModelMime::text_only()
}

/// A gateway id with a vendor prefix, answered by that vendor's table.
pub(crate) fn by_prefix(model: &str) -> ModelMime {
    let m = lower(model);
    match m.split_once('/') {
        Some(("anthropic", rest)) => anthropic(rest),
        Some(("openai", rest)) => openai(rest),
        Some(("google", rest)) => gemini(rest),
        _ => ModelMime::text_only(),
    }
}

/// What this build's table says a model on `provider` takes and produces,
/// for a caller with no provider instance in hand (a listing compiled from
/// the tables). The provider's own [`crate::Provider::mime`] is the answer
/// to prefer when an instance exists, since it also reads the listing and the
/// operator's overrides.
pub fn builtin_mime(provider: &str, model: &str) -> ModelMime {
    match provider {
        "anthropic" => anthropic(model),
        "openai" => openai(model),
        "google" | "gemini" => gemini(model),
        "codex" => codex(model),
        "ollama" => ollama(model),
        "openrouter" => by_prefix(model),
        _ => ModelMime::text_only(),
    }
}

/// OpenRouter's `architecture.input_modalities` words as patterns.
pub(crate) fn modality_pattern(word: &str) -> Option<&'static str> {
    match word.trim().to_ascii_lowercase().as_str() {
        "text" => Some("text/*"),
        "image" => Some("image/*"),
        "audio" => Some("audio/*"),
        "video" => Some("video/*"),
        "file" => Some("application/pdf"),
        _ => None,
    }
}

/// The rows of `mime/modalities.toml`, parsed once.
///
/// A parse failure is a panic on first use rather than an error: the file is
/// compiled in, so a malformed one is a build of this crate that cannot type
/// any model, and the tests below catch it before it ships.
static MODALITY_TABLE: LazyLock<ModalityTable> = LazyLock::new(|| {
    toml::from_str(include_str!("../mime/modalities.toml"))
        .expect("mime/modalities.toml is well-formed; `cargo xtask modalities` writes it")
});

/// The shape of `mime/modalities.toml`.
#[derive(Debug, Deserialize)]
struct ModalityTable {
    /// The day the rows were last refreshed, `YYYY-MM-DD`.
    read_on: String,
    /// Every row, in file order.
    #[serde(default)]
    modality: Vec<PublishedModality>,
}

/// One row of the shipped modality table: what a family of models takes and
/// produces, and where the lists came from.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PublishedModality {
    /// The provider the row applies to: `anthropic`, `openai` or `google`.
    pub provider: String,
    /// The model-id prefix the row covers; the longest matching prefix wins.
    pub prefix: String,
    /// Mime patterns the model accepts in a request.
    pub input: Vec<String>,
    /// Mime patterns the model can hand back.
    pub output: Vec<String>,
    /// Where the row came from: `openrouter` for a row the refresh wrote,
    /// `manual` for a row a person wrote, which `cargo xtask modalities` never
    /// overwrites.
    pub source: String,
}

/// The day the modality rows in the shipped table were last refreshed.
///
/// Shipped with the lists so staleness is visible: a build months old may be
/// naming an older model's modalities, and the honest thing is to say when.
pub fn modalities_read_on() -> &'static str {
    &MODALITY_TABLE.read_on
}

/// The published modalities for `model` at `provider`, or `None` when no row's
/// prefix matches and the name heuristic answers instead. The longest matching
/// prefix wins, so `gpt-5.5` does not swallow a `gpt-5.5-turbo` row.
pub(crate) fn published_modality(provider: &str, model: &str) -> Option<ModelMime> {
    MODALITY_TABLE
        .modality
        .iter()
        .filter(|row| row.provider == provider && model.starts_with(&row.prefix))
        .max_by_key(|row| row.prefix.len())
        .map(|row| ModelMime {
            input: row.input.clone(),
            output: row.output.clone(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::mime::MimeType;

    fn mt(s: &str) -> MimeType {
        MimeType::parse(s).unwrap()
    }

    #[test]
    fn anthropic_reads_images_and_pdfs() {
        let m = anthropic("claude-sonnet-5");
        assert!(m.accepts(&mt("image/png")));
        assert!(m.accepts(&mt("application/pdf")));
        assert!(!m.accepts(&mt("audio/wav")));
        assert!(m.produces(&mt("text/plain")));
        assert!(!m.produces(&mt("image/png")));
    }

    #[test]
    fn the_catalogue_table_refines_the_name_heuristic() {
        // The catalogue names claude-3-haiku with a narrower list than the
        // family default: images, but no PDF. The precise row wins.
        let haiku = anthropic("claude-3-haiku");
        assert!(haiku.accepts(&mt("image/png")));
        assert!(
            !haiku.accepts(&mt("application/pdf")),
            "the table drops the PDF the family default would add"
        );

        // A model no row names falls to the family default, PDF included.
        let unlisted = anthropic("claude-nonexistent-zzz");
        assert!(unlisted.accepts(&mt("application/pdf")));

        // The same fallback for the two vendors whose chat models take more:
        // a vision-family OpenAI model the catalogue does not name still gets
        // the vision default, an old completion model falls to text only, and
        // an unnamed Gemini chat model gets the full Gemini input set.
        assert!(openai("chatgpt-4o-latest").accepts(&mt("image/png")));
        assert!(!openai("babbage-002").accepts(&mt("image/png")));
        assert!(gemini("gemini-9.9-ultra-zzz").accepts(&mt("video/mp4")));

        // The lookup is provider-scoped, and the shipped date reads back.
        assert!(published_modality("openai", "gpt-3.5-turbo").is_some());
        assert!(published_modality("openai", "totally-unknown-zzz").is_none());
        assert!(published_modality("nonprovider", "gpt-5.5").is_none());
        assert_eq!(modalities_read_on().len(), "YYYY-MM-DD".len());

        // Every shipped row is well-formed: real patterns, and a source the
        // refresh wrote (a hand-added `manual` row would say so, but the
        // shipped file carries none, so this pins that).
        for row in &MODALITY_TABLE.modality {
            assert!(!row.input.is_empty() && !row.output.is_empty());
            assert_eq!(row.source, "openrouter");
            let clone = row.clone();
            assert_eq!(&clone, row);
            assert!(format!("{row:?}").contains(&row.prefix));
        }
    }

    #[test]
    fn openai_by_family() {
        assert!(openai("gpt-5.5").accepts(&mt("image/png")));
        assert!(openai("o4-mini").accepts(&mt("image/jpeg")));
        assert!(!openai("gpt-3.5-turbo").accepts(&mt("image/png")));
        let audio = openai("gpt-4o-audio-preview");
        assert!(audio.accepts(&mt("audio/wav")) && audio.produces(&mt("audio/mpeg")));
        assert!(openai("gpt-realtime").accepts(&mt("audio/wav")));
        let image = openai("gpt-image-1");
        assert!(image.produces(&mt("image/png")) && !image.produces(&mt("text/plain")));
        assert!(openai("dall-e-3").produces(&mt("image/png")));
        assert!(openai("gpt-4o-mini-tts").produces(&mt("audio/wav")));
        let stt = openai("gpt-4o-transcribe");
        assert!(stt.accepts(&mt("audio/wav")) && !stt.accepts(&mt("text/plain")));
        assert!(openai("whisper-1").accepts(&mt("audio/mpeg")));
    }

    #[test]
    fn gemini_by_family() {
        let chat = gemini("gemini-3.5-flash");
        assert!(chat.accepts(&mt("video/mp4")) && chat.accepts(&mt("audio/wav")));
        assert!(!chat.produces(&mt("image/png")));
        assert!(gemini("imagen-4").produces(&mt("image/png")));
        assert!(gemini("veo-3").produces(&mt("video/mp4")));
        assert!(gemini("gemini-2.5-flash-image").produces(&mt("image/png")));
        assert!(gemini("gemini-2.5-flash-preview-tts").produces(&mt("audio/wav")));
        assert!(!gemini("gemini-embedding-001").accepts(&mt("image/png")));
    }

    #[test]
    fn codex_ollama_and_prefixes() {
        assert!(codex("gpt-5.5-codex").accepts(&mt("image/png")));
        assert!(ollama("llava:13b").accepts(&mt("image/png")));
        assert!(ollama("qwen3-vl:8b").accepts(&mt("image/png")));
        assert!(!ollama("llama3.3:70b").accepts(&mt("image/png")));
        assert!(by_prefix("anthropic/claude-sonnet-5").accepts(&mt("application/pdf")));
        assert!(by_prefix("openai/gpt-5.5").accepts(&mt("image/png")));
        assert!(by_prefix("google/gemini-3.5-flash").accepts(&mt("video/mp4")));
        assert!(!by_prefix("deepseek/deepseek-v4").accepts(&mt("image/png")));
        assert!(!by_prefix("noslash").accepts(&mt("image/png")));
    }

    #[test]
    fn builtin_mime_dispatches_by_provider_name() {
        assert!(builtin_mime("anthropic", "claude-sonnet-5").accepts(&mt("image/png")));
        assert!(builtin_mime("openai", "gpt-5.5").accepts(&mt("image/png")));
        assert!(builtin_mime("google", "gemini-3.5-flash").accepts(&mt("video/mp4")));
        assert!(builtin_mime("gemini", "gemini-3.5-flash").accepts(&mt("audio/wav")));
        assert!(builtin_mime("codex", "gpt-5.5-codex").accepts(&mt("image/png")));
        assert!(builtin_mime("ollama", "llava").accepts(&mt("image/png")));
        assert!(builtin_mime("openrouter", "openai/gpt-5.5").accepts(&mt("image/png")));
        assert!(!builtin_mime("claude-code", "claude-sonnet-5").accepts(&mt("image/png")));
    }

    #[test]
    fn a_model_info_carries_mime_and_entry_content_becomes_text() {
        let info = crate::provider::ModelInfo::new(
            "claude-sonnet-5",
            "anthropic",
            crate::capabilities::ModelCapabilities::default(),
        );
        assert!(!info.mime.takes_mime(), "text only until told");
        let info = info.with_mime(anthropic("claude-sonnet-5"));
        assert!(info.mime.accepts(&mt("image/png")));
        let content: crate::provider::MessageContent =
            leviath_core::region::EntryContent::text("hi").into();
        assert_eq!(content.as_text(), "hi");
    }

    #[test]
    fn modality_words() {
        assert_eq!(modality_pattern("Image"), Some("image/*"));
        assert_eq!(modality_pattern("file"), Some("application/pdf"));
        assert_eq!(modality_pattern("text"), Some("text/*"));
        assert_eq!(modality_pattern("audio"), Some("audio/*"));
        assert_eq!(modality_pattern("video"), Some("video/*"));
        assert_eq!(modality_pattern("smell"), None);
    }
}
