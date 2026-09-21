//! What a run or a stage is asked to hand back.

use async_graphql::{Enum, SimpleObject};

use crate::commands::serve::graphql::scalars::Json;

/// What happens when the validator refuses a submitted output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ValidatorErrorPolicy {
    /// The output is refused and the stage is asked again with the reason.
    Reject,
    /// The output is taken anyway, and the refusal is a warning.
    Accept,
}

impl From<leviath_core::output::OnValidatorError> for ValidatorErrorPolicy {
    fn from(policy: leviath_core::output::OnValidatorError) -> Self {
        use leviath_core::output::OnValidatorError as Core;
        match policy {
            Core::Reject => Self::Reject,
            Core::Accept => Self::Accept,
        }
    }
}

/// One file the output is expected to carry.
///
/// A slot rather than a file: this is what the blueprint asked for, and a run's
/// `artifacts` is what it produced.
#[derive(Debug, SimpleObject)]
pub(crate) struct OutputArtifact {
    /// The name the run hands it back under.
    pub(crate) name: String,
    /// The mime type or pattern it must be.
    pub(crate) mime_type: String,
    /// Whether the output is incomplete without it.
    pub(crate) required: bool,
    /// One line on what it should contain, shown to the model.
    pub(crate) description: Option<String>,
}

/// The shape an output must take.
///
/// Declaring a shape does not by itself demand an output: that is
/// `outputRequirement` on the stage. This says what the answer has to look like
/// when it comes.
#[derive(Debug, SimpleObject)]
pub(crate) struct OutputSpec {
    /// The format asked for, such as `json` or `markdown`.
    pub(crate) format: Option<String>,
    /// What to tell the model about the answer wanted.
    pub(crate) instructions: Option<String>,
    /// An example answer, shown to the model.
    pub(crate) example: Option<String>,
    /// A JSON Schema the answer must satisfy.
    pub(crate) schema: Option<Json>,
    /// A script that checks the answer, beyond the schema.
    pub(crate) validator: Option<String>,
    /// What happens when that script refuses an answer. Null leaves the default,
    /// which is to refuse the output and ask again.
    pub(crate) on_validator_error: Option<ValidatorErrorPolicy>,
    /// Whether an artifact may replace a file of the same name in the working
    /// directory.
    pub(crate) overwrite_artifacts: Option<bool>,
    /// The files the answer is expected to carry.
    pub(crate) artifacts: Vec<OutputArtifact>,
}

impl From<&leviath_core::output::OutputSpec> for OutputSpec {
    fn from(spec: &leviath_core::output::OutputSpec) -> Self {
        Self {
            format: spec.format.clone(),
            instructions: spec.instructions.clone(),
            example: spec.example.clone(),
            schema: spec.schema.clone().map(Json),
            validator: spec.validator.clone(),
            on_validator_error: spec.on_validator_error.map(ValidatorErrorPolicy::from),
            overwrite_artifacts: spec.overwrite_artifacts,
            artifacts: spec
                .artifacts
                .iter()
                .map(|artifact| OutputArtifact {
                    name: artifact.name.clone(),
                    mime_type: artifact.mime_type.clone(),
                    required: artifact.required,
                    description: artifact.description.clone(),
                })
                .collect(),
        }
    }
}

/// What a stage takes as typed parts.
///
/// Both lists empty means the stage takes whatever the regions it sees accept,
/// which is the usual case: a stage says this only when it wants something
/// narrower or wider than its regions imply.
#[derive(Debug, SimpleObject)]
pub(crate) struct StageInput {
    /// Mime patterns this stage takes as parts.
    pub(crate) accepts: Vec<String>,
    /// Mime patterns whose parts reach the model as text whatever that model
    /// takes. For a type the registry already calls text this changes nothing.
    pub(crate) as_text: Vec<String>,
}
