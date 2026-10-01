//! The short forms people write in a blueprint or a JSON request.
//!
//! A few graph types are enums whose plain serde form is correct but tiring
//! to write: `{ region = { region = "task" } }` where `{ region = "task" }`
//! says the same thing. In a human-readable format (TOML, JSON) these types
//! read and write a short form instead; in the run file's binary frames they
//! keep their full tagged form. The short form is parsed straight into the
//! same typed value, so nothing downstream ever sees text.
//!
//! | Type | Short forms |
//! |---|---|
//! | `ToolSelector` | `"read_file"`, `"@all"` |
//! | `ParamScalar` | `true`, `3`, `0.9`, `"high"`, `["a", "b"]` |
//! | `Budget` | `4000`, `"10%"`, `{ percent = "10%", min = 500, max = 8000 }` |
//! | `OutputCap` | `8000`, `"40%"`, `"100% of claims"` |
//! | `InputSlot` | `"output_format"`, `{ region = "task", template = "..." }`, `{ stage_model = "plan" }` |
//! | `InputType` | `"text"`, `{ kind = "int", min = 1, max = 5 }`, `{ kind = "choice", options = ["a"] }` |
//! | `RegionKind` | `"pinned"`, `{ kind = "sliding_window", max_items = 20 }` |

use serde::{Deserialize, Serialize};

use super::graph::{
    Budget, CodeRef, Eviction, OutputCap, ParamScalar, RegionKind, ToolGroup, ToolSelector,
};
use super::inputs::{InputDecl, InputSlot, InputType, PathKind, RegionBinding, Template};
use super::names::{ChoiceName, MimePattern, RegionName, StageName, ToolName};

/// Give `$ty` a short form in readable formats. `$ty` derives its tagged form
/// with `#[serde(remote = "Self")]`; `$short` is the short form, convertible
/// both ways.
macro_rules! readable {
    ($ty:ty, $short:ty) => {
        impl Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                match s.is_human_readable() {
                    true => <$short>::from(self).serialize(s),
                    false => <$ty>::serialize(self, s),
                }
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                match d.is_human_readable() {
                    true => <$short>::deserialize(d)
                        .and_then(|short| <$ty>::try_from(short).map_err(serde::de::Error::custom)),
                    false => <$ty>::deserialize(d),
                }
            }
        }

        impl schemars::JsonSchema for $ty {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($ty).into()
            }
            fn json_schema(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
                <$short>::json_schema(g)
            }
        }
    };
}

/// A short form as written: a whole number, a name, or a table of settings.
///
/// Read with its own visitor rather than `#[serde(untagged)]`, so a mistake
/// inside a table (an unknown key, a missing one) keeps its own message
/// instead of becoming "did not match any variant".
pub(crate) enum Short<T> {
    /// A whole number.
    Int(i64),
    /// A name or other text.
    Name(String),
    /// A table.
    Table(T),
}

impl<T: Serialize> Serialize for Short<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Int(n) => s.serialize_i64(*n),
            Self::Name(t) => s.serialize_str(t),
            Self::Table(t) => t.serialize(s),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Short<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for V<T> {
            type Value = Short<T>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a number, a name or a table")
            }
            fn visit_i64<E: serde::de::Error>(self, n: i64) -> Result<Short<T>, E> {
                Ok(Short::Int(n))
            }
            fn visit_u64<E: serde::de::Error>(self, n: u64) -> Result<Short<T>, E> {
                i64::try_from(n)
                    .map(Short::Int)
                    .map_err(|_| E::custom(format!("{n} is too large")))
            }
            fn visit_str<E: serde::de::Error>(self, t: &str) -> Result<Short<T>, E> {
                Ok(Short::Name(t.to_string()))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Short<T>, A::Error> {
                T::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(Short::Table)
            }
        }
        d.deserialize_any(V(std::marker::PhantomData))
    }
}

impl<T: schemars::JsonSchema> schemars::JsonSchema for Short<T> {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        format!("Short_{}", T::schema_name()).into()
    }
    fn json_schema(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let table = g.subschema_for::<T>();
        schemars::json_schema!({ "anyOf": [{ "type": "integer" }, { "type": "string" }, table] })
    }
}

