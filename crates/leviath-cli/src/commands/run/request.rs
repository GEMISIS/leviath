//! Turning a `lev run` command line into the [`SpawnRequest`] it asks for.
//!
//! A command line names a blueprint, or hands over a whole request with
//! `--request`, and supplies the run's inputs: `--input name=value`, the
//! `--<name> value` short form, and `--task`, each read by the type the run
//! declares for it (`inputs`). The launch flags (`--model`,
//! `--yolo`, `--allow`, `--max-depth`, `--workdir`, the `--output-*` flags)
//! land on the request too, over whatever a `--request` file said.
//!
//! Everything a person could have typed wrong is found here, before the
//! daemon is asked and before an editor opens for a task: every input that
//! does not read, each with its path, all at once.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use leviath_core::mime::InboundPart;
use leviath_runtime::spec::env::LoadedBlueprint;
use leviath_runtime::spec::graph::OutputDef;
use leviath_runtime::spec::inputs::{InputDecl, RawInput};
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use leviath_runtime::spec::launch::Unattended;
use leviath_runtime::spec::names::{ModelRef, ProfileName, ToolName};
use leviath_runtime::spec::request::{SpawnRequest, SpawnSource};

use super::attach;
use super::inputs::{Typed, read_inputs};
use super::task::resolve_task;
use crate::daemon::client::LocalRun;

/// The name of the input `--task` gives.
pub(crate) const TASK_INPUT: &str = "task";

/// What `lev run` was asked for, before any of it is read.
///
/// Each field is something the person typed (or, from the dashboard, picked).
/// [`run_request`] reads it into a [`LocalRun`]; the two are different types
/// so "what was asked for" and "what that means" stay apart.
pub struct RunLine<'a> {
    /// The blueprint path or installed name, as given. `None` with a
    /// `--request` file, which names its own.
    pub path: Option<&'a str>,
    /// `--request`: a file holding a whole spawn request, TOML or JSON.
    pub request_file: Option<&'a Path>,
    /// The task text, if it was given rather than read from stdin or an editor.
    pub task: Option<&'a str>,
    /// Whether stdin is a terminal, injected so the editor path is testable.
    pub stdin_is_terminal: &'a dyn Fn() -> bool,
    /// Whether a task the run needs and was not given is asked for (on
    /// stdin, or in an editor). Off for `--check`, which reports a missing
    /// task with every other problem rather than stopping to ask for one.
    pub ask_for_task: bool,
    /// `--input name=value`, in the order given.
    pub inputs: Vec<String>,
    /// `--<name> value`, the short form of `--input name=value`.
    pub named: HashMap<String, String>,
    /// Inputs already typed, by a caller that built them itself (the
    /// dashboard's form).
    pub values: BTreeMap<String, RawInput>,
    /// Files attached with `--attach`, already read: their paths were
    /// relative to where the command ran, which only the caller knows.
    pub parts: Vec<InboundPart>,
    /// The directory relative paths in values are read against.
    pub cwd: &'a Path,
    /// `--model`, over the blueprint's choice.
    pub model: Option<String>,
    /// The working directory tools run in.
    pub workdir: &'a str,
    /// Whether `workdir` was asked for (`--workdir`) rather than defaulted.
    /// A `--request` file's own workdir gives way only to one asked for.
    pub workdir_given: bool,
    /// `--yolo`: run unattended.
    pub yolo: bool,
    /// `--yolo=<name>`: the profile that says which parts of unattended a
    /// person still wants. `None` is the bare flag.
    pub yolo_profile: Option<String>,
    /// `--allow`: tools permitted outright.
    pub allow: Vec<String>,
    /// `--max-depth`: sub-agent tree cap.
    pub max_depth: Option<usize>,
    /// `--no-seed-commands`: refuse the blueprint's command seeds.
    pub no_seed_commands: bool,
    /// The output shape the caller asked for, over the blueprint's.
    pub output_request: Option<leviath_core::output::OutputSpec>,
}

impl<'a> RunLine<'a> {
    /// A command line naming `path` (or nothing) and asking for nothing else,
    /// attended, working in `workdir`, reading paths against `cwd`.
    pub fn new(path: Option<&'a str>, workdir: &'a str, cwd: &'a Path) -> Self {
        Self {
            path,
            request_file: None,
            task: None,
            stdin_is_terminal: &crate::daemon::client::never_interactive,
            ask_for_task: true,
            inputs: Vec::new(),
            named: HashMap::new(),
            values: BTreeMap::new(),
            parts: Vec::new(),
            cwd,
            model: None,
            workdir,
            workdir_given: false,
            yolo: false,
            yolo_profile: None,
            allow: Vec::new(),
            max_depth: None,
            no_seed_commands: false,
            output_request: None,
        }
    }
}

/// The run a command line asks for: its request, and what the inputs are
/// declared as.
struct Source {
    /// The request so far: its source, and whatever a `--request` file said.
    request: SpawnRequest,
    /// The inputs the run takes.
    decls: Vec<InputDecl>,
    /// What the run is called and what it does, for the editor's template.
    name: String,
    description: String,
    /// The blueprint's `agent.toml`, for the warnings read from it. Empty for a
    /// raw graph.
    manifest: PathBuf,
}

