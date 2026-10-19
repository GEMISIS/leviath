//! `StageMode`'s two forms: the short one a blueprint writes, and the binary
//! one the run file keeps.

use serde::{Deserialize, Serialize};

use super::{Short, WriteWith, names_schema, pick};
use crate::spec::graph::{FanOutDef, InteractionPointDef, InteractiveDef, StageMode};

/// A stage mode in the run file's binary frames.
///
/// Its variants keep the order and shape `StageMode` has always been written
/// in: an interactive stage with no settings is the bare `Interactive` it
/// always was, so a file written before the mode took settings reads the
/// same. One with settings is the variant added at the end.
#[derive(Serialize, Deserialize)]
enum ModeFrame {
    Autonomous,
    Interactive,
    InteractivePoints(Vec<InteractionPointDef>),
    FanOut(FanOutDef),
    Output,
    InteractiveSettings(InteractiveDef),
}

impl From<StageMode> for ModeFrame {
    fn from(mode: StageMode) -> Self {
        match mode {
            StageMode::Autonomous => Self::Autonomous,
            StageMode::Interactive(def) if def == InteractiveDef::default() => Self::Interactive,
            StageMode::Interactive(def) => Self::InteractiveSettings(def),
            StageMode::InteractivePoints(points) => Self::InteractivePoints(points),
            StageMode::FanOut(fan) => Self::FanOut(fan),
            StageMode::Output => Self::Output,
        }
    }
}

impl From<ModeFrame> for StageMode {
    fn from(frame: ModeFrame) -> Self {
        match frame {
            ModeFrame::Autonomous => Self::Autonomous,
            ModeFrame::Interactive => Self::Interactive(InteractiveDef::default()),
            ModeFrame::InteractiveSettings(def) => Self::Interactive(def),
            ModeFrame::InteractivePoints(points) => Self::InteractivePoints(points),
            ModeFrame::FanOut(fan) => Self::FanOut(fan),
            ModeFrame::Output => Self::Output,
        }
    }
}

/// A stage mode as written: its name alone, or a table naming the mode over
/// its settings.
pub(crate) type ModeText = Short<ModeTable>;

/// The table form of a stage mode: one key, the mode's name, over its
/// settings.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModeTable {
    /// The model and a person take turns, with settings. After a reply with
    /// no tool calls the stage waits for the next message; it is given the
    /// `end_conversation` tool that moves it on.
    Interactive(InteractiveDef),
    /// The model works on its own, stopping at declared points for a person.
    InteractivePoints(Vec<InteractionPointDef>),
    /// The stage splits its work over worker runs. It is given the
    /// `fan_out` tool it starts them with, whether or not its `tools` name it.
    FanOut(FanOutDef),
}

/// The modes whose name alone is a mode.
const MODE_NAMES_ALONE: &str = "autonomous, interactive, output";

impl From<&StageMode> for ModeText {
    fn from(mode: &StageMode) -> Self {
        match mode.clone() {
            StageMode::Autonomous => Self::Name("autonomous".into()),
            StageMode::Output => Self::Name("output".into()),
            StageMode::Interactive(def) if def == InteractiveDef::default() => {
                Self::Name("interactive".into())
            }
            StageMode::Interactive(def) => Self::Table(ModeTable::Interactive(def)),
            StageMode::InteractivePoints(points) => {
                Self::Table(ModeTable::InteractivePoints(points))
            }
            StageMode::FanOut(fan) => Self::Table(ModeTable::FanOut(fan)),
        }
    }
}

impl TryFrom<ModeText> for StageMode {
    type Error = String;
    fn try_from(text: ModeText) -> Result<Self, String> {
        let name = match text {
            Short::Table(ModeTable::Interactive(def)) => return Ok(Self::Interactive(def)),
            Short::Table(ModeTable::InteractivePoints(p)) => return Ok(Self::InteractivePoints(p)),
            Short::Table(ModeTable::FanOut(fan)) => return Ok(Self::FanOut(fan)),
            Short::Int(n) => n.to_string(),
            Short::Name(name) => name,
        };
        match name.as_str() {
            "autonomous" => Ok(Self::Autonomous),
            "interactive" => Ok(Self::Interactive(InteractiveDef::default())),
            "output" => Ok(Self::Output),
            "interactive_points" | "fan_out" => Err(format!(
                "a {name} mode needs its settings, written as a table under `{name}`"
            )),
            other => Err(format!(
                "{other:?} is not a stage mode; the modes are {MODE_NAMES_ALONE}, \
                 interactive_points and fan_out"
            )),
        }
    }
}

short_schema!(ModeTable, "ModeTable", |g| vec![
    names_schema(MODE_NAMES_ALONE),
    g.subschema_for::<ModeTable>().into(),
]);

impl Serialize for StageMode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let frame: WriteWith<Self, S> = |v, s| ModeFrame::from(v.clone()).serialize(s);
        let short: WriteWith<Self, S> = |v, s| ModeText::from(v).serialize(s);
        pick(s.is_human_readable(), frame, short)(self, s)
    }
}

impl<'de> Deserialize<'de> for StageMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let frame: fn(D) -> Result<Self, D::Error> = |d| ModeFrame::deserialize(d).map(Self::from);
        let short: fn(D) -> Result<Self, D::Error> = |d| {
            ModeText::deserialize(d)
                .and_then(|text| Self::try_from(text).map_err(serde::de::Error::custom))
        };
        pick(d.is_human_readable(), frame, short)(d)
    }
}

impl schemars::JsonSchema for StageMode {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "StageMode".into()
    }
    fn json_schema(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        ModeText::json_schema(g)
    }
}
