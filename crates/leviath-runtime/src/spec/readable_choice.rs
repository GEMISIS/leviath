//! `ModelChoice`'s two forms: every field in a readable format, and the
//! binary one the run file keeps.
//!
//! The binary frame has the four fields a stage's model block had before it
//! took an `effort`, so run files keep their layout. A stage's effort rides
//! in its parameters there, under [`EFFORT_KEY`], a key a readable spec may
//! not use, and comes back out when the frame is read.

use serde::{Deserialize, Serialize};

use super::{WriteWith, pick};
use crate::spec::graph::{ModelChoice, ModelParams, ParamScalar};
use crate::spec::names::ModelRef;

/// The parameter a binary frame keeps a stage's effort under.
pub(crate) const EFFORT_KEY: &str = "leviath.effort";

/// A stage's model block in the run file's binary frames.
#[derive(Serialize, Deserialize)]
struct ChoiceFrame {
    models: Vec<ModelRef>,
    allow_user_default: bool,
    params: ModelParams,
    request_timeout_secs: Option<u64>,
}

impl From<&ModelChoice> for ChoiceFrame {
    fn from(choice: &ModelChoice) -> Self {
        let mut params = choice.params.clone();
        params.extra.extend(
            choice
                .effort
                .map(|e| (EFFORT_KEY.to_string(), ParamScalar::Text(e.to_string()))),
        );
        Self {
            models: choice.models.clone(),
            allow_user_default: choice.allow_user_default,
            params,
            request_timeout_secs: choice.request_timeout_secs,
        }
    }
}

impl From<ChoiceFrame> for ModelChoice {
    fn from(mut frame: ChoiceFrame) -> Self {
        let effort = match frame.params.extra.remove(EFFORT_KEY) {
            Some(ParamScalar::Text(text)) => leviath_core::Effort::parse(&text),
            _ => None,
        };
        Self {
            models: frame.models,
            allow_user_default: frame.allow_user_default,
            params: frame.params,
            request_timeout_secs: frame.request_timeout_secs,
            effort,
        }
    }
}

/// A readable model block, refused when its parameters use [`EFFORT_KEY`].
fn readable(choice: ModelChoice) -> Result<ModelChoice, String> {
    match choice.params.extra.contains_key(EFFORT_KEY) {
        true => Err(format!(
            "`{EFFORT_KEY}` is not a parameter a stage may set; write `effort` on the model block"
        )),
        false => Ok(choice),
    }
}

impl Serialize for ModelChoice {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let frame: WriteWith<Self, S> = |v, s| ChoiceFrame::from(v).serialize(s);
        let text: WriteWith<Self, S> = |v, s| ModelChoice::serialize(v, s);
        pick(s.is_human_readable(), frame, text)(self, s)
    }
}

impl<'de> Deserialize<'de> for ModelChoice {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let frame: fn(D) -> Result<Self, D::Error> =
            |d| ChoiceFrame::deserialize(d).map(Self::from);
        let text: fn(D) -> Result<Self, D::Error> = |d| {
            ModelChoice::deserialize(d)
                .and_then(|choice| readable(choice).map_err(serde::de::Error::custom))
        };
        pick(d.is_human_readable(), frame, text)(d)
    }
}
