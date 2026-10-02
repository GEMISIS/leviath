//! What `spawn_agent` and `validate_spawn` take, read into the request a
//! child is started from, and how a refusal is told back to the model.
//!
//! The arguments are read strictly: a key the schema in
//! `leviath_tools::subagent_defs` does not list is refused, never dropped.
//! Every problem comes back as a [`SpawnIssue`] with the path of the argument
//! it is about, so the model can fix all of them in one retry.

use std::collections::BTreeMap;
use std::path::Path;

use leviath_core::JsonDoc;
use leviath_runtime::spec::graph::stage::looks_like_a_path;
use leviath_runtime::spec::graph::{OutputDef, RunGraph};
use leviath_runtime::spec::inputs::RawInput;
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use leviath_runtime::spec::names::{BlueprintPath, BlueprintRef};
use leviath_runtime::spec::request::{SpawnRequest, SpawnSource};
use serde::Deserialize;
use serde_json::Value;

use super::SubAgentHandle;

/// The shape every spawn call takes, for a refusal that cannot read it.
const SHAPE: &str = "{\"source\": {\"blueprint\": \"<name>\"} or {\"graph\": {...}}, \
                     \"inputs\": {\"task\": \"<text>\"}, \"wait\": false, \
                     \"max_child_depth\": 1, \"output\": {\"format\": \"<label>\"}, \
                     \"parts\": [\"<part name>\"]}";

/// A spawn call's arguments as the model wrote them.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SpawnCall {
    /// What to run: read by [`source`], which says precisely what is wrong.
    source: Value,
    /// The child's inputs, by name.
    #[serde(default)]
    inputs: BTreeMap<String, RawInput>,
    /// Whether to block until the child finishes.
    #[serde(default)]
    pub wait: bool,
    /// How deep the child's own tree may grow.
    #[serde(default)]
    max_child_depth: Option<u8>,
    /// The tools the child may call without asking. `None` asks for the
    /// parent's own list.
    #[serde(default)]
    allow: Option<Vec<String>>,
    /// The answer shape asked of the child.
    #[serde(default)]
    output: Option<OutputArgs>,
    /// Stored parts of the parent to hand the child, by name or hash prefix.
    #[serde(default)]
    pub parts: Vec<String>,
}

/// The answer shape a parent may ask of its child.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputArgs {
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    example: Option<String>,
    #[serde(default)]
    schema: Option<Value>,
}

impl OutputArgs {
    /// The output shape this asks for, or `None` when it asks for nothing.
    fn into_def(self) -> Option<OutputDef> {
        let text = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
        let def = OutputDef {
            format: text(self.format),
            instructions: text(self.instructions),
            example: text(self.example),
            schema: self.schema.map(JsonDoc::new),
            ..OutputDef::default()
        };
        (def != OutputDef::default()).then_some(def)
    }
}

impl SpawnCall {
    /// Read a call's arguments, or the one issue that says why they do not
    /// read and what shape they should have.
    pub(super) fn parse(arguments: &Value) -> Result<Self, SpawnIssues> {
        serde_json::from_value(arguments.clone()).map_err(|e| {
            SpawnIssue::new(
                SpecPath::root(),
                IssueCode::Invalid,
                format!("the arguments do not read: {e}"),
            )
            .expected(SHAPE)
            .into()
        })
    }