/// The table half of a [`Short`] for a type that has no table form.
#[derive(Serialize, schemars::JsonSchema)]
pub(crate) enum NoTable {}

impl<'de> Deserialize<'de> for NoTable {
    fn deserialize<D: serde::Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom(
            "this takes a number or text, not a table",
        ))
    }
}

/// A fraction as a person writes it: `0.35` is `"35%"`.
fn percent_text(fraction: f64) -> String {
    let text = format!("{:.6}", fraction * 100.0);
    let text = text.trim_end_matches('0').trim_end_matches('.');
    format!("{text}%")
}

/// `"35%"` as the fraction `0.35`.
fn parse_percent(text: &str) -> Result<f64, String> {
    let number = text
        .trim()
        .strip_suffix('%')
        .ok_or_else(|| format!("{text:?} is not a percentage such as \"35%\""))?;
    let value: f64 = number
        .trim()
        .parse()
        .map_err(|_| format!("{text:?} is not a percentage such as \"35%\""))?;
    Ok(value / 100.0)
}

// ── ToolSelector ────────────────────────────────────────────────────────────

/// A tool by name, or a group as `@all`, `@builtin`, `@subagent`, `@scripts`, `@mcp`.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub(crate) struct ToolSelectorText(String);

impl From<&ToolSelector> for ToolSelectorText {
    fn from(t: &ToolSelector) -> Self {
        Self(match t {
            ToolSelector::Tool(name) => name.to_string(),
            ToolSelector::Group(g) => g.token().to_string(),
        })
    }
}

impl TryFrom<ToolSelectorText> for ToolSelector {
    type Error = String;
    fn try_from(t: ToolSelectorText) -> Result<Self, String> {
        if ToolGroup::is_token(&t.0) {
            return ToolGroup::parse(&t.0)
                .map(ToolSelector::Group)
                .ok_or_else(|| format!("{:?} is not a tool group; the groups are @all, @builtin, @subagent, @scripts and @mcp", t.0));
        }
        ToolName::new(t.0)
            .map(ToolSelector::Tool)
            .map_err(|e| e.to_string())
    }
}

readable!(ToolSelector, ToolSelectorText);

// ── ParamScalar ─────────────────────────────────────────────────────────────

/// A model setting as written: a boolean, a number, text, or a list of text.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub(crate) enum ParamText {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    TextList(Vec<String>),
}

impl From<&ParamScalar> for ParamText {
    fn from(p: &ParamScalar) -> Self {
        match p.clone() {
            ParamScalar::Bool(b) => Self::Bool(b),
            ParamScalar::Int(i) => Self::Int(i),
            ParamScalar::Float(x) => Self::Float(x),
            ParamScalar::Text(t) => Self::Text(t),
            ParamScalar::TextList(l) => Self::TextList(l),
        }
    }
}

impl TryFrom<ParamText> for ParamScalar {
    type Error = String;
    fn try_from(p: ParamText) -> Result<Self, String> {
        Ok(match p {
            ParamText::Bool(b) => Self::Bool(b),
            ParamText::Int(i) => Self::Int(i),
            ParamText::Float(x) => Self::Float(x),
            ParamText::Text(t) => Self::Text(t),
            ParamText::TextList(l) => Self::TextList(l),
        })
    }
}

readable!(ParamScalar, ParamText);

// ── Budget ──────────────────────────────────────────────────────────────────

/// A region budget as written: tokens, `"35%"`, or a clamped percentage.
pub(crate) type BudgetText = Short<ClampedPercent>;

/// A share of the window, clamped.
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClampedPercent {
    /// The share: `"35%"`.
    percent: String,
    /// The fewest tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min: Option<u32>,
    /// The most tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max: Option<u32>,
}

/// A token count from a whole number someone wrote.
fn tokens(n: i64) -> Result<u32, String> {
    u32::try_from(n).map_err(|_| format!("{n} is not a token count"))
}

