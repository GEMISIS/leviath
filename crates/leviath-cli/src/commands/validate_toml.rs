//! Reading the blueprint `lev validate` was pointed at, and the part of its
//! report that describes it.
//!
//! A blueprint is an `agent.toml`: a run graph with a name, read by
//! `leviath-blueprint`. Checking one reads it the way a spawn would, confirms
//! that its graph holds together (every stage, region and input it names is
//! declared, and every setting is in range), and compiles every piece of code
//! it names. Every problem the graph has is reported at once, one per line
//! with its path. The lint that runs after it lives in `crate::lint`.

use std::path::{Path, PathBuf};

use leviath_blueprint::{BlueprintError, BlueprintFile, FILE_NAME};
use leviath_runtime::spec::graph::{CodeRef, EdgeCondition, RunGraph, StageDef};
use leviath_runtime::spec::inputs::InputSlot;
use leviath_runtime::spec::issues::SpecPath;

/// The `agent.toml` a validate target names: the file itself, or the one
/// inside a directory. Says nothing about whether it exists.
pub(super) fn blueprint_path(path: &Path) -> PathBuf {
    match path.is_file() {
        true => path.to_path_buf(),
        false => path.join(FILE_NAME),
    }
}

/// Why a blueprint did not check out. An I/O failure is an ordinary error;
/// the other two are what `lev validate` exists to report, so its caller
/// prints them its own way.
#[derive(Debug)]
pub(super) enum CheckError {
    Io(anyhow::Error),
    Parse(String),
    Validation(String),
}

/// A blueprint that read, held together and compiled.
#[derive(Debug)]
pub(super) struct Checked {
    /// The file as written.
    pub file: BlueprintFile,
    /// Its run graph, with the title and description filled in.
    pub graph: RunGraph,
    /// The directory holding it: where its code and `tools/` live.
    pub agent_dir: PathBuf,
}

/// Read and check the blueprint at `path` (an `agent.toml`, or the directory
/// holding one), and compile every custom region, output validator and stage
/// hook it names, the same checks a spawn makes.
pub(super) fn check(path: &Path) -> Result<Checked, CheckError> {
    let file_path = blueprint_path(path);
    if !file_path.exists() {
        return Err(CheckError::Io(anyhow::anyhow!(
            "No {FILE_NAME} found at {}",
            path.display()
        )));
    }
    // Read and checked here, the way `leviath_blueprint::validate` does it,
    // because the lint needs the file as written beside the graph the loader
    // fills in from it.
    let text = std::fs::read_to_string(&file_path).map_err(|source| {
        CheckError::Io(
            BlueprintError::Read {
                path: file_path.clone(),
                source,
            }
            .into(),
        )
    })?;
    let file = BlueprintFile::parse(&text).map_err(|message| {
        CheckError::Parse(
            BlueprintError::Parse {
                path: file_path.clone(),
                message,
            }
            .to_string(),
        )
    })?;
    let graph = file.run_graph();
    graph
        .validate(&SpecPath::root().field("graph"))
        .map_err(|issues| {
            let mut lines = vec![format!(
                "{} has {} problem(s):",
                file_path.display(),
                issues.len()
            )];
            lines.extend(issues.iter().map(|issue| format!("  {issue}")));
            CheckError::Validation(lines.join("\n"))
        })?;
    let agent_dir = file_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    crate::daemon::spawn::check_graph_code(&graph, &agent_dir).map_err(CheckError::Validation)?;
    Ok(Checked {
        file,
        graph,
        agent_dir,
    })
}

/// The blueprint itself, for a caller that wants to know what it just validated.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct BlueprintSummary {
    /// The blueprint's `[blueprint] name`.
    pub name: String,
    /// Its declared version.
    pub version: String,
    /// Its one-line description.
    pub description: String,
    /// Null when the graph names no `entry`, in which case the first stage
    /// is the entry.
    pub entry_stage: Option<String>,
    /// Stage names in graph order.
    pub stages: Vec<String>,
    /// Whether `lev run <agent> --task <text>` is accepted: the graph
    /// declares a `task` input. False means a run handing this agent a task
    /// is refused at spawn, so a harness can check here instead.
    pub accepts_task: bool,
    /// Every input the graph declares, in declaration order.
    pub inputs: Vec<InputSummary>,
}

