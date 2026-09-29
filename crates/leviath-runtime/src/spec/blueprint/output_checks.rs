//! The checks a caller's output request retires, said out loud at spawn.
//!
//! [`resolve_output_spec`](leviath_core::output::resolve_output_spec) lets a
//! caller's format replace a blueprint's declared one, and with it the declared
//! validator and schema. This walks a blueprint's stages ahead of time so the
//! caller hears about it before the run starts.

use leviath_core::output::OutputSpec;

use super::Blueprint;

/// The warnings a caller's requested output shape earns at spawn: what
/// [`resolve_output_spec`](leviath_core::output::resolve_output_spec) will retire, said out loud before it happens.
///
/// Retiring the declared Rhai validator and JSON schema when the request names
/// a different format is deliberate and stays: a check written for one shape
/// cannot judge another. These warnings say so out loud, so a caller who types
/// `--output-format json` over a blueprint with a validator does not go on
/// believing the run is still being checked.
///
/// One line per group of stages losing the same checks, so an agent-level
/// validator shared by four stages reads as one sentence naming four stages.
/// Empty when there is nothing to say: no request, no format in it, the
/// declared format re-stated (which retires nothing), or nothing declared that
/// could be retired. A declared schema the request *replaces* with its own is
/// also not warned about: supplying a schema for the new shape is exactly what
/// the warning would have asked for. A declared validator is always worth the
/// line, because no request can bring a replacement for it.
pub fn retired_check_warnings(blueprint: &Blueprint, request: Option<&OutputSpec>) -> Vec<String> {
    let Some(requested) = request.and_then(|r| r.format.as_deref()) else {
        return Vec::new();
    };
    let request_has_schema = request.is_some_and(|r| r.schema.is_some());
    let mut groups: Vec<(RetiredChecks, Vec<String>)> = Vec::new();
    for stage in &blueprint.stages {
        let Some(retired) = retired_checks_for_stage(
            blueprint.output.as_ref(),
            stage.output.as_ref(),
            requested,
            request_has_schema,
        ) else {
            continue;
        };
        match groups.iter_mut().find(|(g, _)| *g == retired) {
            Some((_, stages)) => stages.push(stage.name.clone()),
            None => groups.push((retired, vec![stage.name.clone()])),
        }
    }
    groups
        .iter()
        .map(|(checks, stages)| checks.warning_line(requested, stages, request_has_schema))
        .collect()
}

/// The declared checks one stage loses to a format override. Two stages with
/// equal values lose the same thing and share one warning line.
#[derive(PartialEq, Eq)]
struct RetiredChecks {
    /// The format the retired checks were written for, when one was declared.
    declared_format: Option<String>,
    /// The retired Rhai validator's path, when one was declared.
    validator: Option<String>,
    /// Whether a declared JSON schema is retired with nothing in its place.
    schema: bool,
}

/// What `requested` retires for one stage, or `None` when it retires nothing.
///
/// Asks [`resolve_output_spec`](leviath_core::output::resolve_output_spec)'s question ahead of time, with the same
/// cascade: the stage's declaration wins over the agent's, and re-stating the
/// declared format keeps every check. Kept beside it so the two cannot drift.
fn retired_checks_for_stage(
    agent: Option<&OutputSpec>,
    stage: Option<&OutputSpec>,
    requested: &str,
    request_has_schema: bool,
) -> Option<RetiredChecks> {
    let declared =
        |get: fn(&OutputSpec) -> Option<&str>| stage.and_then(get).or_else(|| agent.and_then(get));
    let declared_format = declared(|s| s.format.as_deref());
    if declared_format == Some(requested) {
        return None;
    }
    let validator = declared(|s| s.validator.as_deref());
    let schema = !request_has_schema
        && stage
            .and_then(|s| s.schema.as_ref())
            .or_else(|| agent.and_then(|a| a.schema.as_ref()))
            .is_some();
    if validator.is_none() && !schema {
        return None;
    }
    Some(RetiredChecks {
        declared_format: declared_format.map(str::to_string),
        validator: validator.map(str::to_string),
        schema,
    })
}

impl RetiredChecks {
    /// The warning itself, worded for a person on any spawn path: what was
    /// requested, what it retires, and how to get the new shape checked. The
    /// closing advice depends on the request: a caller who already brought a
    /// schema for the new shape has nothing further to supply.
    fn warning_line(&self, requested: &str, stages: &[String], request_has_schema: bool) -> String {
        let cause = match &self.declared_format {
            Some(declared) => format!(
                "requested output format '{requested}' differs from the declared '{declared}'"
            ),
            None => format!(
                "requested output format '{requested}' reshapes an output declared without a \
                 format"
            ),
        };
        let what = match (&self.validator, self.schema) {
            (Some(v), true) => format!("the Rhai validator '{v}' and the JSON schema"),
            (Some(v), false) => format!("the Rhai validator '{v}'"),
            (None, _) => "the JSON schema".to_string(),
        };
        let tail = match request_has_schema {
            true => "the schema supplied with the request is what checks the answer now",
            false => {
                "nothing checks the answer's shape; supply a schema with the request if the new \
                 shape needs one"
            }
        };
        format!(
            "{cause}: {what} declared for {} will not run, because a check written for one shape \
             cannot judge another. Instead, {tail}.",
            stage_phrase(stages)
        )
    }
}

