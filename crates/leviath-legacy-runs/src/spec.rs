//! The run's spec, rebuilt from its metadata, its blueprint and its first
//! context snapshot.

use std::collections::BTreeMap;

use leviath_core::JsonDoc;
use leviath_core::output::OutputSpec;
use leviath_core::run_meta::{ContextSnapshot, RunMeta, StageModelUse, StageRecord as OldStage};
use leviath_runtime::spec::Blueprint;
use leviath_runtime::spec::graph::{
    ArtifactDef, CodeRef, OutputCap, OutputDef, RunGraph, StageDef,
};
use leviath_runtime::spec::inputs::{InputValue, InputValues};
use leviath_runtime::spec::launch::{
    Callback, Delivery, LaunchPolicy, Placement, Secret, Unattended,
};
use leviath_runtime::spec::layout::RegionSeed;
use leviath_runtime::spec::manifest::parse_manifest;
use leviath_runtime::spec::names::{
    BlueprintName, BlueprintRef, Digest, HttpUrl, MimePattern, ModelId, ModelRef, ProfileName,
    ProviderName, RegionName, RunId,
};
use leviath_runtime::spec::run_spec::{
    EnvFingerprint, RunSpec, SeededContent, SpecOrigin, StagePlan,
};

use crate::ConvertError;
use crate::context::{Losses, n32, parts};
use crate::legacy::LegacyRun;
use crate::report::{BlueprintSource, Report};

/// Why the tool definitions are left out of every stage plan.
const NO_TOOL_DEFS: &str = "an old run kept the names of its tools but not their definitions";

/// A spec and the code its frames carry.
pub(crate) struct Built {
    pub(crate) spec: RunSpec,
    pub(crate) code: Vec<(Digest, Vec<u8>)>,
}

pub(crate) fn build(old: &LegacyRun, report: &mut Report) -> Result<Built, ConvertError> {
    let meta = old.meta();
    for name in &old.stray_blobs {
        report.note(format!(
            "blobs/{name} was left out: a stored part is named by its digest"
        ));
    }
    let blueprint_path = match &old.blueprint.source {
        BlueprintSource::Snapshot => old.dir.join(leviath_core::files::BLUEPRINT_SNAPSHOT_FILE),
        BlueprintSource::Installed(path) => {
            report.note(format!(
                "the run kept no copy of its blueprint; read the installed one at {}, which may have changed since the run started",
                path.display()
            ));
            path.clone()
        }
    };
    let blueprint = parse_manifest(&old.blueprint.text).map_err(|e| ConvertError::Unreadable {
        path: blueprint_path,
        why: e.to_string(),
    })?;
    let graph = RunGraph::from_blueprint(&blueprint).map_err(ConvertError::Graph)?;
    let digest = match meta.blueprint_digest.as_deref().map(Digest::new) {
        Some(Ok(d)) => d,
        _ => {
            let d = Digest::of(old.blueprint.text.as_bytes());
            report.fill(
                "origin.blueprint.digest",
                &d,
                "the run did not record its blueprint's digest; this is the digest of the blueprint read",
            );
            d
        }
    };
    let origin = SpecOrigin::Blueprint {
        blueprint: BlueprintRef {
            name: BlueprintName::new(meta.agent_name.as_str())
                .map_err(ConvertError::name("agent_name"))?,
            digest: Some(digest),
        },
        version: blueprint.version.clone(),
    };
    let first = old.first_context();
    let binds = caller_inputs(&blueprint);
    let inputs = inputs(&graph, &binds, meta, first, report);
    let seeded = seeded(&graph, &binds, &inputs, first);
    let stages = graph
        .stages
        .iter()
        .map(|s| plan(s, &old.stages, &old.folded.context, report))
        .collect();
    report.fill(
        "stages.*.region_budgets",
        "the budgets of the stage the run was last in",
        "an old run recorded region budgets only for the stage it was in",
    );
    report.fill("stages.*.tools", "[]", NO_TOOL_DEFS);
    let (code, code_frames) = code(&graph, old, report);
    let spec = RunSpec {
        run_id: RunId::new(meta.run_id.as_str()).map_err(ConvertError::name("run_id"))?,
        origin,
        inputs,
        stages,
        seeded,
        code,
        requested_output: meta.output_request.as_ref().map(|o| output(o, report)),
        requested_model: requested_model(meta, report),
        launch: launch(&graph, meta, report),
        placement: placement(meta, report)?,
        delivery: delivery(meta, report),
        env: EnvFingerprint::default(),
        created_at: meta.started_at,
        graph,
    };
    report.fill(
        "env",
        "empty",
        "an old run never recorded what it relied on from the machine; a resume treats an empty fingerprint as unknown and does not compare it",
    );
    Ok(Built {
        spec,
        code: code_frames,
    })
}