    /// The request this call asks for, as a child of `h`'s run, handed
    /// `attachments`. The child inherits how unattended its parent is, its
    /// parent's model override, its workdir, its seed-command opt-out and,
    /// unless the call names its own, its allowed tools; the host narrows the
    /// rest against the parent's policy.
    pub(super) fn into_request(
        self,
        h: &SubAgentHandle,
        attachments: Vec<leviath_core::mime::InboundPart>,
    ) -> Result<SpawnRequest, SpawnIssues> {
        let source = source(&self.source, Path::new(&h.workdir))?;
        let launch = crate::daemon::requests::TaskLaunch {
            parts: attachments,
            model: h.model_override.clone(),
            workdir: Some(h.workdir.clone()),
            unattended: h.unattended,
            profile: h.yolo_profile.clone(),
            max_depth: self.max_child_depth.map(usize::from),
            allow: self.allow.unwrap_or_else(|| h.allow.clone()),
            no_seed_commands: h.no_seed_commands,
            ..Default::default()
        };
        let mut request = launch.into_request_for(source).map_err(|e| {
            SpawnIssues::from(SpawnIssue::new(
                SpecPath::root(),
                IssueCode::Invalid,
                format!("this run's own settings do not carry over to a child: {e}"),
            ))
        })?;
        request.inputs = self.inputs;
        request.output = self.output.and_then(OutputArgs::into_def);
        Ok(request)
    }
}

/// What `source` names: an installed blueprint by name or `{name, digest}`,
/// a blueprint's directory outside the run's workspace, or a whole graph.
fn source(value: &Value, workdir: &Path) -> Result<SpawnSource, SpawnIssues> {
    let at = SpecPath::root().field("source");
    let one_of = |code, message: &str| {
        SpawnIssue::new(at.clone(), code, message)
            .expected("{\"blueprint\": \"<name>\"} or {\"graph\": {...}}")
            .known(["blueprint", "graph"])
    };
    let Value::Object(map) = value else {
        return Err(one_of(
            IssueCode::WrongType,
            "`source` is an object naming what to run",
        )
        .into());
    };
    match (map.get("blueprint"), map.get("graph"), map.len()) {
        (Some(blueprint), None, 1) => named(blueprint, workdir, at.field("blueprint")),
        (None, Some(graph), 1) => serde_json::from_value::<RunGraph>(graph.clone())
            .map(|graph| SpawnSource::Raw(Box::new(graph)))
            .map_err(|e| {
                SpawnIssue::new(at.field("graph"), IssueCode::Invalid, e.to_string())
                    .hint("`spawn_schema` with part \"RunGraph\" shows every field a graph takes")
                    .into()
            }),
        _ => Err(one_of(
            IssueCode::Conflict,
            "`source` names exactly one of `blueprint` and `graph`, and nothing else",
        )
        .into()),
    }
}

/// `e` as the one issue at `at`, of a name that does not read.
fn invalid(at: SpecPath, e: impl std::fmt::Display) -> SpawnIssues {
    SpawnIssue::new(at, IssueCode::Invalid, e.to_string()).into()
}

/// The blueprint `value` names, at `at`.
fn named(value: &Value, workdir: &Path, at: SpecPath) -> Result<SpawnSource, SpawnIssues> {
    match value {
        Value::String(text) if looks_like_a_path(text) => directory(text, workdir, at),
        Value::String(text) => BlueprintRef::parse(text)
            .map(SpawnSource::Blueprint)
            .map_err(|e| invalid(at, e)),
        Value::Object(_) => serde_json::from_value::<BlueprintRef>(value.clone())
            .map(SpawnSource::Blueprint)
            .map_err(|e| invalid(at, e)),
        _ => Err(SpawnIssue::new(
            at,
            IssueCode::WrongType,
            "a blueprint is named by text or by {name, digest}",
        )
        .expected("\"<name>\" or {\"name\": \"<name>\", \"digest\": \"<hex>\"}")
        .into()),
    }
}

