//! Everything wrong with a spawn, said at once.
//!
//! A spawn is checked in three passes: the request's own shape, then what it
//! names on this machine, then the live handles a resumed run needs. Each pass
//! keeps going past a problem that does not block the next check, and the
//! caller gets the whole list. A spawner that is an agent can fix every
//! problem in one go instead of learning about them one retry at a time.
//!
//! Every issue says where it is (a [`SpecPath`] such as
//! `stages.plan.model`), what kind of problem it is ([`IssueCode`]), and when
//! they apply, what was expected, what arrived, how to fix it, and what the
//! valid choices were.

use std::fmt;

use serde::{Deserialize, Serialize};

/// One step of a [`SpecPath`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum PathSeg {
    /// A field of a struct: `stages`, `model`.
    Field(String),
    /// A named member of a collection: the `plan` in `stages.plan`.
    Key(String),
    /// A position in a list: the `2` in `items[2]`.
    Index(usize),
}

/// Where in a request (or spec) an issue is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SpecPath(pub Vec<PathSeg>);

impl SpecPath {
    /// The request itself.
    pub fn root() -> Self {
        Self::default()
    }

    /// This path, then a field.
    pub fn field(&self, name: &str) -> Self {
        self.with(PathSeg::Field(name.to_string()))
    }

    /// This path, then a named member.
    pub fn key(&self, name: &str) -> Self {
        self.with(PathSeg::Key(name.to_string()))
    }

    /// This path, then a list position.
    pub fn index(&self, i: usize) -> Self {
        self.with(PathSeg::Index(i))
    }

    fn with(&self, seg: PathSeg) -> Self {
        let mut segs = self.0.clone();
        segs.push(seg);
        Self(segs)
    }
}

impl fmt::Display for SpecPath {
    /// `stages.plan.model`, `inputs.items[2].id`, `regions["a.b"]`; the root
    /// renders as `(request)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("(request)");
        }
        let mut out = String::new();
        for (i, seg) in self.0.iter().enumerate() {
            match seg {
                PathSeg::Field(name) | PathSeg::Key(name) if is_plain(name) => {
                    if i > 0 {
                        out.push('.');
                    }
                    out.push_str(name);
                }
                PathSeg::Field(name) | PathSeg::Key(name) => out.push_str(&format!("[{name:?}]")),
                PathSeg::Index(n) => out.push_str(&format!("[{n}]")),
            }
        }
        f.write_str(&out)
    }
}

fn is_plain(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// What kind of problem an issue is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueCode {
    /// Something required was not given.
    Missing,
    /// A key or name that nothing declares.
    Unknown,
    /// A value of the wrong type.
    WrongType,
    /// A number outside its declared range, or a list or text of the wrong length.
    OutOfRange,
    /// A value that does not have the shape its type requires (a bad name, a
    /// malformed URL, a path that leaves the workdir).
    Invalid,
    /// The same name declared twice.
    Duplicate,
    /// A reference to a stage, region, edge or input that is not declared.
    Dangling,
    /// Two settings that cannot both hold.
    Conflict,
    /// Something the request is not allowed to ask for.
    NotAllowed,
    /// This machine cannot provide what the request names (an unknown model, a
    /// missing MCP server, a seed file that is not there).
    Unresolvable,
    /// A resumed run names something this machine no longer has.
    Unavailable,
    /// A resumed run names something that has changed since it started.
    Changed,
    /// A warning, never a refusal: the graph has stages from which the run
    /// can never reach an end.
    MayNeverFinish,
}

impl IssueCode {
    /// The code as it reads in a message: `wrong type`, `not allowed`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Unknown => "unknown",
            Self::WrongType => "wrong type",
            Self::OutOfRange => "out of range",
            Self::Invalid => "invalid",
            Self::Duplicate => "duplicate",
            Self::Dangling => "dangling reference",
            Self::Conflict => "conflict",
            Self::NotAllowed => "not allowed",
            Self::Unresolvable => "unresolvable",
            Self::Unavailable => "unavailable",
            Self::Changed => "changed",
            Self::MayNeverFinish => "may never finish",
        }
    }
}