/// Each caller input of the blueprint and a region it seeds, as
/// `(input, region)`.
fn caller_inputs(blueprint: &Blueprint) -> Vec<(String, String)> {
    let stage_layouts = blueprint
        .stages
        .iter()
        .filter_map(|s| s.context_layout.as_ref());
    std::iter::once(&blueprint.context_layout)
        .chain(stage_layouts)
        .flat_map(|l| &l.regions)
        .filter_map(|r| match &r.seed {
            Some(RegionSeed::CallerInput { name }) => Some((name.clone(), r.name.clone())),
            _ => None,
        })
        .collect()
}

/// The entries of `region` in `snapshot`, as one text.
fn region_text(snapshot: &ContextSnapshot, region: &str) -> Option<String> {
    let r = snapshot.regions.iter().find(|r| r.name == region)?;
    let texts: Vec<&str> = r.entries.iter().map(|e| e.content.as_str()).collect();
    Some(texts.join("\n")).filter(|t| !t.is_empty())
}

/// The inputs: `task` is the old run's task, and every other input is read
/// back from the region it seeded, as the first snapshot shows it.
fn inputs(
    graph: &RunGraph,
    binds: &[(String, String)],
    meta: &RunMeta,
    first: Option<&ContextSnapshot>,
    report: &mut Report,
) -> InputValues {
    let mut out = BTreeMap::new();
    for decl in &graph.inputs {
        let field = format!("inputs.{}", decl.name);
        if decl.name.as_str() == "task" {
            out.insert(decl.name.clone(), InputValue::Text(meta.task.clone()));
            continue;
        }
        let seeded = binds
            .iter()
            .filter(|(input, _)| input == decl.name.as_str())
            .find_map(|(_, region)| first.and_then(|s| region_text(s, region)));
        match seeded {
            Some(text) => {
                report.note(format!("{field} was read back from the region it seeded"));
                out.insert(decl.name.clone(), InputValue::Text(text));
            }
            None => report.fill(
                field,
                "(none)",
                "the caller's value was not recorded and its region was empty",
            ),
        }
    }
    if !graph.inputs.iter().any(|d| d.name.as_str() == "task") && !meta.task.is_empty() {
        report.note("the run's task is not an input of its graph, so the spec does not carry it");
    }
    report.fill(
        "launch.regions",
        "{}",
        "an old run did not record the region seeds it was launched with, only its task",
    );
    report.fill(
        "launch.parts",
        "[]",
        "an old run did not record the files it was launched with",
    );
    InputValues(out)
}

/// What each seeded or input-bound region held at spawn, as the first
/// snapshot shows it.
fn seeded(
    graph: &RunGraph,
    binds: &[(String, String)],
    inputs: &InputValues,
    first: Option<&ContextSnapshot>,
) -> BTreeMap<RegionName, SeededContent> {
    let bound = |region: &str| {
        binds
            .iter()
            .any(|(input, r)| r == region && inputs.get(input).is_some())
    };
    let mut out = BTreeMap::new();
    let mut losses = Losses::default();
    for r in &graph.layout.regions {
        if r.seed.is_none() && !bound(r.name.as_str()) {
            continue;
        }
        let Some(snap) = first.and_then(|s| s.regions.iter().find(|x| x.name == r.name.as_str()))
        else {
            continue;
        };
        let texts: Vec<&str> = snap.entries.iter().map(|e| e.content.as_str()).collect();
        let content = SeededContent {
            text: texts.join("\n"),
            parts: snap
                .entries
                .iter()
                .flat_map(|e| parts(&e.content, &mut losses))
                .collect(),
        };
        out.insert(r.name.clone(), content);
    }
    out
}