/// One declared input: `--input <key>=...` (or `--<key>`) on `lev run`, the
/// `inputs.<key>` field over the API, and where its value goes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct InputSummary {
    /// The input's name.
    pub key: String,
    /// What it takes, as a person reads it.
    #[serde(rename = "type")]
    pub kind: String,
    /// The regions its value seeds, in the order it binds them.
    pub regions: Vec<String>,
    /// True when a spawn without it is refused.
    pub required: bool,
}

impl BlueprintSummary {
    /// The summary of a checked blueprint.
    pub(super) fn of(checked: &Checked) -> Self {
        let graph = &checked.graph;
        let meta = &checked.file.blueprint;
        Self {
            name: meta.name.to_string(),
            version: meta.version.clone(),
            description: meta.description.clone().unwrap_or_default(),
            entry_stage: graph.entry.as_ref().map(ToString::to_string),
            stages: graph.stages.iter().map(|s| s.name.to_string()).collect(),
            accepts_task: graph.inputs.iter().any(|i| i.name.as_str() == "task"),
            inputs: input_summaries(graph),
        }
    }
}

/// The inputs a graph declares, in declaration order.
///
/// The prose and JSON halves of the report both read from this one walk, so
/// they cannot disagree about what the agent takes.
pub(super) fn input_summaries(graph: &RunGraph) -> Vec<InputSummary> {
    graph
        .inputs
        .iter()
        .map(|decl| InputSummary {
            key: decl.name.to_string(),
            kind: decl.ty.describe(),
            regions: decl
                .binds
                .iter()
                .filter_map(|slot| match slot {
                    InputSlot::Region(binding) => Some(binding.region.to_string()),
                    _ => None,
                })
                .collect(),
            required: decl.required && decl.default.is_none(),
        })
        .collect()
}

