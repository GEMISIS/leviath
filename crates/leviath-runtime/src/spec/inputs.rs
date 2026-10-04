//! Typed run inputs: what a run graph asks for, and the checked values a
//! spawn supplies.
//!
//! A graph declares each input with an [`InputType`]. A request arrives with
//! [`RawInput`]s, which say only what the wire could say (a number, some text,
//! a list). [`check_inputs`] decodes each raw value through its declared type
//! into an [`InputValue`], and from then on nothing downstream handles an
//! untyped value. A declaration also says where its value goes, through
//! [`InputSlot`]s, which is the only way a caller changes a blueprint's graph.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use super::names::{
    BlueprintRef, ChoiceName, HttpUrl, InputName, MimePattern, ModelRef, RegionName, StageName,
    WorkdirPath,
};

/// What kind of path a [`InputType::Path`] input names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    /// A file.
    File,
    /// A directory.
    Dir,
    /// Either.
    Any,
}

/// The type of an input. Closed: a value is one of these or it is refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(remote = "Self", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputType {
    /// Text, optionally bounded in length (in characters).
    Text {
        /// Whether the text is expected to span lines (a form shows a text area).
        #[serde(default)]
        multiline: bool,
        /// The fewest characters allowed.
        #[serde(default)]
        min_len: Option<u32>,
        /// The most characters allowed.
        #[serde(default)]
        max_len: Option<u32>,
    },
    /// `true` or `false`.
    Bool,
    /// A whole number, optionally bounded (inclusive).
    Int {
        /// The smallest allowed.
        #[serde(default)]
        min: Option<i64>,
        /// The largest allowed.
        #[serde(default)]
        max: Option<i64>,
    },
    /// A number, optionally bounded (inclusive). A whole number is accepted.
    Float {
        /// The smallest allowed.
        #[serde(default)]
        min: Option<f64>,
        /// The largest allowed.
        #[serde(default)]
        max: Option<f64>,
    },
    /// One of a fixed set of options.
    Choice {
        /// The options, in the order a form lists them.
        options: Vec<ChoiceName>,
    },
    /// A list of values of one type, optionally bounded in length.
    List {
        /// The type of every item.
        item: Box<InputType>,
        /// The fewest items allowed.
        #[serde(default)]
        min: Option<u32>,
        /// The most items allowed.
        #[serde(default)]
        max: Option<u32>,
    },
    /// A fixed set of named fields, each typed.
    Record {
        /// The fields. Their `binds` are ignored; a record is placed whole.
        fields: Vec<InputDecl>,
    },
    /// A file the caller attaches, named by the attachment's name.
    File {
        /// The mime types accepted. Empty accepts any. Checked when the run
        /// is resolved, once the file's type is known.
        #[serde(default)]
        accepts: Vec<MimePattern>,
    },
    /// A path inside the run's workdir.
    Path {
        /// What the path must name.
        kind: PathKind,
        /// Whether it must exist when the run is resolved.
        #[serde(default)]
        must_exist: bool,
    },
    /// A model, as `provider/model` or a bare model id.
    Model,
    /// An installed blueprint, as `name` or `name@digest`.
    Blueprint,
    /// A span of time, as `90s`, `5m`, `2h`, `1d` or a sum such as `1h30m`.
    Duration,
    /// An http or https URL.
    Url,
}

/// A declared input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputDecl {
    /// The input's name, as a caller writes it.
    pub name: InputName,
    /// Its type.
    #[serde(rename = "type")]
    pub ty: InputType,
    /// Whether a spawn must supply it. An input with a `default` is never
    /// missing.
    #[serde(default)]
    pub required: bool,
    /// The value used when a spawn does not supply one.
    #[serde(default)]
    pub default: Option<InputValue>,
    /// What the input is for, as a form or an agent reads it.
    #[serde(default)]
    pub description: Option<String>,
    /// Where the value goes.
    #[serde(default)]
    pub binds: Vec<InputSlot>,
}

/// Where an input's value goes in the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self", rename_all = "snake_case")]
pub enum InputSlot {
    /// Into a context region, as text, at spawn.
    Region(RegionBinding),
    /// A stage's model. Takes a `model` input.
    StageModel(StageName),
    /// A stage's iteration cap. Takes an `int` input of at least 1.
    StageMaxIterations(StageName),
    /// A fan-out stage's worker cap. Takes an `int` input of at least 1.
    FanOutMaxWorkers(StageName),
    /// The run's output format. Takes a `text` or `choice` input.
    OutputFormat,
    /// The run's output instructions. Takes a `text` input.
    OutputInstructions,
}