impl From<&Budget> for BudgetText {
    fn from(b: &Budget) -> Self {
        match *b {
            Budget::Tokens(n) => Self::Int(n.into()),
            Budget::Percent {
                percent,
                min: None,
                max: None,
            } => Self::Name(percent_text(percent)),
            Budget::Percent { percent, min, max } => Self::Table(ClampedPercent {
                percent: percent_text(percent),
                min,
                max,
            }),
        }
    }
}

impl TryFrom<BudgetText> for Budget {
    type Error = String;
    fn try_from(b: BudgetText) -> Result<Self, String> {
        Ok(match b {
            Short::Int(n) => Self::Tokens(tokens(n)?),
            Short::Name(p) => Self::Percent {
                percent: parse_percent(&p)?,
                min: None,
                max: None,
            },
            Short::Table(c) => Self::Percent {
                percent: parse_percent(&c.percent)?,
                min: c.min,
                max: c.max,
            },
        })
    }
}

readable!(Budget, BudgetText);

// ── OutputCap ───────────────────────────────────────────────────────────────

/// A reply cap as written: tokens, `"40%"` of the window, or `"100% of claims"`.
pub(crate) type OutputCapText = Short<NoTable>;

impl From<&OutputCap> for OutputCapText {
    fn from(c: &OutputCap) -> Self {
        match c {
            OutputCap::Tokens(n) => Self::Int((*n).into()),
            OutputCap::WindowPercent(p) => Self::Name(percent_text(*p)),
            OutputCap::RegionPercent { percent, region } => {
                Self::Name(format!("{} of {region}", percent_text(*percent)))
            }
        }
    }
}

impl TryFrom<OutputCapText> for OutputCap {
    type Error = String;
    fn try_from(c: OutputCapText) -> Result<Self, String> {
        match c {
            Short::Int(n) => Ok(Self::Tokens(tokens(n)?)),
            Short::Name(t) => match t.split_once(" of ") {
                Some((p, region)) => Ok(Self::RegionPercent {
                    percent: parse_percent(p)?,
                    region: RegionName::new(region.trim()).map_err(|e| e.to_string())?,
                }),
                None => Ok(Self::WindowPercent(parse_percent(&t)?)),
            },
            Short::Table(never) => match never {},
        }
    }
}

readable!(OutputCap, OutputCapText);

// ── InputSlot ───────────────────────────────────────────────────────────────

/// Where an input goes, as written: `"output_format"`, `"output_instructions"`,
/// or a table naming one target.
pub(crate) type SlotText = Short<SlotTable>;

#[derive(Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SlotTable {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    region: Option<RegionName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    template: Option<Template>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stage_model: Option<StageName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stage_max_iterations: Option<StageName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fan_out_max_workers: Option<StageName>,
}

impl From<&InputSlot> for SlotText {
    fn from(s: &InputSlot) -> Self {
        let table = |t: SlotTable| Short::Table(t);
        match s.clone() {
            InputSlot::OutputFormat => Short::Name("output_format".into()),
            InputSlot::OutputInstructions => Short::Name("output_instructions".into()),
            InputSlot::Region(b) => table(SlotTable {
                region: Some(b.region),
                template: b.template,
                ..SlotTable::default()
            }),
            InputSlot::StageModel(s) => table(SlotTable {
                stage_model: Some(s),
                ..SlotTable::default()
            }),
            InputSlot::StageMaxIterations(s) => table(SlotTable {
                stage_max_iterations: Some(s),
                ..SlotTable::default()
            }),
            InputSlot::FanOutMaxWorkers(s) => table(SlotTable {
                fan_out_max_workers: Some(s),
                ..SlotTable::default()
            }),
        }
    }
}

