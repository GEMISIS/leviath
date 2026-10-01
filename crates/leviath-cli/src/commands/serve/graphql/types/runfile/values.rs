//! Typed inputs, read back: the inputs a graph declares and the values a run
//! was given.
//!
//! An input's type and its value are both closed sets, so each is a union with
//! one member per kind rather than a string a client has to parse. Every
//! member of the two unions answers `summary` or `asText` as well, so a client
//! that only wants to show one has a field that works whichever member came
//! back.

use async_graphql::{Enum, SimpleObject, Union};
use leviath_graphql_derive::mirror;
use leviath_runtime::spec::inputs::{
    InputDecl as CoreDecl, InputSlot as CoreSlot, InputType as CoreType, InputValue as CoreValue,
    InputValues, PathKind as CorePathKind,
};

use super::super::super::scalars::BigInt;
use super::saturating;

/// A checked input value, one member per input type.
#[derive(Debug, Union)]
pub(crate) enum InputValue {
    /// A `text` value.
    Text(TextValue),
    /// A `bool` value.
    Bool(BoolValue),
    /// An `int` value.
    Int(IntValue),
    /// A `float` value.
    Float(FloatValue),
    /// A `choice` value.
    Choice(ChoiceValue),
    /// A `list` value.
    List(ListValue),
    /// A `record` value.
    Record(RecordValue),
    /// A `file` value.
    File(FileValue),
    /// A `path` value.
    Path(PathValue),
    /// A `model` value.
    Model(ModelValue),
    /// A `blueprint` value.
    Blueprint(BlueprintValue),
    /// A `duration` value.
    Duration(DurationValue),
    /// A `url` value.
    Url(UrlValue),
}

/// A `text` input's value.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct TextValue {
    /// The text.
    pub(crate) text: String,
    /// The value as a region shows it.
    pub(crate) as_text: String,
}

/// A `bool` input's value.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct BoolValue {
    /// The value.
    pub(crate) flag: bool,
    /// The value as a region shows it.
    pub(crate) as_text: String,
}

/// An `int` input's value.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct IntValue {
    /// The value.
    pub(crate) integer: BigInt,
    /// The value as a region shows it.
    pub(crate) as_text: String,
}

/// A `float` input's value.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct FloatValue {
    /// The value.
    pub(crate) number: f64,
    /// The value as a region shows it.
    pub(crate) as_text: String,
}

/// A `choice` input's value.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ChoiceValue {
    /// The option chosen.
    pub(crate) choice: String,
    /// The value as a region shows it.
    pub(crate) as_text: String,
}

/// A `list` input's value.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ListValue {
    /// The items, in order.
    pub(crate) items: Vec<InputValue>,
    /// The value as a region shows it: one item per line.
    pub(crate) as_text: String,
}

/// A `record` input's value.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RecordValue {
    /// The fields, by name.
    pub(crate) fields: Vec<InputEntry>,
    /// The value as a region shows it: one `name: value` per line.
    pub(crate) as_text: String,
}

/// A `file` input's value.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct FileValue {
    /// The name of the attachment it names.
    pub(crate) attachment: String,
    /// The value as a region shows it.
    pub(crate) as_text: String,
}

/// A `path` input's value.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct PathValue {
    /// The path, inside the run's working directory.
    pub(crate) path: String,
    /// The value as a region shows it.
    pub(crate) as_text: String,
}

/// A `model` input's value.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ModelValue {
    /// The provider, when the value named one.
    pub(crate) provider: Option<String>,
    /// The model.
    pub(crate) model: String,
    /// The value as a region shows it.
    pub(crate) as_text: String,
}

/// A `blueprint` input's value.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct BlueprintValue {
    /// The blueprint's name.
    pub(crate) name: String,
    /// The revision it is pinned to, when it is.
    pub(crate) digest: Option<String>,
    /// The value as a region shows it.
    pub(crate) as_text: String,
}

/// A `duration` input's value.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct DurationValue {
    /// The span, in seconds.
    pub(crate) seconds: BigInt,
    /// The value as a region shows it, such as `1h30m`.
    pub(crate) as_text: String,
}

/// A `url` input's value.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct UrlValue {
    /// The URL.
    pub(crate) url: String,
    /// The value as a region shows it.
    pub(crate) as_text: String,
}

