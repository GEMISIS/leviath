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

    /// A JSON Schema describing the answer's shape. When present, a submission
    /// is parsed as JSON and validated against it, and a failure is refused back
    /// to the model so it can correct itself.
    ///
    /// Separate from `format` because they answer different questions.
    /// `format = "json"` asks "does this parse as JSON"; a schema asks "does the
    /// parsed document have the fields I need". A format check comes free for
    /// the handful of formats the engine can parse; shape is only ever checked
    /// when someone writes a schema down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,

    /// A `.rhai` script that decides whether an answer is valid, as a path
    /// relative to the blueprint directory.
    ///
    /// For a format the engine cannot parse and a shape a JSON Schema cannot
    /// describe. The script defines `fn validate(content)` and returns `()` when
    /// the answer is fine or a string saying what is wrong; the string goes back
    /// to the agent as the same refusal a schema failure produces.
    ///
    /// Written for the format it accompanies, so a caller who overrides the
    /// format retires it along with the schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validator: Option<String>,
}

impl OutputSpec {
    /// Whether this spec constrains anything at all. An empty spec still asks
    /// for an output, so this is about wording the request, not skipping it.
    pub fn is_empty(&self) -> bool {
        self.format.is_none()
            && self.instructions.is_none()
            && self.example.is_none()
            && self.schema.is_none()
            && self.validator.is_none()
    }
}

/// What an agent actually produced, content included.
///
/// [`content`](Self::content) is stored exactly as submitted. Nothing in the
/// engine reformats, re-indents, or re-serializes it, so a consumer that asked
/// for a particular byte sequence receives that byte sequence.
///
/// This is the in-memory and one-shot form: the live ECS component, the
/// completion event, a webhook body, a reply to a waiting parent. What a run's
/// `meta.json` carries is the [`FinalOutputDescriptor`], because that file is
/// read for every run on every listing and must not carry a payload.
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

    /// Files the run produced, as workdir-relative paths.
    ///
    /// An answer is one model response; anything larger is a file. A run that
    /// gathers two million rows writes them incrementally and names the file
    /// here, so a consumer can fetch it rather than parse the path out of prose.
    /// Validated to resolve inside the run's working directory, the same rule
    /// the files endpoint enforces when serving one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
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
            artifacts: Vec::new(),
        }
    }

    /// The same submission with `artifacts` attached.
    pub fn with_artifacts(mut self, artifacts: Vec<String>) -> Self {
        self.artifacts = artifacts;
        self
    }

    /// Everything about this answer except the bytes.
    pub fn descriptor(&self) -> FinalOutputDescriptor {
        FinalOutputDescriptor {
            format: self.format.clone(),
            stage: self.stage.clone(),
            submitted_at: self.submitted_at,
            bytes: self.content.len(),
            truncated: self.truncated,
            artifacts: self.artifacts.clone(),
        }
    }
}

/// What a run's `meta.json` records about its answer: everything but the bytes.
///
/// The content lives beside it in a sidecar file
/// ([`FINAL_OUTPUT_FILE`]). `meta.json` is
/// parsed for every run on every `lev ps`, every `/api/runs` page, and every
/// restart scan, so a payload in it is paid for by operations that never wanted
/// it: a thousand answered runs would mean hundreds of megabytes of JSON per
/// listing. A descriptor is a couple of hundred bytes and stays that way.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalOutputDescriptor {
    /// The format label the answer was produced under, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// The stage that produced it.
    pub stage: String,
    /// Unix seconds at submission.
    pub submitted_at: i64,
    /// Size of the answer in bytes, so a caller can decide whether to fetch it.
    #[serde(default)]
    pub bytes: usize,
    /// Whether [`MAX_FINAL_OUTPUT_BYTES`] cut the answer short.
    #[serde(default)]
    pub truncated: bool,
    /// Files the run produced, as workdir-relative paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
}

/// The file, inside a run's directory, holding the answer's bytes.
///
/// Raw content with no wrapper, so serving it is a read and `lev result --raw`
/// is a copy.
pub const FINAL_OUTPUT_FILE: &str = "final_output";

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

    // A shape check is written for one format. When a caller asks for a
    // different one, a check the blueprint declared no longer describes what is
    // being produced, so it is retired rather than applied to something it was
    // never about. A caller who wants their new shape checked supplies their own.
    let declared_format = field(agent, stage, None, |s| s.format.clone());
    let requested_format = request.and_then(|r| r.format.clone());
    let reshaped = requested_format.is_some() && requested_format != declared_format;

    let shape_field = |get: fn(&OutputSpec) -> Option<serde_json::Value>| match reshaped {
        true => request.and_then(get),
        false => field(agent, stage, request, get),
    };
    let validator = match reshaped {
        true => request.and_then(|r| r.validator.clone()),
        false => field(agent, stage, request, |s| s.validator.clone()),
    };

    Some(OutputSpec {
        format: field(agent, stage, request, |s| s.format.clone()),
        instructions: field(agent, stage, request, |s| s.instructions.clone()),
        example: field(agent, stage, request, |s| s.example.clone()),
        schema: shape_field(|s| s.schema.clone()),
        validator,
    })
}