impl TryFrom<SlotText> for InputSlot {
    type Error = String;
    fn try_from(s: SlotText) -> Result<Self, String> {
        let t = match s {
            Short::Name(n) if n == "output_format" => return Ok(Self::OutputFormat),
            Short::Name(n) if n == "output_instructions" => return Ok(Self::OutputInstructions),
            Short::Int(n) => return Err(format!("{n} is not a slot")),
            Short::Name(n) => {
                return Err(format!(
                    "{n:?} is not a slot; write \"output_format\", \"output_instructions\", or a table naming a region or a stage"
                ));
            }
            Short::Table(t) => t,
        };
        if t.template.is_some() && t.region.is_none() {
            return Err("only a region slot takes a template".into());
        }
        let template = t.template;
        let mut found: Vec<Self> = [
            t.region
                .map(|region| Self::Region(RegionBinding { region, template })),
            t.stage_model.map(Self::StageModel),
            t.stage_max_iterations.map(Self::StageMaxIterations),
            t.fan_out_max_workers.map(Self::FanOutMaxWorkers),
        ]
        .into_iter()
        .flatten()
        .collect();
        match found.len() {
            1 => Ok(found.remove(0)),
            _ => Err("a slot names exactly one of region, stage_model, stage_max_iterations and fan_out_max_workers".into()),
        }
    }
}

readable!(InputSlot, SlotText);

// ── InputType ───────────────────────────────────────────────────────────────

/// An input type as written: its name alone, or a table with `kind` and its
/// settings.
pub(crate) type TypeText = Short<Box<TypeTable>>;

/// A number bound, whole or not.
#[derive(Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub(crate) enum Bound {
    Int(i64),
    Float(f64),
}

#[derive(Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypeTable {
    kind: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    multiline: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min_len: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_len: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min: Option<Bound>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max: Option<Bound>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    options: Option<Vec<ChoiceName>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    item: Option<InputType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fields: Option<Vec<InputDecl>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accepts: Option<Vec<MimePattern>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    names: Option<PathKind>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    must_exist: bool,
}

impl From<&InputType> for TypeText {
    fn from(t: &InputType) -> Self {
        let table = |kind: &str, fill: &dyn Fn(&mut TypeTable)| {
            let mut tt = TypeTable {
                kind: kind.to_string(),
                ..TypeTable::default()
            };
            fill(&mut tt);
            Self::Table(Box::new(tt))
        };
        match t.clone() {
            InputType::Text {
                multiline: false,
                min_len: None,
                max_len: None,
            } => Self::Name("text".into()),
            InputType::Text {
                multiline,
                min_len,
                max_len,
            } => table("text", &|tt| {
                tt.multiline = multiline;
                tt.min_len = min_len;
                tt.max_len = max_len;
            }),
            InputType::Bool => Self::Name("bool".into()),
            InputType::Int {
                min: None,
                max: None,
            } => Self::Name("int".into()),
            InputType::Int { min, max } => table("int", &|tt| {
                tt.min = min.map(Bound::Int);
                tt.max = max.map(Bound::Int);
            }),
            InputType::Float {
                min: None,
                max: None,
            } => Self::Name("float".into()),
            InputType::Float { min, max } => table("float", &|tt| {
                tt.min = min.map(Bound::Float);
                tt.max = max.map(Bound::Float);
            }),
            InputType::Choice { options } => {
                table("choice", &|tt| tt.options = Some(options.clone()))
            }
            InputType::List { item, min, max } => table("list", &|tt| {
                tt.item = Some((*item).clone());
                tt.min = min.map(|n| Bound::Int(n.into()));
                tt.max = max.map(|n| Bound::Int(n.into()));
            }),
            InputType::Record { fields } => table("record", &|tt| tt.fields = Some(fields.clone())),
            InputType::File { accepts } if accepts.is_empty() => Self::Name("file".into()),
            InputType::File { accepts } => table("file", &|tt| tt.accepts = Some(accepts.clone())),
            InputType::Path { kind, must_exist } => table("path", &|tt| {
                tt.names = Some(kind);
                tt.must_exist = must_exist;
            }),
            InputType::Model => Self::Name("model".into()),
            InputType::Blueprint => Self::Name("blueprint".into()),
            InputType::Duration => Self::Name("duration".into()),
            InputType::Url => Self::Name("url".into()),
        }
    }
}

const TYPE_NAMES: &str =
    "text, bool, int, float, choice, list, record, file, path, model, blueprint, duration, url";

fn whole(b: Option<Bound>, what: &str) -> Result<Option<i64>, String> {
    match b {
        None => Ok(None),
        Some(Bound::Int(i)) => Ok(Some(i)),
        Some(Bound::Float(x)) => Err(format!("`{what}` of an int is a whole number, not {x}")),
    }
}