/// One named value: an input of a run, or a field of a `record` value.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct InputEntry {
    /// The input's or the field's name.
    pub(crate) name: String,
    /// Its value.
    pub(crate) value: InputValue,
}

impl From<&CoreValue> for InputValue {
    fn from(value: &CoreValue) -> Self {
        let as_text = value.render_text();
        match value {
            CoreValue::Text(text) => Self::Text(TextValue {
                text: text.clone(),
                as_text,
            }),
            CoreValue::Bool(flag) => Self::Bool(BoolValue {
                flag: *flag,
                as_text,
            }),
            CoreValue::Int(n) => Self::Int(IntValue {
                integer: BigInt(*n),
                as_text,
            }),
            CoreValue::Float(x) => Self::Float(FloatValue {
                number: *x,
                as_text,
            }),
            CoreValue::Choice(choice) => Self::Choice(ChoiceValue {
                choice: choice.to_string(),
                as_text,
            }),
            CoreValue::List(items) => Self::List(ListValue {
                items: items.iter().map(Self::from).collect(),
                as_text,
            }),
            CoreValue::Record(fields) => Self::Record(RecordValue {
                fields: fields
                    .iter()
                    .map(|(name, value)| InputEntry::new(name.as_str(), value))
                    .collect(),
                as_text,
            }),
            CoreValue::File(attachment) => Self::File(FileValue {
                attachment: attachment.clone(),
                as_text,
            }),
            CoreValue::Path(path) => Self::Path(PathValue {
                path: path.to_string(),
                as_text,
            }),
            CoreValue::Model(model) => Self::Model(ModelValue {
                provider: model.provider.as_ref().map(ToString::to_string),
                model: model.model.to_string(),
                as_text,
            }),
            CoreValue::Blueprint(blueprint) => Self::Blueprint(BlueprintValue {
                name: blueprint.name.to_string(),
                digest: blueprint.digest.as_ref().map(ToString::to_string),
                as_text,
            }),
            CoreValue::Duration(seconds) => Self::Duration(DurationValue {
                seconds: BigInt(i64::try_from(*seconds).unwrap_or(i64::MAX)),
                as_text,
            }),
            CoreValue::Url(url) => Self::Url(UrlValue {
                url: url.to_string(),
                as_text,
            }),
        }
    }
}

impl InputEntry {
    /// One named value.
    pub(crate) fn new(name: &str, value: &CoreValue) -> Self {
        Self {
            name: name.to_string(),
            value: InputValue::from(value),
        }
    }
}

/// Every checked input of a run, by name.
pub(crate) fn entries(values: &InputValues) -> Vec<InputEntry> {
    values
        .0
        .iter()
        .map(|(name, value)| InputEntry::new(name.as_str(), value))
        .collect()
}

/// What kind of path a `path` input names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum InputPathKind {
    /// A file.
    File,
    /// A directory.
    Dir,
    /// Either.
    Any,
}

impl From<CorePathKind> for InputPathKind {
    fn from(kind: CorePathKind) -> Self {
        match kind {
            CorePathKind::File => Self::File,
            CorePathKind::Dir => Self::Dir,
            CorePathKind::Any => Self::Any,
        }
    }
}

/// The type of a declared input, one member per type. Each member's
/// `summary` says in words what values it takes.
#[derive(Debug, Union)]
pub(crate) enum InputType {
    /// Text.
    Text(TextInputType),
    /// `true` or `false`.
    Bool(BoolInputType),
    /// A whole number.
    Int(IntInputType),
    /// A number.
    Float(FloatInputType),
    /// One of a fixed set of options.
    Choice(ChoiceInputType),
    /// A list of values of one type.
    List(ListInputType),
    /// A fixed set of named, typed fields.
    Record(RecordInputType),
    /// A file the caller attaches.
    File(FileInputType),
    /// A path inside the run's working directory.
    Path(PathInputType),
    /// A model.
    Model(ModelInputType),
    /// An installed blueprint.
    Blueprint(BlueprintInputType),
    /// A span of time.
    Duration(DurationInputType),
    /// An http or https URL.
    Url(UrlInputType),
}