/// A model the stage ledger recorded, as a checked reference.
pub(crate) fn ledger_model(m: &StageModelUse) -> Option<ModelRef> {
    ModelRef::parse(&format!("{}/{}", m.provider, m.model)).ok()
}

/// The provider and model a stage ran on: the last one its ledger recorded,
/// else the first its graph names.
fn model_of(stage: &StageDef, ledger: &[OldStage]) -> Option<ModelRef> {
    let used = ledger
        .iter()
        .find(|r| r.name == stage.name.as_str())
        .and_then(|r| r.models.last())
        .and_then(ledger_model);
    used.or_else(|| stage.model.models.first().cloned())
}

fn plan(
    stage: &StageDef,
    ledger: &[OldStage],
    context: &ContextSnapshot,
    report: &mut Report,
) -> StagePlan {
    let window = context.max_tokens;
    let at = format!("stages.{}", stage.name);
    let (provider, model) = match model_of(stage, ledger) {
        Some(ModelRef {
            provider: Some(provider),
            model,
        }) => (provider, model),
        _ => {
            report.fill(
                format!("{at}.model"),
                "unknown/unknown",
                "the stage never ran and its graph names no provider",
            );
            (
                ProviderName::new("unknown").expect("a plain word is a provider name"),
                ModelId::new("unknown").expect("a plain word is a model id"),
            )
        }
    };
    report.fill(
        format!("{at}.context_window"),
        window,
        "an old run did not record its models' windows; this is the context budget it last ran with",
    );
    let max_output_tokens = match &stage.model.params.max_output_tokens {
        Some(OutputCap::Tokens(n)) => Some(*n),
        Some(_) => {
            report.fill(
                format!("{at}.max_output_tokens"),
                "None",
                "the cap was relative to a window the old run did not record",
            );
            None
        }
        None => None,
    };
    let current = ModelRef {
        provider: Some(provider.clone()),
        model: model.clone(),
    };
    StagePlan {
        stage: stage.name.clone(),
        fallbacks: stage
            .model
            .models
            .iter()
            .filter(|m| **m != current)
            .cloned()
            .collect(),
        provider,
        model,
        context_window: n32(window),
        max_output_tokens,
        tools: Vec::new(),
        output: stage.output.clone(),
        region_budgets: context
            .regions
            .iter()
            .filter_map(|r| Some((RegionName::new(r.name.as_str()).ok()?, n32(r.max_tokens))))
            .collect(),
        notes: vec!["converted from an old run directory".to_string()],
    }
}

/// Every script the graph names, found by walking its serialized form for
/// [`CodeRef::File`] values, with the bytes of each one that can be read.
/// A graph read from a blueprint names code only by file.
type Code = (Vec<(CodeRef, Digest)>, Vec<(Digest, Vec<u8>)>);

fn code(graph: &RunGraph, old: &LegacyRun, report: &mut Report) -> Code {
    let tagged =
        serde_json::to_value(CodeRef::File(String::new())).expect("a code reference is plain JSON");
    let key = tagged
        .as_object()
        .and_then(|m| m.keys().next())
        .expect("a code reference is tagged by its kind");
    let graph = serde_json::to_value(graph).expect("a run graph is plain JSON");
    let mut paths = Vec::new();
    walk(&graph, key, &mut paths);
    let mut pairs = Vec::new();
    let mut frames: Vec<(Digest, Vec<u8>)> = Vec::new();
    for path in paths {
        let read = old
            .blueprint
            .script_dir
            .as_ref()
            .and_then(|d| std::fs::read(d.join(&path)).ok());
        let Some(bytes) = read else {
            report.fill(
                format!("code.{path}"),
                "(missing)",
                "an old run read its scripts from the installed agent, and this one is not there",
            );
            continue;
        };
        let digest = Digest::of(&bytes);
        if !frames.iter().any(|(d, _)| *d == digest) {
            frames.push((digest.clone(), bytes));
        }
        pairs.push((CodeRef::File(path), digest));
    }
    (pairs, frames)
}

