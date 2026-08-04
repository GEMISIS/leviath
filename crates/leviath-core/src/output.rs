//! An agent's final output: the one value a run hands back to whoever asked.
//!
//! Before this existed, the only way an agent could return something was to
//! write a file. Every surface that should have reported a result reported
//! something else - `GET /api/agents/{id}/result` tailed a log file, the
//! completion webhook's `result` field carried the *error* string, and
//! `wait_for_agent`, whose schema promises "return its final result", returned
//! `"Sub-agent 'x' finished with status: Complete"`. A fan-out worker's
//! contribution to its merge stage was whatever text happened to sit in its last
//! assistant message, so a worker whose final turn was a tool call contributed
//! an empty string.
//!
//! # The format rule
//!
//! **Nothing here interprets the format.** There is no enum of supported
//! formats, no per-format parser, and no branch on a format name anywhere in the
//! engine. [`OutputSpec::format`] is an opaque label; markdown, JSON, XML, CSV,
//! an [a2ui](https://a2ui.org/) document, and a house format invented next week
//! all travel the same path: describe it to the model, record what comes back
//! verbatim, hand it on unchanged.
//!
//! The single exception is opt-in and named as such. When an author supplies
//! [`OutputSpec::schema`], the submission is parsed as JSON and validated
//! against it. That is the only thing that ever looks inside the content, and it
//! happens because someone asked for it, never because a format string said
//! `"json"`.
//!
//! This is also why an unusual format needs no engine support. There is no
//! usual: every format is produced by the model from
//! [`OutputSpec::instructions`] and [`OutputSpec::example`].

use serde::{Deserialize, Serialize};

/// Largest final output kept, in bytes. Anything longer is cut at a character
/// boundary and flagged [`FinalOutput::truncated`].
///
/// Sits between the log tail the result endpoint already serves (64 KiB) and the
/// cap on reading a file the run wrote (1 MiB). A final output is meant to be an
/// answer, not a payload; an agent with megabytes to hand back should write a
/// file and say where it is.
pub const MAX_FINAL_OUTPUT_BYTES: usize = 256 * 1024;

/// What shape an agent should return.
///
/// Declared by a blueprint (`[agent.output]`), narrowed by a stage
/// (`[stages.<name>.output]`), and overridable by whoever starts the run. See
/// [`resolve_output_spec`] for how the three combine.
///
/// Every field is optional, and an entirely empty spec is meaningful: it asks
/// for a final output without constraining its shape.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSpec {
    /// An opaque label for the shape, carried to the model and recorded beside
    /// the result. `"markdown"`, `"json"`, `"a2ui"`, and
    /// `"application/vnd.acme.report+xml"` are all equally valid and equally
    /// uninterpreted. Consumers that render differently per format (a browser
    /// UI, say) match on this string; the engine never does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,

    /// Free-form guidance folded into the `submit_output` tool description and
    /// the output stage's system prompt. This is where a format that the model
    /// has never seen gets explained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,

    /// A literal sample shown to the model verbatim. The most effective lever
    /// for an unusual format, and the reason one needs no code support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<String>,

    /// A JSON Schema. **The only machine check in this module.** When present,
    /// a submission is parsed as JSON and validated against it, and a failure is
    /// refused back to the model so it can correct itself. When absent - the
    /// common case - the content is never inspected at all.
    ///
    /// Note this is a separate key rather than something inferred from
    /// `format`. Setting `format = "json"` validates nothing; supplying a schema
    /// does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
}

impl OutputSpec {
    /// Whether this spec constrains anything at all. An empty spec still asks
    /// for an output, so this is about wording the request, not skipping it.
    pub fn is_empty(&self) -> bool {
        self.format.is_none()
            && self.instructions.is_none()
            && self.example.is_none()
            && self.schema.is_none()
    }
}

/// What an agent actually produced.
///
/// [`content`](Self::content) is stored exactly as submitted. Nothing in the
/// engine reformats, re-indents, or re-serializes it, so a consumer that asked
/// for a particular byte sequence receives that byte sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalOutput {
    /// The submission, verbatim (subject only to [`MAX_FINAL_OUTPUT_BYTES`]).
    pub content: String,

    /// The format label in effect when this was submitted, if any. Copied from
    /// the resolved spec rather than guessed from the content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,

    /// The stage that produced it. Read by the enforcement gate, which must
    /// tell "this stage submitted" from "some earlier stage did".
    pub stage: String,

    /// Unix seconds at submission.
    pub submitted_at: i64,

    /// Whether [`MAX_FINAL_OUTPUT_BYTES`] cut the content short.
    #[serde(default)]
    pub truncated: bool,
}