/// A `text` input: text, optionally bounded in length.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct TextInputType {
    /// Whether the text is expected to span lines, so a form shows a text area.
    pub(crate) multiline: bool,
    /// The fewest characters allowed.
    pub(crate) min_length: Option<i32>,
    /// The most characters allowed.
    pub(crate) max_length: Option<i32>,
    /// What values it takes, in words.
    pub(crate) summary: String,
}

/// A `bool` input.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct BoolInputType {
    /// What values it takes, in words.
    pub(crate) summary: String,
}

/// An `int` input: a whole number, optionally bounded (inclusive).
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct IntInputType {
    /// The smallest allowed.
    pub(crate) min: Option<BigInt>,
    /// The largest allowed.
    pub(crate) max: Option<BigInt>,
    /// What values it takes, in words.
    pub(crate) summary: String,
}

/// A `float` input: a number, optionally bounded (inclusive). A whole number
/// is accepted.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct FloatInputType {
    /// The smallest allowed.
    pub(crate) min_value: Option<f64>,
    /// The largest allowed.
    pub(crate) max_value: Option<f64>,
    /// What values it takes, in words.
    pub(crate) summary: String,
}

/// A `choice` input: one of a fixed set of options.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ChoiceInputType {
    /// The options, in the order a form lists them.
    pub(crate) options: Vec<String>,
    /// What values it takes, in words.
    pub(crate) summary: String,
}

/// A `list` input: values of one type, optionally bounded in length.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ListInputType {
    /// The type of every item.
    pub(crate) item: Box<InputType>,
    /// The fewest items allowed.
    pub(crate) min_items: Option<i32>,
    /// The most items allowed.
    pub(crate) max_items: Option<i32>,
    /// What values it takes, in words.
    pub(crate) summary: String,
}

/// A `record` input: a fixed set of named, typed fields.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RecordInputType {
    /// The fields. A record is placed whole, so their `binds` are empty.
    pub(crate) fields: Vec<InputDecl>,
    /// What values it takes, in words.
    pub(crate) summary: String,
}

/// A `file` input: the name of an attachment on the request.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct FileInputType {
    /// The mime types accepted. Empty accepts any.
    pub(crate) accepts: Vec<String>,
    /// What values it takes, in words.
    pub(crate) summary: String,
}

/// A `path` input: a path inside the run's working directory.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct PathInputType {
    /// What the path must name.
    pub(crate) kind: InputPathKind,
    /// Whether it must exist when the run is resolved.
    pub(crate) must_exist: bool,
    /// What values it takes, in words.
    pub(crate) summary: String,
}

/// A `model` input: `provider/model` or a bare model id.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ModelInputType {
    /// What values it takes, in words.
    pub(crate) summary: String,
}

/// A `blueprint` input: an installed blueprint, as `name` or `name@digest`.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct BlueprintInputType {
    /// What values it takes, in words.
    pub(crate) summary: String,
}

/// A `duration` input: `90s`, `5m`, `2h`, `1d` or a sum such as `1h30m`.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct DurationInputType {
    /// What values it takes, in words.
    pub(crate) summary: String,
}

/// A `url` input: an http or https URL.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct UrlInputType {
    /// What values it takes, in words.
    pub(crate) summary: String,
}

/// A bound in an input type, as a GraphQL `Int`.
fn bound(n: Option<u32>) -> Option<i32> {
    n.map(saturating)
}

