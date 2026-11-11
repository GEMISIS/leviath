//! The media stage's nudge: a stage that makes an image, a video or audio and
//! answered with none is sent back, with its own words, twice, and then
//! fails saying what came back.

/// How many replies with nothing of its family a media stage is nudged back
/// before it fails: three generations in all, whatever the graph's
/// `max_iterations`. Small on purpose: each reply from a media model is a paid
/// generation, and one that makes nothing three times running is refusing,
/// erroring, or on a route that cannot hand its media back.
pub(crate) const MAX_NO_IMAGE_NUDGES: usize = 2;

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

/// Why a media stage stopped: `attempts` replies in a row held nothing of the
/// `family` it makes. Names what the last one did hold, each part by type and
/// size, so a route that returned the wrong thing can be told from a model
/// that returned nothing.
pub(crate) fn no_media_failure(
    stage: &str,
    family: &str,
    attempts: usize,
    reply_text: &str,
    parts: &[leviath_core::mime::Part],
) -> String {
    let mut held: Vec<String> = parts
        .iter()
        .map(|part| {
            let size = match &part.body {
                leviath_core::mime::PartBody::Inline(text) => text.len() as u64,
                leviath_core::mime::PartBody::Stored(blob) => blob.size,
            };
            format!("{} ({size} bytes)", part.mime_type)
        })
        .collect();
    let characters = reply_text.trim().chars().count();
    if characters > 0 {
        held.push(format!("{characters} characters of text"));
    }
    let held = match held.is_empty() {
        true => "nothing at all".to_string(),
        false => held.join(", "),
    };
    format!(
        "stage '{stage}' makes {family}, and {attempts} replies in a row carried none. The last \
         one held {held}. A media model is not asked a fourth time: every reply is a paid \
         generation. Check that the stage's model makes {family} on the provider it ran on \
         (`lev models show <model>`)."
    )
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