/// A region an input fills, and how its value reads there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegionBinding {
    /// The region.
    pub region: RegionName,
    /// The text to seed, with `{name}` standing for an input's value. `None`
    /// seeds this input's value alone.
    #[serde(default)]
    pub template: Option<Template>,
}

/// A value as the wire carries it, before its declared type is known.
///
/// JSON, TOML and GraphQL can each say these much and no more, so they are
/// what every front door decodes into. [`check_inputs`] turns them into
/// [`InputValue`]s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum RawInput {
    /// `true` or `false`.
    Bool(bool),
    /// A whole number.
    Int(i64),
    /// A number with a fraction.
    Float(f64),
    /// Text.
    Text(String),
    /// A list.
    List(Vec<RawInput>),
    /// A table of named values.
    Record(BTreeMap<String, RawInput>),
}

impl RawInput {
    /// The value as a number, when it is one; `NaN` otherwise, which no
    /// bound accepts.
    fn as_float(&self) -> f64 {
        match self {
            Self::Int(n) => *n as f64,
            Self::Float(x) => *x,
            _ => f64::NAN,
        }
    }

    /// How this value reads in an issue: `text "five"`, `a list of 3`.
    pub fn describe(&self) -> String {
        match self {
            Self::Bool(b) => format!("the boolean {b}"),
            Self::Int(n) => format!("the integer {n}"),
            Self::Float(x) => format!("the number {x}"),
            Self::Text(t) => format!("text {:?}", clip(t)),
            Self::List(items) => format!("a list of {}", items.len()),
            Self::Record(fields) => format!("a table with {} field(s)", fields.len()),
        }
    }
}

fn clip(text: &str) -> String {
    let head: String = text.chars().take(40).collect();
    match head.len() < text.len() {
        true => format!("{head}..."),
        false => head,
    }
}

/// A checked input value. Each variant matches one [`InputType`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputValue {
    /// A `text` value.
    Text(String),
    /// A `bool` value.
    Bool(bool),
    /// An `int` value.
    Int(i64),
    /// A `float` value.
    Float(f64),
    /// A `choice` value.
    Choice(ChoiceName),
    /// A `list` value.
    List(Vec<InputValue>),
    /// A `record` value.
    Record(BTreeMap<InputName, InputValue>),
    /// A `file` value: the name of an attachment on the request.
    File(String),
    /// A `path` value.
    Path(WorkdirPath),
    /// A `model` value.
    Model(ModelRef),
    /// A `blueprint` value.
    Blueprint(BlueprintRef),
    /// A `duration` value, in seconds.
    Duration(u64),
    /// A `url` value.
    Url(HttpUrl),
}

impl InputValue {
    /// The value as text in a context region.
    ///
    /// Lists render one item per line after `- `, records one `name: value`
    /// per line, and a file renders as its name (its bytes reach the region as
    /// a part beside the text).
    pub fn render_text(&self) -> String {
        match self {
            Self::Text(t) | Self::File(t) => t.clone(),
            Self::Bool(b) => b.to_string(),
            Self::Int(n) => n.to_string(),
            Self::Float(x) => x.to_string(),
            Self::Choice(c) => c.to_string(),
            Self::List(items) => items
                .iter()
                .map(|i| format!("- {}", i.render_text()))
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Record(fields) => fields
                .iter()
                .map(|(k, v)| format!("{k}: {}", v.render_text()))
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Path(p) => p.to_string(),
            Self::Model(m) => m.to_string(),
            Self::Blueprint(b) => b.to_string(),
            Self::Duration(secs) => exact_duration(*secs),
            Self::Url(u) => u.to_string(),
        }
    }

    /// The value as the wire would carry it, so a declared default goes
    /// through the same check as a supplied value.
    pub fn to_raw(&self) -> RawInput {
        match self {
            Self::Text(t) | Self::File(t) => RawInput::Text(t.clone()),
            Self::Bool(b) => RawInput::Bool(*b),
            Self::Int(n) => RawInput::Int(*n),
            Self::Float(x) => RawInput::Float(*x),
            Self::Choice(c) => RawInput::Text(c.to_string()),
            Self::List(items) => RawInput::List(items.iter().map(Self::to_raw).collect()),
            Self::Record(fields) => RawInput::Record(
                fields
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_raw()))
                    .collect(),
            ),
            Self::Path(p) => RawInput::Text(p.to_string()),
            Self::Model(m) => RawInput::Text(m.to_string()),
            Self::Blueprint(b) => RawInput::Text(b.to_string()),
            Self::Duration(secs) => RawInput::Text(format!("{secs}s")),
            Self::Url(u) => RawInput::Text(u.to_string()),
        }
    }
}