impl From<&CoreType> for InputType {
    fn from(ty: &CoreType) -> Self {
        let summary = ty.describe();
        match ty {
            CoreType::Text {
                multiline,
                min_len,
                max_len,
            } => Self::Text(TextInputType {
                multiline: *multiline,
                min_length: bound(*min_len),
                max_length: bound(*max_len),
                summary,
            }),
            CoreType::Bool => Self::Bool(BoolInputType { summary }),
            CoreType::Int { min, max } => Self::Int(IntInputType {
                min: min.map(BigInt),
                max: max.map(BigInt),
                summary,
            }),
            CoreType::Float { min, max } => Self::Float(FloatInputType {
                min_value: *min,
                max_value: *max,
                summary,
            }),
            CoreType::Choice { options } => Self::Choice(ChoiceInputType {
                options: options.iter().map(ToString::to_string).collect(),
                summary,
            }),
            CoreType::List { item, min, max } => Self::List(ListInputType {
                item: Box::new(Self::from(item.as_ref())),
                min_items: bound(*min),
                max_items: bound(*max),
                summary,
            }),
            CoreType::Record { fields } => Self::Record(RecordInputType {
                fields: fields.iter().map(InputDecl::from).collect(),
                summary,
            }),
            CoreType::File { accepts } => Self::File(FileInputType {
                accepts: accepts.iter().map(ToString::to_string).collect(),
                summary,
            }),
            CoreType::Path { kind, must_exist } => Self::Path(PathInputType {
                kind: InputPathKind::from(*kind),
                must_exist: *must_exist,
                summary,
            }),
            CoreType::Model => Self::Model(ModelInputType { summary }),
            CoreType::Blueprint => Self::Blueprint(BlueprintInputType { summary }),
            CoreType::Duration => Self::Duration(DurationInputType { summary }),
            CoreType::Url => Self::Url(UrlInputType { summary }),
        }
    }
}

/// Where an input's value goes in the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum InputSlotKind {
    /// Into a context region, as text, at spawn.
    Region,
    /// A stage's model.
    StageModel,
    /// A stage's iteration cap.
    StageMaxIterations,
    /// A fan-out stage's worker cap.
    FanOutMaxWorkers,
    /// The run's output format.
    OutputFormat,
    /// The run's output instructions.
    OutputInstructions,
}

/// One place an input's value goes.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct InputSlot {
    /// What the value sets.
    pub(crate) kind: InputSlotKind,
    /// The region a `REGION` slot fills.
    pub(crate) region: Option<String>,
    /// The text a `REGION` slot seeds, with `{name}` standing for an input's
    /// value. Null seeds the value alone.
    pub(crate) template: Option<String>,
    /// The stage a `STAGE_MODEL`, `STAGE_MAX_ITERATIONS` or
    /// `FAN_OUT_MAX_WORKERS` slot sets.
    pub(crate) stage: Option<String>,
}

impl From<&CoreSlot> for InputSlot {
    fn from(slot: &CoreSlot) -> Self {
        let staged = |kind, stage: &leviath_runtime::spec::names::StageName| Self {
            kind,
            region: None,
            template: None,
            stage: Some(stage.to_string()),
        };
        let bare = |kind| Self {
            kind,
            region: None,
            template: None,
            stage: None,
        };
        match slot {
            CoreSlot::Region(binding) => Self {
                kind: InputSlotKind::Region,
                region: Some(binding.region.to_string()),
                template: binding.template.as_ref().map(ToString::to_string),
                stage: None,
            },
            CoreSlot::StageModel(stage) => staged(InputSlotKind::StageModel, stage),
            CoreSlot::StageMaxIterations(stage) => staged(InputSlotKind::StageMaxIterations, stage),
            CoreSlot::FanOutMaxWorkers(stage) => staged(InputSlotKind::FanOutMaxWorkers, stage),
            CoreSlot::OutputFormat => bare(InputSlotKind::OutputFormat),
            CoreSlot::OutputInstructions => bare(InputSlotKind::OutputInstructions),
        }
    }
}

/// An input a run graph declares: what a spawn of it may, or must, supply.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct InputDecl {
    /// The input's name, as a request's `inputs` names it.
    pub(crate) name: String,
    /// Its type.
    #[graphql(name = "type")]
    pub(crate) ty: InputType,
    /// Whether a spawn must supply it. An input with a `default` is never
    /// missing.
    pub(crate) required: bool,
    /// The value used when a spawn does not supply one.
    pub(crate) default: Option<InputValue>,
    /// What the input is for, as a form or an agent reads it.
    pub(crate) description: Option<String>,
    /// Where the value goes.
    pub(crate) binds: Vec<InputSlot>,
}

impl From<&CoreDecl> for InputDecl {
    fn from(decl: &CoreDecl) -> Self {
        Self {
            name: decl.name.to_string(),
            ty: InputType::from(&decl.ty),
            required: decl.required,
            default: decl.default.as_ref().map(InputValue::from),
            description: decl.description.clone(),
            binds: decl.binds.iter().map(InputSlot::from).collect(),
        }
    }
}