/// Every problem with `issues`, one per line, under a line saying how many.
pub(crate) fn issues_report(issues: &SpawnIssues) -> String {
    let mut lines = vec![match issues.len() {
        1 => "1 problem with this run:".to_string(),
        n => format!("{n} problems with this run:"),
    }];
    lines.extend(issues.iter().map(|issue| format!("  {issue}")));
    lines.join("\n")
}

/// Read a command line into the request it asks for.
///
/// Inputs are read before the task on purpose: a value that does not read has
/// to fail before the person is dropped into an editor and types a paragraph
/// they are about to lose. Whether the run as a whole holds together is the
/// daemon's to say, with every other problem with the request.
pub fn run_request(line: RunLine<'_>) -> anyhow::Result<LocalRun> {
    let mut source = source(line.path, line.request_file)?;
    let mut typed: Vec<Typed> = Vec::with_capacity(line.inputs.len() + line.named.len());
    let mut issues = SpawnIssues::new();
    for flag in &line.inputs {
        match Typed::from_input_flag(flag) {
            Ok(t) => typed.push(t),
            Err(message) => issues.push(SpawnIssue::new(
                SpecPath::root().field("inputs"),
                IssueCode::Invalid,
                message,
            )),
        }
    }
    let mut named: Vec<(&String, &String)> = line.named.iter().collect();
    named.sort();
    typed.extend(
        named
            .into_iter()
            .map(|(name, text)| Typed::from_named_flag(name, text)),
    );
    let read = match read_inputs(&source.decls, &typed, line.cwd) {
        Ok(read) => Some(read),
        Err(found) => {
            issues.absorb(found);
            None
        }
    };
    let launch_issues = apply_flags(&mut source.request, &line);
    issues.absorb(launch_issues);
    let task_decl = source.decls.iter().find(|d| d.name.as_str() == TASK_INPUT);
    let given_task = line.task.map(str::trim).filter(|t| !t.is_empty());
    if task_decl.is_none() && given_task.is_some() {
        issues.push(
            SpawnIssue::new(
                SpecPath::root().field("inputs").key(TASK_INPUT),
                IssueCode::Unknown,
                format!("{} takes no task", source.name),
            )
            .hint("give it its inputs with --input <name>=<value>")
            .known(source.decls.iter().map(|d| &d.name)),
        );
    }
    let Some(read) = read.filter(|_| issues.is_empty()) else {
        anyhow::bail!(issues_report(&issues));
    };
    let mut parts = line.parts;
    parts.extend(read.parts);
    let mut unresolved = read.unresolved;
    let mut values = line.values;
    values.extend(read.values);
    source.request.inputs.extend(values);
    // A run that takes a task is asked for one, unless it was given another
    // way, or it does not insist and the caller gave it something else to
    // work on: `lev run reviewer --diff @x.patch` is a complete command line.
    if let Some(decl) = task_decl
        && !source.request.inputs.contains_key(TASK_INPUT)
    {
        let handed_in = !source.request.inputs.is_empty() || !parts.is_empty();
        let waived = (handed_in && !decl.required) || !line.ask_for_task;
        let task = match (given_task, waived) {
            (None, true) => String::new(),
            _ => resolve_task(
                given_task,
                &source.name,
                &source.description,
                line.stdin_is_terminal,
            )?,
        };
        // A file the task names where it mentions it: `edit @hero.png`.
        let (task, named_files, missing) = attach::inline_parts(&task, None, line.cwd)?;
        parts.extend(named_files);
        unresolved.extend(missing);
        if !task.trim().is_empty() {
            source
                .request
                .inputs
                .insert(TASK_INPUT.to_string(), RawInput::Text(task));
        }
    }
    attach::warn_unresolved(&unresolved);
    source.request.attachments.extend(
        dedupe(parts)
            .into_iter()
            .map(crate::daemon::requests::attachment),
    );
    // What the run reports about itself is what the request asks for, which a
    // `--request` file may have set and the flags only adjusted.
    let request = source.request;
    let workdir = request
        .workdir
        .as_deref()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (yolo, yolo_profile) = match &request.launch.unattended {
        Unattended::Off => (false, None),
        Unattended::All => (true, None),
        Unattended::Profile(name) => (true, Some(name.to_string())),
    };
    Ok(LocalRun {
        request,
        manifest: source.manifest,
        workdir,
        yolo,
        yolo_profile,
        output: line.output_request,
    })
}

/// Drop an exact repeat of a part (same region, name and bytes), which no
/// caller ever means: the dashboard resolves a task's `@path` and hands the
/// part over, and the task is read for `@path` tokens again here.
fn dedupe(parts: Vec<InboundPart>) -> Vec<InboundPart> {
    let mut kept: Vec<InboundPart> = Vec::with_capacity(parts.len());
    for part in parts {
        let repeat = kept
            .iter()
            .any(|k| k.region == part.region && k.name == part.name && k.data == part.data);
        if !repeat {
            kept.push(part);
        }
    }
    kept
}

