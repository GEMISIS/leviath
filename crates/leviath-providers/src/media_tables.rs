//! What the built-in vendors' models take and produce, by name.
//!
//! These are the opinionated part: a vendor's API has one way to send an
//! image and one list of models that accept one, and both are published,
//! not discoverable. Everything else in the engine asks the registry and
//! these answers rather than a type name. A listing that says more (as
//! OpenRouter's does) corrects a row here, and an operator's
//! `[model_capabilities]` entry corrects both.

use crate::capabilities::ModelMedia;

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

/// Every Claude model in the current line-up reads images and PDFs and
/// writes text.
pub(crate) fn anthropic(_model: &str) -> ModelMedia {
    ModelMedia::new(VISION_DOC, TEXT)
}

/// OpenAI: the chat and reasoning models read images and PDFs; the audio
/// models also take and return audio; the image models return images.
pub(crate) fn openai(model: &str) -> ModelMedia {
    let m = lower(model);
    if m.starts_with("gpt-image") || m.starts_with("dall-e") {
        return ModelMedia::new(&["text/*", "image/*"], &["image/*"]);
    }
    if m.contains("audio") || m.contains("realtime") {
        return ModelMedia::new(&["text/*", "audio/*"], &["text/*", "audio/*"]);
    }
    if m.contains("tts") {
        return ModelMedia::new(TEXT, &["audio/*"]);
    }
    if m.contains("transcribe") || m.starts_with("whisper") {
        return ModelMedia::new(&["audio/*"], TEXT);
    }
    let vision = ["gpt-4.1", "gpt-4o", "gpt-5", "o3", "o4", "o1", "chatgpt"];
    if vision.iter().any(|p| m.starts_with(p)) {
        return ModelMedia::new(VISION_DOC, TEXT);
    }
    ModelMedia::text_only()
}

/// Gemini: the chat models take text, images, audio, video and PDFs; the
/// image models return images; Imagen and Veo are generators.
pub(crate) fn gemini(model: &str) -> ModelMedia {
    let m = lower(model);
    if m.starts_with("imagen") {
        return ModelMedia::new(TEXT, &["image/*"]);
    }
    if m.starts_with("veo") {
        return ModelMedia::new(&["text/*", "image/*"], &["video/*"]);
    }
    if m.contains("-image") {
        return ModelMedia::new(&["text/*", "image/*"], &["text/*", "image/*"]);
    }
    if m.contains("-tts") {
        return ModelMedia::new(TEXT, &["audio/*"]);
    }
    if m.contains("embedding") {
        return ModelMedia::text_only();
    }
    ModelMedia::new(GEMINI_INPUT, TEXT)
}

/// Codex: the Responses API takes images beside text.
pub(crate) fn codex(_model: &str) -> ModelMedia {
    ModelMedia::new(&["text/*", "image/*"], TEXT)
}

/// A local model, by the names the vision builds are published under.
pub(crate) fn ollama(model: &str) -> ModelMedia {
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
        return ModelMedia::new(&["text/*", "image/*"], TEXT);
    }
    ModelMedia::text_only()
}

/// A gateway id with a vendor prefix, answered by that vendor's table.
pub(crate) fn by_prefix(model: &str) -> ModelMedia {
    let m = lower(model);
    match m.split_once('/') {
        Some(("anthropic", rest)) => anthropic(rest),
        Some(("openai", rest)) => openai(rest),
        Some(("google", rest)) => gemini(rest),
        _ => ModelMedia::text_only(),
    }
}

/// What this build's table says a model on `provider` takes and produces,
/// for a caller with no provider instance in hand (a listing compiled from
/// the tables). The provider's own [`crate::Provider::media`] is the answer
/// to prefer when an instance exists, since it also reads the listing and the
/// operator's overrides.
pub fn builtin_media(provider: &str, model: &str) -> ModelMedia {
    match provider {
        "anthropic" => anthropic(model),
        "openai" => openai(model),
        "google" | "gemini" => gemini(model),
        "codex" => codex(model),
        "ollama" => ollama(model),
        "openrouter" => by_prefix(model),
        _ => ModelMedia::text_only(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::media::MediaType;

    fn mt(s: &str) -> MediaType {
        MediaType::parse(s).unwrap()
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
    fn builtin_media_dispatches_by_provider_name() {
        assert!(builtin_media("anthropic", "claude-sonnet-5").accepts(&mt("image/png")));
        assert!(builtin_media("openai", "gpt-5.5").accepts(&mt("image/png")));
        assert!(builtin_media("google", "gemini-3.5-flash").accepts(&mt("video/mp4")));
        assert!(builtin_media("gemini", "gemini-3.5-flash").accepts(&mt("audio/wav")));
        assert!(builtin_media("codex", "gpt-5.5-codex").accepts(&mt("image/png")));
        assert!(builtin_media("ollama", "llava").accepts(&mt("image/png")));
        assert!(builtin_media("openrouter", "openai/gpt-5.5").accepts(&mt("image/png")));
        assert!(!builtin_media("claude-code", "claude-sonnet-5").accepts(&mt("image/png")));
    }

    #[test]
    fn a_model_info_carries_media_and_entry_content_becomes_text() {
        let info = crate::provider::ModelInfo::new(
            "claude-sonnet-5",
            "anthropic",
            crate::capabilities::ModelCapabilities::default(),
        );
        assert!(!info.media.takes_media(), "text only until told");
        let info = info.with_media(anthropic("claude-sonnet-5"));
        assert!(info.media.accepts(&mt("image/png")));
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