/// The checked inputs of one run, by name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InputValues(pub BTreeMap<InputName, InputValue>);

impl InputValues {
    /// One input's value.
    pub fn get(&self, name: &str) -> Option<&InputValue> {
        self.0.get(name)
    }
}

/// Text with `{input}` placeholders. `{{` and `}}` write a literal brace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct Template {
    source: String,
    parts: Vec<TemplatePart>,
}

/// One piece of a [`Template`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplatePart {
    /// Literal text.
    Text(String),
    /// An input's value.
    Input(InputName),
}

impl Template {
    /// Parse template text.
    pub fn parse(source: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut chars = source.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    text.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    text.push('}');
                }
                '{' => {
                    let name: String = chars.by_ref().take_while(|&c| c != '}').collect();
                    let name = InputName::new(name.trim()).map_err(|e| e.to_string())?;
                    if !text.is_empty() {
                        parts.push(TemplatePart::Text(std::mem::take(&mut text)));
                    }
                    parts.push(TemplatePart::Input(name));
                }
                '}' => return Err("a lone `}` must be written `}}`".to_string()),
                c => text.push(c),
            }
        }
        if !text.is_empty() {
            parts.push(TemplatePart::Text(text));
        }
        Ok(Self {
            source: source.to_string(),
            parts,
        })
    }

    /// The inputs the template names.
    pub fn inputs(&self) -> impl Iterator<Item = &InputName> {
        self.parts.iter().filter_map(|p| match p {
            TemplatePart::Input(name) => Some(name),
            TemplatePart::Text(_) => None,
        })
    }

    /// Fill in the placeholders. An input with no value renders as nothing.
    pub fn render(&self, values: &InputValues) -> String {
        self.parts
            .iter()
            .map(|p| match p {
                TemplatePart::Text(t) => t.clone(),
                TemplatePart::Input(name) => values
                    .get(name.as_str())
                    .map(InputValue::render_text)
                    .unwrap_or_default(),
            })
            .collect()
    }
}

impl TryFrom<String> for Template {
    type Error = String;
    fn try_from(source: String) -> Result<Self, String> {
        Self::parse(&source)
    }
}

impl From<Template> for String {
    fn from(t: Template) -> String {
        t.source
    }
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

/// What a check can see besides the value: the request's attachment names,
/// so a `file` input can be matched to one.
#[derive(Debug, Clone, Copy, Default)]
pub struct CheckCtx<'a> {
    /// The names of the files attached to the request.
    pub attachments: &'a [String],
}

impl InputType {
    /// The type as an issue's `expected` reads: `an integer from 1 to 5`.
    pub fn describe(&self) -> String {
        match self {
            Self::Text {
                min_len, max_len, ..
            } => {
                format!("text{}", bounds(" of ", " characters", min_len, max_len))
            }
            Self::Bool => "true or false".to_string(),
            Self::Int { min, max } => format!("an integer{}", bounds(" ", "", min, max)),
            Self::Float { min, max } => format!("a number{}", bounds(" ", "", min, max)),
            Self::Choice { options } => {
                let list: Vec<String> = options.iter().map(|o| format!("\"{o}\"")).collect();
                format!("one of {}", list.join(", "))
            }
            Self::List { item, min, max } => {
                format!(
                    "a list{} of {}",
                    bounds(" of ", " items", min, max),
                    item.describe()
                )
            }
            Self::Record { fields } => {
                let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
                format!("a table with fields {}", names.join(", "))
            }
            Self::File { accepts } if accepts.is_empty() => {
                "the name of an attached file".to_string()
            }
            Self::File { accepts } => {
                let list: Vec<&str> = accepts.iter().map(MimePattern::as_str).collect();
                format!("the name of an attached file of type {}", list.join(" or "))
            }
            Self::Path { kind, .. } => {
                let what = match kind {
                    PathKind::File => "a file",
                    PathKind::Dir => "a directory",
                    PathKind::Any => "a path",
                };
                format!("{what} inside the workdir")
            }
            Self::Model => "a model, as provider/model or a model id".to_string(),
            Self::Blueprint => "an installed blueprint's name".to_string(),
            Self::Duration => "a duration such as 90s, 5m or 1h30m".to_string(),
            Self::Url => "an http or https URL".to_string(),
        }
    }

