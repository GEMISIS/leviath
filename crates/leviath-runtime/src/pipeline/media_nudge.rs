//! The media stage's nudge: a stage that makes an image, a video or audio and
//! answered with text alone is sent back, with its own words, a few times.

/// How many text-only replies an image stage is nudged back before it is let
/// go. Small on purpose: an image model that returns text three times running
/// is refusing or erroring, not warming up, and the stage's own
/// `require_output`/`max_iterations` then ends it rather than looping.
pub(crate) const MAX_NO_IMAGE_NUDGES: usize = 3;

/// The media family a stage declares it makes (`image`, `video` or `audio`),
/// from a `format` or an `output_routing` target: `None` for a stage that
/// makes text. A plain prefix check, because both are opaque labels the
/// manifest already validated as mime patterns. Image first, then video, then
/// audio, when a stage names more than one.
pub(crate) fn stage_expected_media(
    stage: Option<&crate::spec::graph::StageDef>,
) -> Option<&'static str> {
    let stage = stage?;
    let format = stage.output.as_ref().and_then(|o| o.format.as_deref());
    ["image", "video", "audio"].into_iter().find(|family| {
        let prefix = format!("{family}/");
        format.is_some_and(|f| f.starts_with(&prefix))
            || stage.output_routing.keys().any(|k| k.starts_with(&prefix))
    })
}

/// Parts of `family` among a reply's produced parts.
pub(crate) fn media_part_count(parts: &[leviath_core::mime::Part], family: &str) -> usize {
    let pattern = format!("{family}/*");
    parts
        .iter()
        .filter(|p| p.mime_type.matches(&pattern))
        .count()
}

/// The `[System]` line sent back when a media stage returned text and nothing
/// of the `family` it makes. It quotes the model's own words, because a
/// refusal or a filtered request states its reason there, so the retry is
/// informed rather than blind.
pub(crate) fn no_media_nudge(reply_text: &str, family: &str) -> String {
    let what = match family {
        "image" => "an image",
        "video" => "a video",
        _ => "audio",
    };
    let trimmed = reply_text.trim();
    if trimmed.is_empty() {
        return format!(
            "This stage produces {what}, but your last reply contained none. The \
             generation may have failed. Generate {what} and try again."
        );
    }
    let mut quoted = leviath_core::text::truncate_chars(trimmed, 500);
    if trimmed.chars().count() > 500 {
        quoted.push_str("...");
    }
    format!(
        "This stage produces {what}, but your last reply contained none, only text: \
         \"{quoted}\". That usually means the generation failed or was refused. If that \
         text names a problem, address it; then generate {what} and try again."
    )
}