fn count(b: Option<Bound>, what: &str) -> Result<Option<u32>, String> {
    whole(b, what)?
        .map(|n| u32::try_from(n).map_err(|_| format!("`{what}` of a list is a count, not {n}")))
        .transpose()
}

fn number(b: Option<Bound>) -> Option<f64> {
    b.map(|b| match b {
        Bound::Int(i) => i as f64,
        Bound::Float(x) => x,
    })
}

impl TryFrom<TypeText> for InputType {
    type Error = String;
    fn try_from(t: TypeText) -> Result<Self, String> {
        let tt = match t {
            Short::Int(n) => {
                return Err(format!(
                    "{n} is not an input type; the types are {TYPE_NAMES}"
                ));
            }
            Short::Name(name) => TypeTable {
                kind: name,
                ..TypeTable::default()
            },
            Short::Table(tt) => *tt,
        };
        // Each kind takes only its own settings; anything else is a mistake
        // worth naming rather than ignoring.
        let allowed: &[&str] = match tt.kind.as_str() {
            "text" => &["multiline", "min_len", "max_len"],
            "int" | "float" => &["min", "max"],
            "choice" => &["options"],
            "list" => &["item", "min", "max"],
            "record" => &["fields"],
            "file" => &["accepts"],
            "path" => &["names", "must_exist"],
            "bool" | "model" | "blueprint" | "duration" | "url" => &[],
            other => {
                return Err(format!(
                    "{other:?} is not an input type; the types are {TYPE_NAMES}"
                ));
            }
        };
        let set = [
            ("multiline", tt.multiline),
            ("min_len", tt.min_len.is_some()),
            ("max_len", tt.max_len.is_some()),
            ("min", tt.min.is_some()),
            ("max", tt.max.is_some()),
            ("options", tt.options.is_some()),
            ("item", tt.item.is_some()),
            ("fields", tt.fields.is_some()),
            ("accepts", tt.accepts.is_some()),
            ("names", tt.names.is_some()),
            ("must_exist", tt.must_exist),
        ];
        if let Some((key, _)) = set.iter().find(|(key, on)| *on && !allowed.contains(key)) {
            return Err(format!("a {} input does not take `{key}`", tt.kind));
        }
        let need = |what: &str| format!("a {} input needs `{what}`", tt.kind);
        Ok(match tt.kind.as_str() {
            "text" => Self::Text {
                multiline: tt.multiline,
                min_len: tt.min_len,
                max_len: tt.max_len,
            },
            "int" => Self::Int {
                min: whole(tt.min, "min")?,
                max: whole(tt.max, "max")?,
            },
            "float" => Self::Float {
                min: number(tt.min),
                max: number(tt.max),
            },
            "choice" => Self::Choice {
                options: tt.options.ok_or_else(|| need("options"))?,
            },
            "list" => Self::List {
                item: Box::new(tt.item.ok_or_else(|| need("item"))?),
                min: count(tt.min, "min")?,
                max: count(tt.max, "max")?,
            },
            "record" => Self::Record {
                fields: tt.fields.ok_or_else(|| need("fields"))?,
            },
            "file" => Self::File {
                accepts: tt.accepts.unwrap_or_default(),
            },
            "path" => Self::Path {
                kind: tt.names.unwrap_or(PathKind::Any),
                must_exist: tt.must_exist,
            },
            "bool" => Self::Bool,
            "model" => Self::Model,
            "blueprint" => Self::Blueprint,
            "duration" => Self::Duration,
            _ => Self::Url,
        })
    }
}

readable!(InputType, TypeText);

// ── RegionKind ──────────────────────────────────────────────────────────────

/// A region kind as written: its name alone, or a table with `kind` and its
/// settings.
pub(crate) type KindText = Short<KindTable>;

#[derive(Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KindTable {
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_items: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    eviction: Option<Eviction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    threshold_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<RegionName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_entries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code: Option<CodeRef>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pinned: bool,
}