    /// Decode `raw` as this type. Every problem is recorded in `issues`; the
    /// value comes back only when there were none.
    pub fn check(
        &self,
        raw: &RawInput,
        path: &SpecPath,
        cx: &CheckCtx<'_>,
        issues: &mut SpawnIssues,
    ) -> Option<InputValue> {
        let fail =
            |issues: &mut SpawnIssues, code: IssueCode, message: String| -> Option<InputValue> {
                issues.push(
                    SpawnIssue::new(path.clone(), code, message)
                        .expected(self.describe())
                        .got(raw.describe()),
                );
                None
            };
        let wrong = |issues: &mut SpawnIssues| {
            fail(
                issues,
                IssueCode::WrongType,
                "the value has the wrong type".to_string(),
            )
        };
        let range = |issues: &mut SpawnIssues, message: String| {
            fail(issues, IssueCode::OutOfRange, message)
        };
        let named =
            |issues: &mut SpawnIssues, message: String| fail(issues, IssueCode::Invalid, message);
        match (self, raw) {
            (
                Self::Text {
                    min_len, max_len, ..
                },
                RawInput::Text(t),
            ) => {
                let n = t.chars().count() as u64;
                match within(n, min_len.map(u64::from), max_len.map(u64::from)) {
                    true => Some(InputValue::Text(t.clone())),
                    false => range(issues, format!("the text is {n} characters long")),
                }
            }
            (Self::Bool, RawInput::Bool(b)) => Some(InputValue::Bool(*b)),
            (Self::Int { min, max }, RawInput::Int(n)) => {
                match min.is_none_or(|m| *n >= m) && max.is_none_or(|m| *n <= m) {
                    true => Some(InputValue::Int(*n)),
                    false => range(issues, format!("{n} is out of range")),
                }
            }
            (Self::Float { min, max }, RawInput::Int(_) | RawInput::Float(_)) => {
                let x = raw.as_float();
                match x.is_finite() && min.is_none_or(|m| x >= m) && max.is_none_or(|m| x <= m) {
                    true => Some(InputValue::Float(x)),
                    false => range(issues, format!("{x} is out of range")),
                }
            }
            (Self::Choice { options }, RawInput::Text(t)) => {
                match options.iter().find(|o| o.as_str() == t) {
                    Some(o) => Some(InputValue::Choice(o.clone())),
                    None => {
                        issues.push(
                            SpawnIssue::new(
                                path.clone(),
                                IssueCode::Invalid,
                                "not one of the options",
                            )
                            .got(raw.describe())
                            .known(options),
                        );
                        None
                    }
                }
            }
            (Self::List { item, min, max }, RawInput::List(items)) => {
                let before = issues.len();
                let n = items.len() as u64;
                if !within(n, min.map(u64::from), max.map(u64::from)) {
                    range(issues, format!("the list has {n} items"));
                }
                let values: Vec<Option<InputValue>> = items
                    .iter()
                    .enumerate()
                    .map(|(i, raw)| item.check(raw, &path.index(i), cx, issues))
                    .collect();
                match issues.len() == before {
                    true => Some(InputValue::List(values.into_iter().flatten().collect())),
                    false => None,
                }
            }
            (Self::Record { fields }, RawInput::Record(given)) => {
                check_decls(fields, given, path, cx, issues).map(|v| InputValue::Record(v.0))
            }
            (Self::File { .. }, RawInput::Text(name)) => {
                if cx.attachments.iter().any(|a| a == name) {
                    Some(InputValue::File(name.clone()))
                } else {
                    issues.push(
                        SpawnIssue::new(
                            path.clone(),
                            IssueCode::Dangling,
                            "no attachment has this name",
                        )
                        .got(raw.describe())
                        .hint("attach the file to the request and give its name here")
                        .known(cx.attachments),
                    );
                    None
                }
            }
            (Self::Path { .. }, RawInput::Text(t)) => match WorkdirPath::new(t.clone()) {
                Ok(p) => Some(InputValue::Path(p)),
                Err(e) => named(issues, e.to_string()),
            },
            (Self::Model, RawInput::Text(t)) => match ModelRef::parse(t) {
                Ok(m) => Some(InputValue::Model(m)),
                Err(e) => named(issues, e.to_string()),
            },
            (Self::Model, RawInput::Record(fields)) => {
                let text = |k: &str| match fields.get(k) {
                    Some(RawInput::Text(t)) => Some(t.as_str()),
                    _ => None,
                };
                let joined = match (text("provider"), text("model"), fields.len()) {
                    (Some(p), Some(m), 2) => format!("{p}/{m}"),
                    (None, Some(m), 1) => m.to_string(),
                    _ => return wrong(issues),
                };
                match ModelRef::parse(&joined) {
                    Ok(m) => Some(InputValue::Model(m)),
                    Err(e) => named(issues, e.to_string()),
                }
            }
            (Self::Blueprint, RawInput::Text(t)) => match BlueprintRef::parse(t) {
                Ok(b) => Some(InputValue::Blueprint(b)),
                Err(e) => named(issues, e.to_string()),
            },
            (Self::Duration, RawInput::Text(t)) => match parse_duration(t) {
                Some(secs) => Some(InputValue::Duration(secs)),
                None => named(issues, "not a duration".to_string()),
            },
            (Self::Url, RawInput::Text(t)) => match HttpUrl::new(t.clone()) {
                Ok(u) => Some(InputValue::Url(u)),
                Err(e) => named(issues, e.to_string()),
            },
            _ => wrong(issues),
        }
    }
}

