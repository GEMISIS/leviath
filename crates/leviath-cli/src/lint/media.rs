//! The media checks: what a stage takes against what its models see, what a
//! tool may be handed, and the rows a blueprint adds to the registry.

use super::*;

/// A stage that limits what a tool may be handed, without granting the tool.
///
/// The limit is harmless, since a tool the stage never offers is never
/// handed anything, but it is a sign the author meant to grant the tool or
/// misspelled its name. Said only when the stage names its tools one by
/// one: under a group grant (`@builtin`, `@scripts`) whether the tool is
/// reached depends on the install, which is not the manifest's business.
pub(super) fn lint_tool_accepts(stage: &leviath_core::Stage) -> Vec<LintFinding> {
    if !stage.tool_groups().is_empty() {
        return Vec::new();
    }
    stage
        .tool_accepts
        .iter()
        .filter(|(tool, _)| {
            !stage
                .named_tools()
                .any(|granted| canonical_tool_name(granted) == canonical_tool_name(tool))
        })
        .map(|(tool, list)| {
            LintFinding::new(
                LintSeverity::Warning,
                "tool-accepts-ungranted",
                format!(
                    "limits '{tool}' to {} under tool_accepts but does not grant it, so the \
                     limit never applies",
                    list.join(", ")
                ),
            )
            .in_stage(&stage.name)
            .with_fix("add the tool to available_tools, or drop the limit")
        })
        .collect()
}

/// A blueprint `[media_types]` row that changes the family or the text flag
/// of a type the compiled table already knows.
///
/// Legal, and sometimes right (a shop that treats SVG as text), but a row
/// that turns `image/png` into a model or makes `audio/wav` text changes
/// what every provider is handed for that agent's runs, which is rarely what
/// an extension or a check was meant to do.
pub(super) fn lint_media_types(blueprint: &Blueprint) -> Vec<LintFinding> {
    let builtin = leviath_core::media::MediaRegistry::builtin();
    let known: std::collections::HashSet<String> =
        builtin.keys().into_iter().map(|(key, _)| key).collect();
    let mut findings = Vec::new();
    // Every key the parser accepted spells as a type (a `type/*` pattern
    // included), so nothing is skipped here.
    let typed = blueprint.media_types.iter().filter_map(|(key, row)| {
        leviath_core::media::MediaType::parse(key)
            .ok()
            .zip(Some(row))
    });
    for (media_type, row) in typed {
        if !known.contains(media_type.as_str()) {
            continue;
        }
        let was = builtin.info(&media_type);
        let mut changed = Vec::new();
        if let Some(family) = row.get("family").and_then(|v| v.as_str())
            && family != was.family
        {
            changed.push(format!("family from {} to {family}", was.family));
        }
        if let Some(text) = row.get("text").and_then(|v| v.as_bool())
            && text != was.text
        {
            changed.push(format!("text from {} to {text}", was.text));
        }
        if changed.is_empty() {
            continue;
        }
        findings.push(
            LintFinding::new(
                LintSeverity::Warning,
                "media-type-overrides-builtin",
                format!(
                    "[media_types] changes {media_type}, a built-in type, for this agent's runs: {}",
                    changed.join("; ")
                ),
            )
            .with_fix("keep the row to extensions, magic, tokens, stand_in or check unless the change is meant"),
        );
    }
    findings
}

/// A stage whose regions take media none of its listed models can see.
///
/// Such a run does not fail: every stored part reaches the model as its
/// one-line stand-in, and the model works from the file name. That is the
/// right outcome for a stage that only shuffles files, and a silent
/// surprise for one meant to look at them, so it is said once at validate
/// time. Only providers with built-in media tables are judged; an open
/// route (no provider named) or a provider the tables do not cover is taken
/// on trust.
pub(super) fn lint_stage_media(
    blueprint: &Blueprint,
    stage: &leviath_core::Stage,
) -> Vec<LintFinding> {
    let needs: Vec<String> = blueprint
        .stage_inputs(stage)
        .into_iter()
        .filter(|p| p != "*/*")
        .collect();
    if needs.is_empty() {
        return Vec::new();
    }
    let judged: Vec<&leviath_core::blueprint::ModelEntry> = stage
        .model
        .models
        .iter()
        .filter(|e| !e.provider.is_empty())
        .collect();
    if judged.is_empty() || judged.len() != stage.model.models.len() {
        return Vec::new();
    }
    let unseen: Vec<&String> = needs
        .iter()
        .filter(|need| {
            !judged.iter().any(|e| {
                leviath_providers::media_tables::builtin_media(&e.provider, &e.model)
                    .covers(std::slice::from_ref(need))
            })
        })
        .collect();
    if unseen.is_empty() {
        return Vec::new();
    }
    let listed: Vec<String> = judged
        .iter()
        .map(|e| format!("{}/{}", e.provider, e.model))
        .collect();
    vec![
        LintFinding::new(
            LintSeverity::Warning,
            "media-unseen",
            format!(
                "takes {} but none of its models ({}) takes that natively, so such parts reach \
                 the model as one-line stand-ins",
                unseen
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                listed.join(", ")
            ),
        )
        .in_stage(&stage.name)
        .with_fix(
            "list a model that takes the type (lev models --accepts <type>), set \
             [stages.<name>.input] as_text for a type the model can read as text, or \
             leave it if the stage only needs the file names"
                .to_string(),
        ),
    ]
}