impl FinalOutput {
    /// Record a submission, truncating at a character boundary if it exceeds
    /// [`MAX_FINAL_OUTPUT_BYTES`].
    ///
    /// Truncation walks back to a boundary rather than slicing by byte index:
    /// this workspace denies `clippy::string_slice` because a byte cut through a
    /// multi-byte character once double-panicked and aborted the whole daemon.
    pub fn new(content: &str, format: Option<String>, stage: String, submitted_at: i64) -> Self {
        let truncated = content.len() > MAX_FINAL_OUTPUT_BYTES;
        let kept = crate::text::truncate_at_boundary(content, MAX_FINAL_OUTPUT_BYTES);
        Self {
            content: kept.to_string(),
            format,
            stage,
            submitted_at,
            truncated,
        }
    }
}

/// Combine the blueprint's, the stage's, and the caller's output specs into the
/// one that governs a stage. Later levels win field by field, the way
/// [`resolve_nudge`](crate::blueprint::resolve_nudge) cascades.
///
/// Returns `None` when no level asks for an output at all, which is how a stage
/// that has nothing to hand back stays silent.
///
/// # The schema drop
///
/// A caller who names a `format` and supplies no `schema` **drops the declared
/// schema**. Validating an a2ui document against the agent's own JSON schema
/// would be nonsense: the caller asked for a different shape, so the check
/// written for the old shape no longer applies. A caller who wants validation
/// supplies a schema alongside the format. This is the one place where fields do
/// not cascade independently, and it is deliberate.
pub fn resolve_output_spec(
    agent: Option<&OutputSpec>,
    stage: Option<&OutputSpec>,
    request: Option<&OutputSpec>,
) -> Option<OutputSpec> {
    if agent.is_none() && stage.is_none() && request.is_none() {
        return None;
    }

    fn field<T: Clone>(
        agent: Option<&OutputSpec>,
        stage: Option<&OutputSpec>,
        request: Option<&OutputSpec>,
        get: impl Fn(&OutputSpec) -> Option<T>,
    ) -> Option<T> {
        request
            .and_then(&get)
            .or_else(|| stage.and_then(&get))
            .or_else(|| agent.and_then(&get))
    }

    // A caller-named format retires a schema the caller did not also supply.
    let caller_reshaped = request.is_some_and(|r| r.format.is_some() && r.schema.is_none());
    let schema = if caller_reshaped {
        None
    } else {
        field(agent, stage, request, |s| s.schema.clone())
    };

    Some(OutputSpec {
        format: field(agent, stage, request, |s| s.format.clone()),
        instructions: field(agent, stage, request, |s| s.instructions.clone()),
        example: field(agent, stage, request, |s| s.example.clone()),
        schema,
    })
}