fn within(n: u64, min: Option<u64>, max: Option<u64>) -> bool {
    min.is_none_or(|m| n >= m) && max.is_none_or(|m| n <= m)
}

fn bounds<T: fmt::Display>(lead: &str, unit: &str, min: &Option<T>, max: &Option<T>) -> String {
    match (min, max) {
        (Some(a), Some(b)) => format!("{lead}{a} to {b}{unit}"),
        (Some(a), None) => format!("{lead}at least {a}{unit}"),
        (None, Some(b)) => format!("{lead}at most {b}{unit}"),
        (None, None) => String::new(),
    }
}

/// A span of seconds exactly, in the form [`parse_duration`] reads back:
/// `1h30m`, `2d5s`, `0s`.
fn exact_duration(secs: u64) -> String {
    let parts = [(86_400, 'd'), (3_600, 'h'), (60, 'm'), (1, 's')];
    let mut left = secs;
    let mut out = String::new();
    for (size, unit) in parts {
        if left >= size {
            out.push_str(&format!("{}{unit}", left / size));
            left %= size;
        }
    }
    match out.is_empty() {
        true => "0s".to_string(),
        false => out,
    }
}

/// Seconds in `90s`, `5m`, `2h`, `1d`, or a sum of them such as `1h30m`.
pub fn parse_duration(text: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut digits = String::new();
    let mut any = false;
    for c in text.trim().chars() {
        match c {
            '0'..='9' => digits.push(c),
            's' | 'm' | 'h' | 'd' if !digits.is_empty() => {
                let n: u64 = digits.parse().ok()?;
                let unit = match c {
                    's' => 1,
                    'm' => 60,
                    'h' => 3600,
                    _ => 86_400,
                };
                total = total.checked_add(n.checked_mul(unit)?)?;
                digits.clear();
                any = true;
            }
            _ => return None,
        }
    }
    (any && digits.is_empty()).then_some(total)
}

/// Check the inputs a request gave against a graph's declarations.
///
/// Reports every problem: a name nothing declares, a required input with no
/// value and no default, and each value that does not fit its type. `given`
/// keys are unchecked text so that an unknown name is an issue, not a decode
/// failure.
pub fn check_inputs(
    decls: &[InputDecl],
    given: &BTreeMap<String, RawInput>,
    cx: &CheckCtx<'_>,
) -> Result<InputValues, SpawnIssues> {
    let mut issues = SpawnIssues::new();
    let values = check_decls(
        decls,
        given,
        &SpecPath::root().field("inputs"),
        cx,
        &mut issues,
    );
    match values {
        Some(values) if issues.is_empty() => Ok(values),
        _ => Err(issues),
    }
}

fn check_decls(
    decls: &[InputDecl],
    given: &BTreeMap<String, RawInput>,
    path: &SpecPath,
    cx: &CheckCtx<'_>,
    issues: &mut SpawnIssues,
) -> Option<InputValues> {
    let before = issues.len();
    for name in given.keys() {
        if !decls.iter().any(|d| d.name.as_str() == name) {
            issues.push(
                SpawnIssue::new(
                    path.key(name),
                    IssueCode::Unknown,
                    "nothing declares this input",
                )
                .known(decls.iter().map(|d| &d.name)),
            );
        }
    }
    let mut values = BTreeMap::new();
    for decl in decls {
        let at = path.key(decl.name.as_str());
        let raw = match (given.get(decl.name.as_str()), &decl.default) {
            (Some(raw), _) => raw.clone(),
            (None, Some(default)) => default.to_raw(),
            (None, None) if decl.required => {
                issues.push(
                    SpawnIssue::new(at, IssueCode::Missing, "this input is required")
                        .expected(decl.ty.describe()),
                );
                continue;
            }
            (None, None) => continue,
        };
        if let Some(value) = decl.ty.check(&raw, &at, cx, issues) {
            values.insert(decl.name.clone(), value);
        }
    }
    (issues.len() == before).then_some(InputValues(values))
}

#[cfg(test)]
#[path = "inputs_tests.rs"]
mod tests;