/// Collect every `{ <key>: "<path>" }` in `v`, once each.
fn walk(v: &serde_json::Value, key: &str, out: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(map) => {
            let path = map
                .get(key)
                .and_then(|p| p.as_str())
                .filter(|_| map.len() == 1);
            match path {
                Some(p) => {
                    if !out.iter().any(|o| o == p) {
                        out.push(p.to_string());
                    }
                }
                None => map.values().for_each(|x| walk(x, key, out)),
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|x| walk(x, key, out)),
        _ => {}
    }
}

/// The caller's output request, as an output definition.
fn output(o: &OutputSpec, report: &mut Report) -> OutputDef {
    OutputDef {
        format: o.format.clone(),
        instructions: o.instructions.clone(),
        example: o.example.clone(),
        schema: o.schema.clone().map(JsonDoc::new),
        validator: o.validator.clone().map(CodeRef::File),
        on_validator_error: o.on_validator_error,
        overwrite_artifacts: o.overwrite_artifacts,
        artifacts: o
            .artifacts
            .iter()
            .filter_map(|a| match MimePattern::new(a.mime_type.as_str()) {
                Ok(mime_type) => Some(ArtifactDef {
                    name: a.name.clone(),
                    mime_type,
                    required: a.required,
                    description: a.description.clone(),
                }),
                Err(e) => {
                    report.note(format!("requested artifact {:?} was left out: {e}", a.name));
                    None
                }
            })
            .collect(),
    }
}

fn requested_model(meta: &RunMeta, report: &mut Report) -> Option<ModelRef> {
    let text = meta.model_override.as_deref()?;
    ModelRef::parse(text)
        .inspect_err(|e| report.note(format!("the launch model {text:?} was left out: {e}")))
        .ok()
}

fn launch(graph: &RunGraph, meta: &RunMeta, report: &mut Report) -> LaunchPolicy {
    let unattended = match (&meta.yolo_profile, meta.yolo) {
        (Some(p), _) => match ProfileName::new(p.as_str()) {
            Ok(p) => Unattended::Profile(p),
            Err(_) => {
                report.fill(
                    "launch.unattended",
                    "Off",
                    format!("the yolo profile {p:?} is not a valid profile name"),
                );
                Unattended::Off
            }
        },
        (None, true) => Unattended::All,
        (None, false) => Unattended::Off,
    };
    let tree = match meta.max_child_depth {
        0 => graph.max_child_depth.map_or(0, usize::from),
        n => n,
    };
    let max_depth = u8::try_from(tree.saturating_sub(meta.depth)).unwrap_or(u8::MAX);
    report.fill(
        "launch.allow",
        "[]",
        "an old run did not record the tools it was allowed at launch",
    );
    report.fill(
        "launch.max_depth",
        max_depth,
        "an old run did not record a launch depth override; this is its tree depth less its own depth",
    );
    report.fill(
        "launch.seed_commands",
        "false",
        "a converted run never re-runs a command seed",
    );
    report.fill(
        "launch.capture_model_input",
        "false",
        "an old run did not record whether it captured model input",
    );
    LaunchPolicy {
        unattended,
        allow: Vec::new(),
        max_depth,
        seed_commands: false,
        capture_model_input: false,
    }
}

fn placement(meta: &RunMeta, report: &mut Report) -> Result<Placement, ConvertError> {
    let parent = meta
        .parent_run_id
        .as_deref()
        .map(RunId::new)
        .transpose()
        .map_err(ConvertError::name("parent_run_id"))?;
    report.fill(
        "placement.worker_stage",
        "None",
        "an old run did not record the parent stage a fan-out worker ran",
    );
    Ok(Placement {
        workdir: meta.workdir.clone().into(),
        parent,
        depth: u8::try_from(meta.depth).unwrap_or(u8::MAX),
        worker_stage: None,
    })
}

fn delivery(meta: &RunMeta, report: &mut Report) -> Delivery {
    let callback = meta
        .callback_url
        .as_deref()
        .and_then(|url| match HttpUrl::new(url) {
            Ok(url) => Some(Callback {
                url,
                secret: meta.callback_secret.clone().map(Secret::new),
            }),
            Err(e) => {
                report.note(format!("the callback {url:?} was left out: {e}"));
                None
            }
        });
    Delivery {
        callback,
        metadata: meta.metadata.clone().into_iter().collect(),
    }
}