/// A blueprint named by its directory (or the manifest in it): never one
/// inside the run's own workspace.
///
/// `write_file` is confined to the workspace, but a blueprint brings its own
/// seed commands and MCP servers, which run on the host before the child's
/// first inference. A model steered by injected content could write a
/// manifest into its workspace and spawn it, turning a confined file write
/// into unconfined command execution. Refusing a directory inside the
/// workspace closes that and leaves everything legitimate working: an
/// installed blueprint by name, or a directory a person chose.
fn directory(text: &str, workdir: &Path, at: SpecPath) -> Result<SpawnSource, SpawnIssues> {
    let home = leviath_core::paths::home_dir();
    // A `~` with no home to expand to names nothing; it reads as the
    // workspace itself, and is refused as one.
    let written = leviath_core::paths::expand_home(text, home.as_deref()).unwrap_or_default();
    let joined = workdir.join(written);
    let dir = match joined.is_file() {
        true => joined.parent().map(Path::to_path_buf).unwrap_or_default(),
        false => joined,
    };
    if leviath_core::resolves_within(&dir, workdir) {
        return Err(SpawnIssue::new(
            at,
            IssueCode::NotAllowed,
            format!(
                "'{text}' is inside this agent's own working directory, and an agent may not \
                 author the blueprint it runs"
            ),
        )
        .hint("name an installed blueprint, or a blueprint directory outside the workspace")
        .into());
    }
    BlueprintPath::new(dir.to_string_lossy())
        .map(SpawnSource::BlueprintFile)
        .map_err(|e| invalid(at, e))
}

/// A refusal as the model reads it: a count, then one numbered line per
/// problem, each with where it is, what is wrong and how to fix it.
pub(super) fn refusal(tool: &str, issues: &SpawnIssues) -> String {
    let n = issues.len();
    let mut out = format!(
        "[error] {tool} refused: {n} problem{}. Fix every one and call again; validate_spawn \
         checks a spawn without starting it.",
        if n == 1 { "" } else { "s" }
    );
    for (i, issue) in issues.iter().enumerate() {
        out.push_str(&format!("\n{}. {}", i + 1, line(issue)));
    }
    out
}

/// One issue on one line, at the argument the model wrote.
fn line(issue: &SpawnIssue) -> String {
    let mut out = format!(
        "{}: {}: {}",
        argument_path(&issue.path),
        issue.code.label(),
        issue.message
    );
    match (&issue.expected, &issue.got) {
        (Some(e), Some(g)) => out.push_str(&format!(" (expected {e}; got {g})")),
        (Some(e), None) => out.push_str(&format!(" (expected {e})")),
        (None, Some(g)) => out.push_str(&format!(" (got {g})")),
        (None, None) => {}
    }
    let fix = issue
        .hint
        .clone()
        .unwrap_or_else(|| default_fix(issue).to_string());
    out.push_str(&format!(". Fix: {fix}"));
    if !issue.known.is_empty() {
        out.push_str(&format!(". Known: {}", issue.known.join(", ")));
    }
    out
}

/// Where an issue is, as the tool's arguments spell it: a request's
/// `source.raw` is the call's `source.graph`.
fn argument_path(path: &SpecPath) -> String {
    let text = path.to_string();
    match text.strip_prefix("source.raw") {
        Some(rest) => format!("source.graph{rest}"),
        None => text,
    }
}

/// How to fix an issue that did not say.
fn default_fix(issue: &SpawnIssue) -> &'static str {
    match (issue.code, issue.known.is_empty()) {
        (IssueCode::Unknown | IssueCode::Dangling | IssueCode::Unresolvable, false) => {
            "use one of the known names below"
        }
        (IssueCode::Unknown, true) => "remove it, or check its spelling",
        (IssueCode::Dangling, true) => "refer to something the graph declares",
        (IssueCode::Unresolvable, true) => "name something this machine has",
        (IssueCode::Missing, _) => "supply it",
        (IssueCode::WrongType, _) => "send a value of the expected type",
        (IssueCode::OutOfRange, _) => "send a value inside the allowed range",
        (IssueCode::Invalid, _) => "correct the value",
        (IssueCode::Duplicate, _) => "give each one a different name",
        (IssueCode::Conflict, _) => "keep only one of the settings that conflict",
        (IssueCode::NotAllowed, _) => "leave it out; this run may not ask for it",
        (IssueCode::Unavailable, _) => "try again shortly, or pick another",
        (IssueCode::Changed, _) => "start a fresh run",
        (IssueCode::MayNeverFinish, _) => "give the stages named a way to end the run",
    }
}