/// Render a resolved spec as the guidance an agent reads.
///
/// Used twice for the same text: once in the `submit_output` tool description
/// and once in an output stage's system prompt. Saying it in both places matters
/// most for a format the model has no prior knowledge of, which is exactly the
/// case this module is built to support.
///
/// A constrained spec closes with a precedence sentence, because without one
/// this text and the stage's own system prompt are two peer instructions and
/// which wins is model-dependent: a stage prompt saying "lead with the
/// diagnosis" beats `--output-instructions "reply with only the integer"` on
/// some models and loses on others. By the time this runs, [`resolve_output_spec`]
/// has already picked one winner per field - a caller's flag replaces the
/// blueprint's line rather than joining it - so there is exactly one shape here
/// and it is the one that should govern. The sentence is scoped to presentation
/// so a bare `format` does not read as licence to drop content.
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
    if !parts.is_empty() {
        parts.push(
            "This governs how the answer is presented. Where anything else you were told says \
             to present it differently - its length, its structure, what to lead with - follow \
             this."
                .to_string(),
        );
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

    /// The artifacts list is how an answer points at what it could never
    /// contain: a dataset, a report, a directory of generated files. It travels
    /// with the descriptor so a caller can fetch them without parsing paths back
    /// out of prose.
    #[test]
    fn artifacts_attach_to_a_submission_and_reach_the_descriptor() {
        let output = FinalOutput::new(
            "the summary",
            Some("markdown".to_string()),
            "present".to_string(),
            42,
        )
        .with_artifacts(vec![
            "data/dataset.csv".to_string(),
            "report.pdf".to_string(),
        ]);

        assert_eq!(output.artifacts, ["data/dataset.csv", "report.pdf"]);
        assert_eq!(output.descriptor().artifacts, output.artifacts);
        // The bytes stay out of the descriptor: it goes in `meta.json`, which is
        // read for every run in a listing.
        assert_eq!(output.descriptor().bytes, "the summary".len());
    }

    #[test]
    fn a_submission_carries_no_artifacts_unless_given_some() {
        assert!(
            FinalOutput::new("x", None, "present".to_string(), 0)
                .artifacts
                .is_empty()
        );
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
            validator: None,
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

    /// The bug this replaced: naming the format the blueprint already declared
    /// dropped the schema, so a caller who asked for exactly what was on offer
    /// lost the check that came with it.
    #[test]
    fn re_stating_the_declared_format_keeps_its_shape_checks() {
        let agent = OutputSpec {
            format: Some("json".to_string()),
            schema: Some(json!({"type": "object"})),
            validator: Some("v.rhai".to_string()),
            ..OutputSpec::default()
        };
        let request = spec(Some("json"), None);
        let resolved = resolve_output_spec(Some(&agent), None, Some(&request))
            .expect("the agent asked for one");
        assert_eq!(resolved.schema, Some(json!({"type": "object"})));
        assert_eq!(resolved.validator.as_deref(), Some("v.rhai"));
    }

    /// A Rhai validator is written for one format, so it retires with the schema
    /// when a caller asks for a different one.
    #[test]
    fn reshaping_retires_the_validator_too() {
        let agent = OutputSpec {
            format: Some("a2ui".to_string()),
            validator: Some("a2ui.rhai".to_string()),
            ..OutputSpec::default()
        };
        let request = spec(Some("xml"), None);
        let resolved = resolve_output_spec(Some(&agent), None, Some(&request))
            .expect("the agent asked for one");
        assert_eq!(resolved.format.as_deref(), Some("xml"));
        assert_eq!(resolved.validator, None);
    }

    /// A caller that brings its own checks keeps them.
    #[test]
    fn a_caller_can_supply_shape_checks_with_its_own_format() {
        let agent = OutputSpec {
            format: Some("a2ui".to_string()),
            validator: Some("a2ui.rhai".to_string()),
            ..OutputSpec::default()
        };
        let request = OutputSpec {
            format: Some("json".to_string()),
            schema: Some(json!({"type": "array"})),
            ..OutputSpec::default()
        };
        let resolved = resolve_output_spec(Some(&agent), None, Some(&request))
            .expect("the agent asked for one");
        assert_eq!(resolved.schema, Some(json!({"type": "array"})));
        assert_eq!(resolved.validator, None, "the agent's own is still retired");
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
            validator: None,
        });
        assert!(described.contains("Return it in this format: a2ui."));
        assert!(described.contains("One card per finding."));
        assert!(described.contains("valid against this schema"));
        assert!(described.contains("{\"root\": {}}"));
    }

    /// Without this the spec and the stage's own system prompt are two peer
    /// instructions, and a strongly-shaped stage prompt wins on some models
    /// and loses on others.
    #[test]
    fn a_constrained_spec_says_it_outranks_the_stage_prompt() {
        let described = describe_spec(&OutputSpec {
            instructions: Some("Reply with only the integer.".to_string()),
            ..OutputSpec::default()
        });
        assert!(
            described.contains("Where anything else you were told"),
            "{described}"
        );
        // Last, so it is read as governing what precedes it rather than as one
        // more line the next paragraph can override.
        assert!(
            described.trim_end().ends_with("follow this."),
            "{described}"
        );
    }

    /// A format on its own is still a shape, so it still outranks a prompt that
    /// describes a different one.
    #[test]
    fn a_format_only_spec_claims_precedence_too() {
        let described = describe_spec(&OutputSpec {
            format: Some("text".to_string()),
            ..OutputSpec::default()
        });
        assert!(
            described.contains("Where anything else you were told"),
            "{described}"
        );
    }

    /// The claim is scoped to presentation. A spec that constrains nothing must
    /// not tell a model to disregard its stage prompt.
    #[test]
    fn an_unconstrained_spec_claims_nothing() {
        assert!(!describe_spec(&OutputSpec::default()).contains("follow this"));
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