/// `stage 'plan'`, `stages 'plan' and 'wrap'`, `stages 'a', 'b', and 'c'`.
/// Callers only group stages they saw, so the slice is never empty.
fn stage_phrase(stages: &[String]) -> String {
    let quoted: Vec<String> = stages.iter().map(|s| format!("'{s}'")).collect();
    match quoted.split_last() {
        Some((last, [])) => format!("stage {last}"),
        Some((last, [first])) => format!("stages {first} and {last}"),
        Some((last, head)) => format!("stages {}, and {last}", head.join(", ")),
        // Unreachable by construction; an empty phrase keeps the sentence
        // grammatical if a future caller ever passes one.
        None => "its stages".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(format: Option<&str>, schema: Option<serde_json::Value>) -> OutputSpec {
        OutputSpec {
            format: format.map(str::to_string),
            schema,
            ..OutputSpec::default()
        }
    }

    /// A parsed blueprint whose agent-level output is `agent`, over stages
    /// named and shaped by `stages`. Both take TOML output tables (or `None`),
    /// because a manifest is where these declarations really come from.
    fn blueprint_with(agent: Option<&str>, stages: &[(&str, Option<&str>)]) -> Blueprint {
        let mut manifest =
            String::from("[agent]\nname = \"checked\"\nversion = \"1.0.0\"\ndescription = \"d\"\n");
        if let Some(fields) = agent {
            manifest.push_str(&format!("\n[agent.output]\n{fields}\n"));
        }
        for (name, output) in stages {
            manifest.push_str(&format!("\n[stages.{name}]\nsystem_prompt = \"p\"\n"));
            if let Some(fields) = output {
                manifest.push_str(&format!("\n[stages.{name}.output]\n{fields}\n"));
            }
        }
        crate::spec::manifest::parse_manifest(&manifest).expect("the test manifest parses")
    }

    /// The headline: a differing format retires the declared validator, and
    /// the warning names the stage, the script, and both formats.
    #[test]
    fn a_differing_format_earns_a_warning_naming_the_retired_validator() {
        let bp = blueprint_with(
            Some("format = \"markdown\"\nvalidator = \"checks/report.rhai\""),
            &[("plan", None)],
        );
        let request = spec(Some("json"), None);
        let warnings = retired_check_warnings(&bp, Some(&request));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let line = &warnings[0];
        assert!(line.contains("'json'"), "{line}");
        assert!(line.contains("'markdown'"), "{line}");
        assert!(
            line.contains("the Rhai validator 'checks/report.rhai'"),
            "{line}"
        );
        assert!(line.contains("stage 'plan'"), "{line}");
    }

    /// Re-stating the declared format keeps the checks (see
    /// `re_stating_the_declared_format_keeps_its_shape_checks`), so it earns
    /// no warning - and neither does a request with no format in it, nor no
    /// request at all.
    #[test]
    fn nothing_retired_means_nothing_warned() {
        let bp = blueprint_with(
            Some("format = \"markdown\"\nvalidator = \"v.rhai\"\nschema = { type = \"object\" }"),
            &[("plan", None)],
        );
        let restated = spec(Some("markdown"), None);
        assert!(retired_check_warnings(&bp, Some(&restated)).is_empty());
        let formatless = OutputSpec {
            instructions: Some("keep it short".to_string()),
            ..OutputSpec::default()
        };
        assert!(retired_check_warnings(&bp, Some(&formatless)).is_empty());
        assert!(retired_check_warnings(&bp, None).is_empty());
        // And a blueprint with nothing retirable has nothing to lose.
        let unchecked = blueprint_with(Some("format = \"markdown\""), &[("plan", None)]);
        let reshaped = spec(Some("json"), None);
        assert!(retired_check_warnings(&unchecked, Some(&reshaped)).is_empty());
    }

    /// A schema alone, a validator alone, and the two together each word the
    /// loss precisely.
    #[test]
    fn the_warning_names_exactly_what_is_lost() {
        let request = spec(Some("json"), None);
        let schema_only = blueprint_with(
            Some("format = \"markdown\"\nschema = { type = \"object\" }"),
            &[("plan", None)],
        );
        let warnings = retired_check_warnings(&schema_only, Some(&request));
        assert!(
            warnings[0].contains("the JSON schema declared"),
            "{warnings:?}"
        );
        let both = blueprint_with(
            Some("format = \"markdown\"\nvalidator = \"v.rhai\"\nschema = { type = \"object\" }"),
            &[("plan", None)],
        );
        let warnings = retired_check_warnings(&both, Some(&request));
        assert!(
            warnings[0].contains("the Rhai validator 'v.rhai' and the JSON schema"),
            "{warnings:?}"
        );
    }

    /// A caller who brings a schema for the new shape replaced the declared
    /// one on purpose, so only the validator - which nothing can replace - is
    /// still worth a warning.
    #[test]
    fn a_replacement_schema_is_not_warned_about() {
        let bp = blueprint_with(
            Some("format = \"markdown\"\nvalidator = \"v.rhai\"\nschema = { type = \"object\" }"),
            &[("plan", None)],
        );
        let request = spec(Some("json"), Some(json!({"type": "array"})));
        let warnings = retired_check_warnings(&bp, Some(&request));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("the Rhai validator 'v.rhai'"),
            "{warnings:?}"
        );
        assert!(
            !warnings[0].contains("JSON schema declared"),
            "{warnings:?}"
        );
        // And the advice acknowledges the schema they brought rather than
        // asking for one.
        assert!(
            warnings[0].contains("the schema supplied with the request"),
            "{warnings:?}"
        );
        // With nothing but the schema declared, the replacement leaves nothing
        // retired at all.
        let schema_only = blueprint_with(
            Some("format = \"markdown\"\nschema = { type = \"object\" }"),
            &[("plan", None)],
        );
        assert!(retired_check_warnings(&schema_only, Some(&request)).is_empty());
    }

    /// An agent-level validator shared by several stages is one line naming
    /// them all, and a stage with its own distinct declaration gets its own.
    #[test]
    fn stages_losing_the_same_checks_share_one_line() {
        let bp = blueprint_with(
            Some("format = \"markdown\"\nvalidator = \"shared.rhai\""),
            &[
                ("plan", None),
                ("draft", None),
                ("wrap", Some("format = \"a2ui\"\nvalidator = \"a2ui.rhai\"")),
            ],
        );
        let request = spec(Some("json"), None);
        let warnings = retired_check_warnings(&bp, Some(&request));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings[0].contains("stages 'plan' and 'draft'"),
            "{warnings:?}"
        );
        assert!(warnings[1].contains("stage 'wrap'"), "{warnings:?}");
        assert!(warnings[1].contains("'a2ui'"), "{warnings:?}");
    }

    /// A declaration that lives only on a stage retires the same way: the
    /// warning does not need an agent-level `[agent.output]` to exist.
    #[test]
    fn a_stage_level_declaration_retires_without_an_agent_one() {
        let bp = blueprint_with(
            None,
            &[(
                "plan",
                Some("format = \"markdown\"\nvalidator = \"v.rhai\""),
            )],
        );
        let request = spec(Some("json"), None);
        let warnings = retired_check_warnings(&bp, Some(&request));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("the Rhai validator 'v.rhai'"),
            "{warnings:?}"
        );
    }

    /// A stage that re-declares the requested format keeps its checks even
    /// while its siblings lose theirs, because the cascade is per stage.
    #[test]
    fn a_stage_already_in_the_requested_format_keeps_its_checks() {
        let bp = blueprint_with(
            Some("format = \"markdown\"\nvalidator = \"shared.rhai\""),
            &[("plan", None), ("emit", Some("format = \"json\""))],
        );
        let request = spec(Some("json"), None);
        let warnings = retired_check_warnings(&bp, Some(&request));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("stage 'plan'"), "{warnings:?}");
    }

    /// A validator declared without any format is still retired by naming one
    /// (the request reshapes an output that never named its shape), and the
    /// warning says so without inventing a declared format.
    #[test]
    fn a_formatless_declaration_is_reshaped_by_any_request() {
        let bp = blueprint_with(Some("validator = \"v.rhai\""), &[("plan", None)]);
        let request = spec(Some("json"), None);
        let warnings = retired_check_warnings(&bp, Some(&request));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("declared without a format"),
            "{warnings:?}"
        );
    }

    /// The three list shapes, plus the guard for a slice no caller produces.
    #[test]
    fn stage_phrases_read_as_prose() {
        let names = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(stage_phrase(&names(&["a"])), "stage 'a'");
        assert_eq!(stage_phrase(&names(&["a", "b"])), "stages 'a' and 'b'");
        assert_eq!(
            stage_phrase(&names(&["a", "b", "c"])),
            "stages 'a', 'b', and 'c'"
        );
        assert_eq!(stage_phrase(&[]), "its stages");
    }
}
