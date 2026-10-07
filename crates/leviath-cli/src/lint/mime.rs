//! The mime checks: what a stage takes against what its models see, what a
//! tool may be handed, and the rows a blueprint adds to the registry.

use super::*;
use leviath_runtime::spec::graph::StageDef;

/// A stage that limits what a tool may be handed, without granting the tool.
///
/// The limit is harmless, since a tool the stage never offers is never
/// handed anything, but it is a sign the author meant to grant the tool or
/// misspelled its name. Said only when the stage names its tools one by
/// one: under a group grant (`@builtin`, `@scripts`) whether the tool is
/// reached depends on the install, which is not the blueprint's business.
pub(super) fn lint_tool_accepts(stage: &StageDef) -> Vec<LintFinding> {
    if !tool_groups(stage).is_empty() {
        return Vec::new();
    }
    stage
        .tool_accepts
        .iter()
        .filter(|(tool, _)| {
            !named_tools(stage)
                .any(|granted| canonical_tool_name(granted) == canonical_tool_name(tool.as_str()))
        })
        .map(|(tool, list)| {
            let list: Vec<&str> = list.iter().map(|p| p.as_str()).collect();
            LintFinding::new(
                LintSeverity::Warning,
                "tool-accepts-ungranted",
                format!(
                    "limits '{tool}' to {} under tool_accepts but does not grant it, so the \
                     limit never applies",
                    list.join(", ")
                ),
            )
            .in_stage(stage.name.as_str())
            .with_fix("add the tool to the stage's tools, or drop the limit")
        })
        .collect()
}

/// A `[graph.mime_types]` row that changes the family or the text flag of a
/// type the compiled table already knows.
///
/// Legal, and sometimes right (a shop that treats SVG as text), but a row
/// that turns `image/png` into a model or makes `audio/wav` text changes
/// what every provider is handed for that agent's runs, which is rarely what
/// an extension or a check was meant to do.
pub(super) fn lint_mime_types(graph: &RunGraph) -> Vec<LintFinding> {
    let builtin = leviath_core::mime::MimeRegistry::builtin();
    let known: HashSet<String> = builtin.keys().into_iter().map(|(key, _)| key).collect();
    let mut findings = Vec::new();
    // Every key spells as a type (a `type/*` pattern included), so nothing is
    // skipped here.
    let typed = graph.mime_types.iter().filter_map(|(key, row)| {
        leviath_core::mime::MimeType::parse(key.as_str())
            .ok()
            .zip(Some(row))
    });
    for (mime_type, row) in typed {
        if !known.contains(mime_type.as_str()) {
            continue;
        }
        let was = builtin.info(&mime_type);
        let mut changed = Vec::new();
        if let Some(family) = row.family.as_deref()
            && family != was.family
        {
            changed.push(format!("family from {} to {family}", was.family));
        }
        if let Some(text) = row.text
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
                "mime-type-overrides-builtin",
                format!(
                    "[graph.mime_types] changes {mime_type}, a built-in type, for this agent's \
                     runs: {}",
                    changed.join("; ")
                ),
            )
            .with_fix("keep the row to extensions, magic, tokens, stand_in or check unless the change is meant"),
        );
    }
    findings
}

/// A stage whose regions take mime none of its listed models can see.
///
/// Such a run does not fail: every stored part reaches the model as its
/// one-line stand-in, and the model works from the file name. That is the
/// right outcome for a stage that only shuffles files, and a silent
/// surprise for one meant to look at them, so it is said once at validate
/// time. Only providers with built-in mime tables are judged; an open
/// route (no provider named) or a provider the tables do not cover is taken
/// on trust.
pub(super) fn lint_stage_mime(graph: &RunGraph, stage: &StageDef) -> Vec<LintFinding> {
    let needs: Vec<String> = stage_inputs(graph, stage)
        .into_iter()
        .filter(|p| p != "*/*")
        .collect();
    if needs.is_empty() || !all_pinned(&stage.model) {
        return Vec::new();
    }
    let judged: Vec<(&str, &str)> = stage.model.models.iter().map(route).collect();
    let unseen: Vec<&String> = needs
        .iter()
        .filter(|need| {
            !judged.iter().any(|(provider, model)| {
                leviath_providers::mime_tables::builtin_mime(provider, model)
                    .covers(std::slice::from_ref(need))
            })
        })
        .collect();
    if unseen.is_empty() {
        return Vec::new();
    }
    // A stage whose models see some of what it takes and not the rest is a
    // media pipeline working as designed: the mesh stage sees the mesh and
    // reads the reference images as stand-ins, the drawing stage the other
    // way round. That is said as information. The warning is for a stage
    // that sees none of it - a text model asked to look at pictures.
    let severity = match unseen.len() == needs.len() {
        true => LintSeverity::Warning,
        false => LintSeverity::Note,
    };
    let listed: Vec<String> = judged
        .iter()
        .map(|(provider, model)| format!("{provider}/{model}"))
        .collect();
    vec![
        LintFinding::new(
            severity,
            "mime-unseen",
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
        .in_stage(stage.name.as_str())
        .with_fix(
            "list a model that takes the type (lev models --accepts <type>), list a type the \
             model can read as text under the stage's input_as_text, or leave it if the stage \
             only needs the file names"
                .to_string(),
        ),
    ]
}