/// One problem with a spawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SpawnIssue {
    /// Where the problem is.
    pub path: SpecPath,
    /// What kind of problem it is.
    pub code: IssueCode,
    /// One sentence saying what is wrong.
    pub message: String,
    /// What would have been accepted, when that is one thing: `an integer`.
    pub expected: Option<String>,
    /// What arrived instead: `text "five"`.
    pub got: Option<String>,
    /// How to fix it, when there is more to say than `expected`.
    pub hint: Option<String>,
    /// The valid choices, when the problem is picking one: declared inputs,
    /// installed models, a stage's edges.
    pub known: Vec<String>,
}

impl SpawnIssue {
    /// An issue with just a place, a code and a message.
    pub fn new(path: SpecPath, code: IssueCode, message: impl Into<String>) -> Self {
        Self {
            path,
            code,
            message: message.into(),
            expected: None,
            got: None,
            hint: None,
            known: Vec::new(),
        }
    }

    /// Say what was expected.
    pub fn expected(mut self, expected: impl Into<String>) -> Self {
        self.expected = Some(expected.into());
        self
    }

    /// Say what arrived.
    pub fn got(mut self, got: impl Into<String>) -> Self {
        self.got = Some(got.into());
        self
    }

    /// Say how to fix it.
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// List the valid choices.
    pub fn known<I, S>(mut self, known: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: fmt::Display,
    {
        self.known = known.into_iter().map(|k| k.to_string()).collect();
        self
    }
}

impl fmt::Display for SpawnIssue {
    /// `stages.plan.model: unresolvable: no provider serves "gpt-9" (expected
    /// ...; got ...). Hint. Known: a, b.`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = format!("{}: {}: {}", self.path, self.code.label(), self.message);
        match (&self.expected, &self.got) {
            (Some(e), Some(g)) => out.push_str(&format!(" (expected {e}; got {g})")),
            (Some(e), None) => out.push_str(&format!(" (expected {e})")),
            (None, Some(g)) => out.push_str(&format!(" (got {g})")),
            (None, None) => {}
        }
        if let Some(hint) = &self.hint {
            end_sentence(&mut out);
            out.push_str(hint);
        }
        if !self.known.is_empty() {
            end_sentence(&mut out);
            out.push_str(&format!("Known: {}", self.known.join(", ")));
        }
        f.write_str(&out)
    }
}

/// Close the sentence `out` ends with, unless it already closes itself.
fn end_sentence(out: &mut String) {
    match out.ends_with(['.', '!', '?']) {
        true => out.push(' '),
        false => out.push_str(". "),
    }
}

/// Every problem found with a spawn. Empty means none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SpawnIssues(pub Vec<SpawnIssue>);

impl SpawnIssues {
    /// No issues yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one issue.
    pub fn push(&mut self, issue: SpawnIssue) {
        self.0.push(issue);
    }

    /// Record every issue from another check.
    pub fn absorb(&mut self, other: SpawnIssues) {
        self.0.extend(other.0);
    }

    /// Whether nothing is wrong.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many issues there are.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The issues.
    pub fn iter(&self) -> std::slice::Iter<'_, SpawnIssue> {
        self.0.iter()
    }

    /// `Ok(value)` when nothing is wrong, the issues otherwise.
    pub fn into_result<T>(self, value: T) -> Result<T, SpawnIssues> {
        match self.is_empty() {
            true => Ok(value),
            false => Err(self),
        }
    }

    /// Keep an `Ok` value, or record its issues and return `None`, so a check
    /// can carry on past a problem that does not block it.
    pub fn take<T>(&mut self, result: Result<T, SpawnIssues>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(issues) => {
                self.absorb(issues);
                None
            }
        }
    }
}

impl From<SpawnIssue> for SpawnIssues {
    fn from(issue: SpawnIssue) -> Self {
        Self(vec![issue])
    }
}