/// What the command line runs: the blueprint it names, or the request its
/// `--request` file holds, with the inputs the run declares.
fn source(path: Option<&str>, request_file: Option<&Path>) -> anyhow::Result<Source> {
    let request = match (path, request_file) {
        (Some(_), Some(_)) => {
            anyhow::bail!("give a blueprint or --request, not both: a request names what it runs")
        }
        (_, Some(file)) => read_request_file(file)?,
        (path, None) => SpawnRequest::new(
            crate::daemon::requests::blueprint_source(path.unwrap_or("."))
                .map_err(anyhow::Error::msg)?,
        ),
    };
    let loaded = match &request.source {
        SpawnSource::Blueprint(reference) => crate::daemon::resolve_env::load_installed(
            leviath_core::paths::agents_dir().as_deref(),
            reference,
        ),
        SpawnSource::BlueprintFile(path) => crate::daemon::resolve_env::load_file(path),
        SpawnSource::Raw(graph) => {
            return Ok(Source {
                decls: graph.inputs.clone(),
                name: graph
                    .title
                    .clone()
                    .unwrap_or_else(|| "this run".to_string()),
                description: graph.description.clone().unwrap_or_default(),
                manifest: PathBuf::new(),
                request,
            });
        }
    };
    // Read the way the daemon reads it, so a blueprint that will not load
    // fails here, before an editor opens for its task.
    let loaded: LoadedBlueprint = loaded.map_err(|issue| anyhow::anyhow!("{issue}"))?;
    Ok(Source {
        decls: loaded.graph.inputs,
        name: loaded.reference.name.to_string(),
        description: loaded.graph.description.unwrap_or_default(),
        manifest: loaded.base_dir.join(leviath_blueprint::FILE_NAME),
        request,
    })
}

/// Read a whole spawn request from `file`: TOML for a `.toml` file, JSON
/// otherwise. An unknown key is refused, the same as on every other front
/// door.
pub(crate) fn read_request_file(file: &Path) -> anyhow::Result<SpawnRequest> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| anyhow::anyhow!("could not read --request '{}': {e}", file.display()))?;
    let toml = file
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"));
    let parsed = match toml {
        true => toml::from_str(&text).map_err(|e| e.to_string()),
        false => serde_json::from_str(&text).map_err(|e| e.to_string()),
    };
    parsed.map_err(|e| {
        anyhow::anyhow!(
            "--request '{}' is not a spawn request: {e}\n  `lev schema spawn-request` prints what one holds",
            file.display()
        )
    })
}

/// Put the launch flags on `request`, over what it already says. A flag
/// left off leaves the request's own setting alone. Every flag that does
/// not read is an issue at the request field it would have set.
fn apply_flags(request: &mut SpawnRequest, line: &RunLine<'_>) -> SpawnIssues {
    let mut issues = SpawnIssues::new();
    let root = SpecPath::root();
    if let Some(model) = line.model.as_deref().filter(|m| !m.trim().is_empty()) {
        match ModelRef::parse(model) {
            Ok(model) => request.model = Some(model),
            Err(e) => issues.push(
                SpawnIssue::new(root.field("model"), IssueCode::Invalid, e.to_string())
                    .got(format!("{model:?}")),
            ),
        }
    }
    if let Some(output) = &line.output_request {
        match OutputDef::from_output_spec(output) {
            Ok(output) => request.output = Some(output),
            Err(found) => issues.absorb(found),
        }
    }
    if line.workdir_given || request.workdir.is_none() {
        request.workdir = Some(PathBuf::from(line.workdir));
    }
    let launch = root.field("launch");
    if line.yolo {
        request.launch.unattended = match line.yolo_profile.as_deref() {
            Some(profile) if !profile.is_empty() => match ProfileName::new(profile) {
                Ok(name) => Unattended::Profile(name),
                Err(e) => {
                    issues.push(
                        SpawnIssue::new(
                            launch.field("unattended"),
                            IssueCode::Invalid,
                            e.to_string(),
                        )
                        .got(format!("{profile:?}")),
                    );
                    Unattended::All
                }
            },
            _ => Unattended::All,
        };
    }
    let first = request.launch.allow.len();
    for (i, tool) in line.allow.iter().enumerate() {
        match ToolName::new(tool.as_str()) {
            Ok(name) => request.launch.allow.push(name),
            Err(e) => issues.push(
                SpawnIssue::new(
                    launch.field("allow").index(first + i),
                    IssueCode::Invalid,
                    e.to_string(),
                )
                .got(format!("{tool:?}")),
            ),
        }
    }
    if let Some(depth) = line.max_depth {
        request.launch.max_depth = Some(u8::try_from(depth).unwrap_or(u8::MAX));
    }
    if line.no_seed_commands {
        request.launch.seed_commands = false;
    }
    issues
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