/// Render a resolved spec as the guidance an agent reads.
///
/// Used twice for the same text: once in the `submit_output` tool description
/// and once in an output stage's system prompt. Saying it in both places matters
/// most for a format the model has no prior knowledge of, which is exactly the
/// case this module is built to support.
///
/// Returns an empty string for a spec that constrains nothing, so callers can
/// append it unconditionally.
pub fn describe_spec(spec: &OutputSpec) -> String {
    let mut parts = Vec::new();
    if let Some(format) = &spec.format {
        parts.push(format!("Return it in this format: {format}."));
    }
    if let Some(instructions) = &spec.instructions {
        parts.push(instructions.clone());
    }
    if let Some(schema) = &spec.schema {
        parts.push(format!(
            "It must be JSON valid against this schema:\n{schema}"
        ));
    }
    if let Some(example) = &spec.example {
        parts.push(format!(
            "Here is an example of the expected shape:\n{example}"
        ));
    }
    parts.join("\n\n")
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

    #[test]
    fn empty_spec_constrains_nothing() {
        assert!(OutputSpec::default().is_empty());
        assert!(!spec(Some("json"), None).is_empty());
        assert!(!spec(None, Some(json!({}))).is_empty());
        assert!(
            !OutputSpec {
                instructions: Some("be brief".to_string()),
                ..OutputSpec::default()
            }
            .is_empty()
        );
        assert!(
            !OutputSpec {
                example: Some("<doc/>".to_string()),
                ..OutputSpec::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn no_level_asking_for_output_resolves_to_none() {
        assert_eq!(resolve_output_spec(None, None, None), None);
    }

    #[test]
    fn later_levels_win_field_by_field() {
        let agent = OutputSpec {
            format: Some("markdown".to_string()),
            instructions: Some("agent guidance".to_string()),
            example: Some("agent example".to_string()),
            schema: None,
        };
        let stage = OutputSpec {
            instructions: Some("stage guidance".to_string()),
            ..OutputSpec::default()
        };
        let resolved = resolve_output_spec(Some(&agent), Some(&stage), None)
            .expect("some level asked for an output");
        // The stage narrows one field; the rest fall through to the agent.
        assert_eq!(resolved.instructions.as_deref(), Some("stage guidance"));
        assert_eq!(resolved.format.as_deref(), Some("markdown"));
        assert_eq!(resolved.example.as_deref(), Some("agent example"));
    }

    #[test]
    fn a_stage_alone_can_ask_for_an_output() {
        let stage = spec(Some("a2ui"), None);
        let resolved =
            resolve_output_spec(None, Some(&stage), None).expect("the stage asked for one");
        assert_eq!(resolved.format.as_deref(), Some("a2ui"));
    }

    #[test]
    fn a_caller_reshaping_the_output_drops_the_declared_schema() {
        let agent = spec(Some("json"), Some(json!({"type": "object"})));
        // Caller names a different format and supplies no schema of its own:
        // the schema written for the old shape no longer applies.
        let request = spec(Some("a2ui"), None);
        let resolved = resolve_output_spec(Some(&agent), None, Some(&request))
            .expect("the agent asked for one");
        assert_eq!(resolved.format.as_deref(), Some("a2ui"));
        assert_eq!(resolved.schema, None);
    }

    #[test]
    fn a_caller_supplying_its_own_schema_keeps_it() {
        let agent = spec(Some("json"), Some(json!({"type": "object"})));
        let request = spec(Some("json"), Some(json!({"type": "array"})));
        let resolved = resolve_output_spec(Some(&agent), None, Some(&request))
            .expect("the agent asked for one");
        assert_eq!(resolved.schema, Some(json!({"type": "array"})));
    }

    #[test]
    fn a_caller_that_names_no_format_leaves_the_schema_alone() {
        let agent = spec(Some("json"), Some(json!({"type": "object"})));
        // Only instructions differ, so the declared shape still stands.
        let request = OutputSpec {
            instructions: Some("keep it short".to_string()),
            ..OutputSpec::default()
        };
        let resolved = resolve_output_spec(Some(&agent), None, Some(&request))
            .expect("the agent asked for one");
        assert_eq!(resolved.format.as_deref(), Some("json"));
        assert_eq!(resolved.schema, Some(json!({"type": "object"})));
    }

    #[test]
    fn short_content_is_stored_verbatim() {
        let out = FinalOutput::new(
            "done: 3 files",
            Some("markdown".to_string()),
            "wrap".into(),
            7,
        );
        assert_eq!(out.content, "done: 3 files");
        assert_eq!(out.format.as_deref(), Some("markdown"));
        assert_eq!(out.stage, "wrap");
        assert_eq!(out.submitted_at, 7);
        assert!(!out.truncated);
    }

    #[test]
    fn oversized_content_is_cut_at_a_char_boundary_and_flagged() {
        // A multi-byte character straddling the cap: slicing by byte index here
        // is what once aborted the daemon, so the cut must walk back.
        let mut content = "a".repeat(MAX_FINAL_OUTPUT_BYTES - 1);
        content.push('\u{1f600}');
        let out = FinalOutput::new(&content, None, "wrap".into(), 0);
        assert!(out.truncated);
        assert_eq!(out.content.len(), MAX_FINAL_OUTPUT_BYTES - 1);
        assert!(out.format.is_none());
    }

    #[test]
    fn describe_spec_is_empty_when_nothing_is_constrained() {
        assert_eq!(describe_spec(&OutputSpec::default()), "");
    }

    #[test]
    fn describe_spec_renders_every_field_it_has() {
        let described = describe_spec(&OutputSpec {
            format: Some("a2ui".to_string()),
            instructions: Some("One card per finding.".to_string()),
            example: Some("{\"root\": {}}".to_string()),
            schema: Some(json!({"type": "object"})),
        });
        assert!(described.contains("Return it in this format: a2ui."));
        assert!(described.contains("One card per finding."));
        assert!(described.contains("valid against this schema"));
        assert!(described.contains("{\"root\": {}}"));
    }

    #[test]
    fn a_spec_round_trips_through_serde() {
        let original = spec(Some("a2ui"), Some(json!({"type": "object"})));
        let text = serde_json::to_string(&original).expect("a spec serializes");
        let back: OutputSpec = serde_json::from_str(&text).expect("and deserializes");
        assert_eq!(back, original);
        // Unset fields stay off the wire rather than serializing as nulls.
        assert!(!text.contains("instructions"));
    }

    #[test]
    fn a_final_output_round_trips_through_serde() {
        let original = FinalOutput::new("answer", None, "wrap".into(), 1);
        let text = serde_json::to_string(&original).expect("an output serializes");
        let back: FinalOutput = serde_json::from_str(&text).expect("and deserializes");
        assert_eq!(back, original);
    }
}