impl fmt::Display for SpawnIssues {
    /// A count, then one numbered line per issue.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.0.len();
        let mut out = format!(
            "{n} problem{} with this spawn:",
            if n == 1 { "" } else { "s" }
        );
        for (i, issue) in self.0.iter().enumerate() {
            out.push_str(&format!("\n{}. {issue}", i + 1));
        }
        f.write_str(&out)
    }
}

impl std::error::Error for SpawnIssues {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_render_the_way_a_person_writes_them() {
        let p = SpecPath::root().field("stages").key("plan").field("model");
        assert_eq!(p.to_string(), "stages.plan.model");
        let odd = SpecPath::root().field("a b").index(1);
        assert_eq!(odd.to_string(), "[\"a b\"][1]");
        let q = SpecPath::root()
            .field("inputs")
            .key("items")
            .index(2)
            .field("id");
        assert_eq!(q.to_string(), "inputs.items[2].id");
        let odd = SpecPath::root().field("regions").key("a.b");
        assert_eq!(odd.to_string(), "regions[\"a.b\"]");
        assert_eq!(SpecPath::root().to_string(), "(request)");
    }

    #[test]
    fn an_issue_says_everything_it_knows_on_one_line() {
        let issue = SpawnIssue::new(
            SpecPath::root().field("inputs").key("depth"),
            IssueCode::WrongType,
            "this input is an integer",
        )
        .expected("an integer")
        .got("text \"deep\"")
        .hint("send a number such as 3")
        .known(["1", "2"]);
        assert_eq!(
            issue.to_string(),
            "inputs.depth: wrong type: this input is an integer (expected an integer; got text \"deep\"). send a number such as 3. Known: 1, 2"
        );
        let bare = SpawnIssue::new(SpecPath::root(), IssueCode::Missing, "no source");
        assert_eq!(bare.to_string(), "(request): missing: no source");
        let only_e = SpawnIssue::new(SpecPath::root(), IssueCode::Invalid, "x").expected("y");
        assert_eq!(only_e.to_string(), "(request): invalid: x (expected y)");
        let only_g = SpawnIssue::new(SpecPath::root(), IssueCode::Invalid, "x").got("z");
        assert_eq!(only_g.to_string(), "(request): invalid: x (got z)");
        // A message or hint that ends its own sentence is not given a second
        // full stop.
        let closed = SpawnIssue::new(SpecPath::root(), IssueCode::Invalid, "no restart.")
            .hint("really?")
            .known(["a"]);
        assert_eq!(
            closed.to_string(),
            "(request): invalid: no restart. really? Known: a"
        );
    }

    #[test]
    fn every_code_has_a_label() {
        use IssueCode::*;
        let all = [
            Missing,
            Unknown,
            WrongType,
            OutOfRange,
            Invalid,
            Duplicate,
            Dangling,
            Conflict,
            NotAllowed,
            Unresolvable,
            Unavailable,
            Changed,
        ];
        let labels: std::collections::BTreeSet<_> = all.iter().map(|c| c.label()).collect();
        assert_eq!(labels.len(), all.len());
    }

    #[test]
    fn issues_collect_and_report_together() {
        let mut issues = SpawnIssues::new();
        assert!(issues.is_empty());
        assert_eq!(issues.clone().into_result(7), Ok(7));
        assert_eq!(issues.take::<u8>(Ok(1)), Some(1));
        let one = SpawnIssue::new(SpecPath::root().field("a"), IssueCode::Missing, "need a");
        assert_eq!(issues.take::<u8>(Err(one.clone().into())), None);
        issues.push(SpawnIssue::new(
            SpecPath::root().field("b"),
            IssueCode::Unknown,
            "what is b",
        ));
        assert_eq!(issues.len(), 2);
        assert_eq!(issues.iter().next(), Some(&one));
        assert_eq!(
            issues.to_string(),
            "2 problems with this spawn:\n1. a: missing: need a\n2. b: unknown: what is b"
        );
        let single: SpawnIssues = one.into();
        assert!(single.to_string().starts_with("1 problem with this spawn:"));
        assert!(single.clone().into_result(()).is_err());
        let as_error: &dyn std::error::Error = &single;
        assert!(as_error.to_string().contains("need a"));
    }
}
