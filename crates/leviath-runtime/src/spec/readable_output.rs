//! `OutputDef`'s two forms: every field in a readable format, and the binary
//! one the run file keeps.
//!
//! The binary frame has the eight fields an output shape had before it took a
//! `validator_tool`, so run files keep their layout. A validator tool rides in
//! the frame's schema document there: the document becomes an object holding
//! the tool under [`VALIDATOR_TOOL_KEY`] and the shape's own schema, if it has
//! one, under [`SCHEMA_KEY`], and both come back out when the frame is read.
//! A schema that itself holds [`VALIDATOR_TOOL_KEY`] at its top is wrapped the
//! same way, with no tool, so no schema a caller writes is ever read back as
//! one.

use leviath_core::JsonDoc;
use leviath_core::output::OnValidatorError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{WriteWith, pick};
use crate::spec::graph::{ArtifactDef, CodeRef, OutputDef, ValidatorToolDef};

/// The key a binary frame's schema document keeps a validator tool under.
pub(crate) const VALIDATOR_TOOL_KEY: &str = "leviath.validator_tool";

/// The key a wrapped schema document keeps the shape's own schema under.
pub(crate) const SCHEMA_KEY: &str = "leviath.schema";

/// An output shape in the run file's binary frames.
#[derive(Serialize, Deserialize)]
struct OutputFrame {
    format: Option<String>,
    instructions: Option<String>,
    example: Option<String>,
    schema: Option<JsonDoc>,
    validator: Option<CodeRef>,
    on_validator_error: Option<OnValidatorError>,
    overwrite_artifacts: Option<bool>,
    artifacts: Vec<ArtifactDef>,
}

/// Whether `schema` has to be wrapped to be told apart from a wrapped one.
fn holds_the_key(schema: Option<&JsonDoc>) -> bool {
    schema
        .and_then(|s| s.value().as_object())
        .is_some_and(|o| o.contains_key(VALIDATOR_TOOL_KEY))
}

impl From<&OutputDef> for OutputFrame {
    fn from(def: &OutputDef) -> Self {
        let schema = match def.validator_tool.is_some() || holds_the_key(def.schema.as_ref()) {
            false => def.schema.clone(),
            true => {
                let mut wrapped = serde_json::Map::new();
                let tool = def.validator_tool.as_ref().map_or(Value::Null, |t| {
                    serde_json::to_value(t).expect("a validator tool is plain JSON")
                });
                wrapped.insert(VALIDATOR_TOOL_KEY.to_string(), tool);
                if let Some(schema) = &def.schema {
                    wrapped.insert(SCHEMA_KEY.to_string(), schema.value().clone());
                }
                Some(JsonDoc::new(Value::Object(wrapped)))
            }
        };
        Self {
            format: def.format.clone(),
            instructions: def.instructions.clone(),
            example: def.example.clone(),
            schema,
            validator: def.validator.clone(),
            on_validator_error: def.on_validator_error,
            overwrite_artifacts: def.overwrite_artifacts,
            artifacts: def.artifacts.clone(),
        }
    }
}

impl TryFrom<OutputFrame> for OutputDef {
    type Error = String;

    fn try_from(frame: OutputFrame) -> Result<Self, String> {
        let (schema, validator_tool) = match frame.schema.map(JsonDoc::into_value) {
            Some(Value::Object(mut wrapped)) if wrapped.contains_key(VALIDATOR_TOOL_KEY) => {
                let tool = wrapped
                    .remove(VALIDATOR_TOOL_KEY)
                    .filter(|t| !t.is_null())
                    .map(serde_json::from_value::<ValidatorToolDef>)
                    .transpose()
                    .map_err(|e| format!("stored validator tool: {e}"))?;
                (wrapped.remove(SCHEMA_KEY).map(JsonDoc::new), tool)
            }
            other => (other.map(JsonDoc::new), None),
        };
        Ok(Self {
            format: frame.format,
            instructions: frame.instructions,
            example: frame.example,
            schema,
            validator: frame.validator,
            on_validator_error: frame.on_validator_error,
            validator_tool,
            overwrite_artifacts: frame.overwrite_artifacts,
            artifacts: frame.artifacts,
        })
    }
}

impl Serialize for OutputDef {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let frame: WriteWith<Self, S> = |v, s| OutputFrame::from(v).serialize(s);
        let text: WriteWith<Self, S> = |v, s| OutputDef::serialize(v, s);
        pick(s.is_human_readable(), frame, text)(self, s)
    }
}

impl<'de> Deserialize<'de> for OutputDef {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let frame: fn(D) -> Result<Self, D::Error> = |d| {
            OutputFrame::deserialize(d)
                .and_then(|f| OutputDef::try_from(f).map_err(serde::de::Error::custom))
        };
        let text: fn(D) -> Result<Self, D::Error> = |d| OutputDef::deserialize(d);
        pick(d.is_human_readable(), frame, text)(d)
    }
}