impl From<&RegionKind> for KindText {
    fn from(k: &RegionKind) -> Self {
        let table = |kind: &str, fill: &dyn Fn(&mut KindTable)| {
            let mut kt = KindTable {
                kind: kind.to_string(),
                ..KindTable::default()
            };
            fill(&mut kt);
            Self::Table(kt)
        };
        match k.clone() {
            RegionKind::Pinned => Self::Name("pinned".into()),
            RegionKind::Temporary => Self::Name("temporary".into()),
            RegionKind::Clearable => Self::Name("clearable".into()),
            RegionKind::Checklist => Self::Name("checklist".into()),
            RegionKind::Keyed { max_entries: None } => Self::Name("keyed".into()),
            RegionKind::Compacting {
                threshold_tokens: None,
            } => Self::Name("compacting".into()),
            RegionKind::SlidingWindow {
                max_items,
                eviction,
            } => table("sliding_window", &|kt| {
                kt.max_items = Some(max_items);
                kt.eviction = (eviction != Eviction::PerItem).then_some(eviction);
            }),
            RegionKind::Compacting { threshold_tokens } => {
                table("compacting", &|kt| kt.threshold_tokens = threshold_tokens)
            }
            RegionKind::CompactHistory { source: None } => Self::Name("compact_history".into()),
            RegionKind::CompactHistory { source } => {
                table("compact_history", &|kt| kt.source = source.clone())
            }
            RegionKind::Keyed { max_entries } => table("keyed", &|kt| kt.max_entries = max_entries),
            RegionKind::Custom { code, pinned } => table("custom", &|kt| {
                kt.code = Some(code.clone());
                kt.pinned = pinned;
            }),
        }
    }
}

const KIND_NAMES: &str = "pinned, sliding_window, temporary, compacting, clearable, compact_history, keyed, checklist, custom";

impl TryFrom<KindText> for RegionKind {
    type Error = String;
    fn try_from(k: KindText) -> Result<Self, String> {
        let kt = match k {
            Short::Int(n) => {
                return Err(format!(
                    "{n} is not a region kind; the kinds are {KIND_NAMES}"
                ));
            }
            Short::Name(name) => KindTable {
                kind: name,
                ..KindTable::default()
            },
            Short::Table(kt) => kt,
        };
        let allowed: &[&str] = match kt.kind.as_str() {
            "sliding_window" => &["max_items", "eviction"],
            "compacting" => &["threshold_tokens"],
            "compact_history" => &["source"],
            "keyed" => &["max_entries"],
            "custom" => &["code", "pinned"],
            "pinned" | "temporary" | "clearable" | "checklist" => &[],
            other => {
                return Err(format!(
                    "{other:?} is not a region kind; the kinds are {KIND_NAMES}"
                ));
            }
        };
        let set = [
            ("max_items", kt.max_items.is_some()),
            ("eviction", kt.eviction.is_some()),
            ("threshold_tokens", kt.threshold_tokens.is_some()),
            ("source", kt.source.is_some()),
            ("max_entries", kt.max_entries.is_some()),
            ("code", kt.code.is_some()),
            ("pinned", kt.pinned),
        ];
        if let Some((key, _)) = set.iter().find(|(key, on)| *on && !allowed.contains(key)) {
            return Err(format!("a {} region does not take `{key}`", kt.kind));
        }
        let need = |what: &str| format!("a {} region needs `{what}`", kt.kind);
        Ok(match kt.kind.as_str() {
            "sliding_window" => Self::SlidingWindow {
                max_items: kt.max_items.ok_or_else(|| need("max_items"))?,
                eviction: kt.eviction.unwrap_or_default(),
            },
            "compacting" => Self::Compacting {
                threshold_tokens: kt.threshold_tokens,
            },
            "compact_history" => Self::CompactHistory { source: kt.source },
            "keyed" => Self::Keyed {
                max_entries: kt.max_entries,
            },
            "custom" => Self::Custom {
                code: kt.code.ok_or_else(|| need("code"))?,
                pinned: kt.pinned,
            },
            "pinned" => Self::Pinned,
            "temporary" => Self::Temporary,
            "clearable" => Self::Clearable,
            _ => Self::Checklist,
        })
    }
}

readable!(RegionKind, KindText);

#[cfg(test)]
#[path = "readable_tests.rs"]
mod tests;