/// The "Inputs:" lines of the report, answering at validate time what `lev
/// run` would otherwise only reveal by refusing at spawn: which inputs this
/// agent takes, and explicitly that `--task` is not among them when the
/// graph declares no `task` input.
pub(super) fn input_lines(graph: &RunGraph) -> Vec<String> {
    let inputs = input_summaries(graph);
    if inputs.is_empty() {
        return vec![
            "  Inputs: none - this agent takes no --task or other caller input".to_string(),
        ];
    }
    let flags: Vec<String> = inputs
        .iter()
        .map(|i| {
            let mut notes = vec![i.kind.clone()];
            if i.required {
                notes.push("required".to_string());
            }
            notes.extend(
                i.regions
                    .iter()
                    .filter(|r| **r != i.key)
                    .map(|r| format!("seeds region '{r}'")),
            );
            format!("--{} ({})", i.key, notes.join(", "))
        })
        .collect();
    let mut lines = vec![format!("  Inputs: {}", flags.join(", "))];
    if !inputs.iter().any(|i| i.key == "task") {
        lines.push(format!(
            "  Note: this agent takes no --task; give it input via {}",
            inputs
                .iter()
                .map(|i| format!("--{}", i.key))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines
}

/// The mime type patterns `stage` takes as parts: its own `input_accepts`
/// when it declares one, else the union of `accepts` across the regions it
/// sees. Text is always taken and never listed. A visible region with no
/// `accepts` takes anything, and is reported as `*/*`.
fn stage_takes(graph: &RunGraph, stage: &StageDef) -> Vec<String> {
    if !stage.input_accepts.is_empty() {
        return stage
            .input_accepts
            .iter()
            .map(ToString::to_string)
            .collect();
    }
    let mut out: Vec<String> = Vec::new();
    for region in &graph.layout_for(stage).regions {
        if stage.hide.contains(&region.name) {
            continue;
        }
        let patterns: Vec<String> = match region.accepts.is_empty() {
            true => vec!["*/*".to_string()],
            false => region.accepts.iter().map(ToString::to_string).collect(),
        };
        for p in patterns {
            if !p.starts_with("text/") && !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// One line per stage that takes mime or hands back declared artifacts:
/// what `lev run --attach` may aim at it, and what `lev result` will list.
pub(super) fn mime_lines(graph: &RunGraph) -> Vec<String> {
    let mut lines = Vec::new();
    if !graph.mime_types.is_empty() {
        let rows: Vec<String> = graph
            .mime_types
            .iter()
            .map(|(key, row)| match &row.check {
                Some(CodeRef::File(check)) => format!("{key} (check {check})"),
                Some(CodeRef::Inline(_)) => format!("{key} (check inline)"),
                None => key.to_string(),
            })
            .collect();
        lines.push(format!(
            "  Mime types: adds {} row{} for its runs: {}",
            rows.len(),
            match rows.len() {
                1 => "",
                _ => "s",
            },
            rows.join(", ")
        ));
    }
    for stage in &graph.stages {
        let takes: Vec<String> = stage_takes(graph, stage)
            .into_iter()
            .filter(|p| p != "*/*")
            .collect();
        let hands_back: Vec<String> = stage
            .output
            .iter()
            .flat_map(|o| &o.artifacts)
            .map(|a| {
                format!(
                    "{} ({}{})",
                    a.name,
                    a.mime_type,
                    if a.required { ", required" } else { "" }
                )
            })
            .collect();
        let limits: Vec<String> = stage
            .tool_accepts
            .iter()
            .map(|(tool, list)| {
                let list: Vec<&str> = list.iter().map(|m| m.as_str()).collect();
                format!("{tool} to [{}]", list.join(", "))
            })
            .collect();
        if takes.is_empty() && hands_back.is_empty() && limits.is_empty() {
            continue;
        }
        let mut parts = Vec::new();
        if !takes.is_empty() {
            parts.push(format!("takes {}", takes.join(", ")));
        }
        if !stage.input_as_text.is_empty() {
            let as_text: Vec<&str> = stage.input_as_text.iter().map(|m| m.as_str()).collect();
            parts.push(format!("as text: {}", as_text.join(", ")));
        }
        if !hands_back.is_empty() {
            parts.push(format!("hands back {}", hands_back.join(", ")));
        }
        if !limits.is_empty() {
            parts.push(format!("limits {}", limits.join(", ")));
        }
        lines.push(format!(
            "  Mime, stage '{}': {}",
            stage.name,
            parts.join("; ")
        ));
    }
    lines
}

/// How an edge's condition reads beside its target, when it is not the
/// ordinary one.
fn when_label(when: EdgeCondition) -> Option<&'static str> {
    match when {
        EdgeCondition::Always => None,
        EdgeCondition::Error => Some("error"),
        EdgeCondition::MaxIterations => Some("max_iterations"),
        EdgeCondition::LlmChoice => Some("llm_choice"),
        EdgeCondition::DeadEnd => Some("dead_end"),
        EdgeCondition::Stuck => Some("stuck"),
    }
}

/// The graph's shape, one line per stage: where each edge leaving it goes
/// (with its condition when it is not the ordinary one), `(terminal)` for a
/// stage nothing leaves, and the revisit cap.
pub(super) fn graph_lines(graph: &RunGraph) -> Vec<String> {
    let entry = graph
        .entry_stage()
        .map(|s| s.name.to_string())
        .unwrap_or_default();
    let mut lines = vec![format!("  Entry stage: '{entry}'")];
    for stage in &graph.stages {
        let targets: Vec<String> = graph
            .edges_from(stage.name.as_str())
            .map(|e| match when_label(e.when) {
                Some(label) => format!("{} [{label}]", e.to),
                None => e.to.to_string(),
            })
            .collect();
        let edges = match targets.is_empty() {
            true => " (terminal)".to_string(),
            false => format!(" → {}", targets.join(", ")),
        };
        let revisits = stage
            .max_revisits
            .map(|n| format!(" (max_revisits: {n})"))
            .unwrap_or_default();
        lines.push(format!("  - {}{edges}{revisits}", stage.name));
    }
    lines
}

/// The "valid blueprint" lines of the report: what it is, what it takes,
/// what it hands back, and its shape.
pub(super) fn success_lines(checked: &Checked) -> Vec<String> {
    let meta = &checked.file.blueprint;
    let graph = &checked.graph;
    let mut lines = vec![
        format!("✓ Blueprint '{}' is valid.", meta.name),
        format!("  {} stages, version {}", graph.stages.len(), meta.version),
    ];
    lines.extend(input_lines(graph));
    lines.extend(mime_lines(graph));
    lines.extend(graph_lines(graph));
    lines
}

#[cfg(test)]
#[path = "validate_toml_tests.rs"]
pub(super) mod tests;
