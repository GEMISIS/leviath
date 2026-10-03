//! The run's spec, rebuilt from its metadata, its blueprint and its first
//! context snapshot.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::old::blueprint::Blueprint;
use crate::old::layout::RegionSeed;
use leviath_core::JsonDoc;
use leviath_core::output::OutputSpec;
use leviath_core::run_meta::{ContextSnapshot, RunMeta, RunStatus as OldStatus};
use leviath_runtime::spec::env::CodeFiles;
use leviath_runtime::spec::graph::{ArtifactDef, CodeRef, OutputDef, RunGraph};
use leviath_runtime::spec::inputs::{InputValue, InputValues};
use leviath_runtime::spec::launch::{
    Callback, Delivery, LaunchPolicy, Placement, Secret, Unattended,
};
use leviath_runtime::spec::names::{
    BlueprintName, BlueprintRef, Digest, HttpUrl, MimePattern, ModelRef, ProfileName, RegionName,
    RunId,
};
use leviath_runtime::spec::run_spec::{
    EnvFingerprint, ListedAs, RunSpec, SeededContent, SpecOrigin,
};

use crate::context::{Losses, parts};
use crate::legacy::LegacyRun;
use crate::manifest::{parse_manifest, read_manifest_tables, unread_keys};
use crate::report::{BlueprintSource, Dropped, Report};
use crate::{ConvertError, StageLookup};

/// A spec and the code its frames carry.
pub(crate) struct Built {
    pub(crate) spec: RunSpec,
    pub(crate) code: Vec<(Digest, Vec<u8>)>,
}

/// Where the run's graph was read from.
pub(crate) enum Source {
    /// Its blueprint.
    Blueprint(Box<Blueprint>),
    /// What the run recorded, because its blueprint could not be read as
    /// the graph it ran, and why.
    Recorded(String),
}

/// The run's blueprint, and the run graph read from it; or, when there is no
/// blueprint that reads as a run graph, the graph the run recorded.
pub(crate) fn graph(old: &LegacyRun, report: &mut Report) -> (Source, RunGraph) {
    let blueprint_path = match &old.blueprint.source {
        BlueprintSource::Snapshot => old.dir.join(crate::legacy::BLUEPRINT_SNAPSHOT_FILE),
        BlueprintSource::Installed(path) => {
            report.note(format!(
                "the run kept no copy of its blueprint; read the installed one at {}, which may have changed since the run started",
                path.display()
            ));
            path.clone()
        }
        BlueprintSource::Recorded { why, .. } => return recorded(old, why.clone(), report),
    };
    let shown = blueprint_path.display().to_string();
    match read_blueprint(&old.blueprint.text, blueprint_path) {
        Ok((blueprint, graph, notes, dropped)) => {
            match crate::recorded::not_what_it_ran(old, &graph) {
                None => {
                    for note in notes {
                        report.note(note);
                    }
                    report.dropped.extend(dropped);
                    (Source::Blueprint(Box::new(blueprint)), graph)
                }
                Some(why) => recorded(
                    old,
                    format!("the blueprint at {shown} is not the one the run ran: {why}"),
                    report,
                ),
            }
        }
        Err(e) => recorded(old, e.to_string(), report),
    }
}

/// The blueprint in `text`, read from `path`, its run graph, what
/// converting it had to change, and the keys it left out.
fn read_blueprint(
    text: &str,
    path: PathBuf,
) -> Result<(Blueprint, RunGraph, Vec<String>, Vec<Dropped>), ConvertError> {
    let blueprint = parse_manifest(text).map_err(|e| ConvertError::Unreadable {
        path,
        why: e.to_string(),
    })?;
    let (mut graph, notes) =
        crate::old::graph::from_blueprint_noted(&blueprint).map_err(ConvertError::Graph)?;
    read_manifest_tables(&mut graph, text).map_err(ConvertError::Graph)?;
    let dropped = unread_keys(&toml::from_str(text).expect("a manifest that parsed is TOML"));
    Ok((blueprint, graph, notes, dropped))
}

/// The graph the run recorded, used in place of its blueprint for `why`.
fn recorded(old: &LegacyRun, why: String, report: &mut Report) -> (Source, RunGraph) {
    report.note(format!(
        "the run's graph is what it recorded, and it never resumes, because {why}"
    ));
    let graph = crate::recorded::graph(old, &why, report);
    (Source::Recorded(why), graph)
}

pub(crate) fn build(
    old: &LegacyRun,
    lookup: Option<&dyn StageLookup>,
    report: &mut Report,
) -> Result<Built, ConvertError> {
    let meta = old.meta();
    for name in &old.stray_blobs {
        report.note(format!(
            "blobs/{name} is not named in the run file: a stored part is a file named by its digest"
        ));
    }
    let (source, graph) = graph(old, report);
    let name =
        BlueprintName::new(meta.agent_name.as_str()).map_err(ConvertError::name("agent_name"))?;
    let manifest = manifest(old);
    let (origin, binds) = match &source {
        Source::Blueprint(blueprint) => (
            SpecOrigin::Blueprint {
                blueprint: BlueprintRef {
                    name,
                    digest: pin(old, report),
                },
                version: blueprint.version.clone(),
                manifest,
            },
            caller_inputs(blueprint),
        ),
        Source::Recorded(why) => (
            SpecOrigin::Recorded {
                name,
                manifest,
                why: why.clone(),
            },
            vec![("task".to_string(), "task".to_string())],
        ),
    };
    // A run that finished, or whose graph is only what it recorded, never
    // runs again, so nothing about this machine is looked up for it: it keeps
    // the models it ran on.
    let resumable = matches!(source, Source::Blueprint(_)) && !finished(meta);
    if lookup.is_some() && !resumable {
        report.note("the run never runs again, so its stages keep the models it recorded and nothing was looked up on this machine");
    }
    let lookup = lookup.filter(|_| resumable);
    let first = old.first_context();
    let inputs = inputs(&graph, &binds, meta, first, report);
    let mut seeded = seeded(&graph, &binds, &inputs, first);
    // The task is what every earlier release listed the run under, whatever
    // the graph seeded with it (an old run's task region could carry its
    // stage's instructions too).
    if !meta.task.is_empty() {
        let task = RegionName::new("task").expect("`task` is a region name");
        seeded.entry(task).or_default().text = meta.task.clone();
    }
    let (mut code, mut code_frames) = code(&graph, old, report);
    let requested_model = requested_model(meta, report);
    // Bare `--yolo` answered everything; what a named profile answered was
    // never recorded, so such a run asks.
    let auto_answers = match meta.yolo && meta.yolo_profile.is_none() {
        true => leviath_runtime::spec::run_spec::AutoAnswers::all(),
        false => Default::default(),
    };
    let held: CodeFiles = code_frames.iter().cloned().collect();
    let workdir = Path::new(&meta.workdir);
    let (stages, found) = crate::plan::plan_all(
        &crate::plan::Stages {
            graph: &graph,
            ledger: &old.stages,
            context: &old.folded.context,
            requested: requested_model.as_ref(),
            launched: meta.model.as_deref().and_then(|m| ModelRef::parse(m).ok()),
            auto: auto_answers,
            lookup,
            code: &held,
            base: old.blueprint.script_dir.as_deref(),
            workdir: Some(workdir).filter(|w| !w.as_os_str().is_empty()),
        },
        report,
    );
    for (reference, bytes) in found {
        let digest = Digest::of(&bytes);
        if !code_frames.iter().any(|(d, _)| *d == digest) {
            code_frames.push((digest.clone(), bytes));
        }
        if !code.iter().any(|(c, _)| *c == reference) {
            code.push((reference, digest));
        }
    }
    let spec = RunSpec {
        run_id: RunId::new(meta.run_id.as_str()).map_err(ConvertError::name("run_id"))?,
        origin,
        inputs,
        stages,
        seeded,
        code,
        requested_output: meta.output_request.as_ref().map(|o| output(o, report)),
        requested_model,
        launch: launch(&graph, meta, lookup, report),
        auto_answers,
        placement: placement(meta, report)?,
        delivery: delivery(meta, report),
        env: EnvFingerprint::default(),
        created_at: meta.started_at,
        graph,
        listed: None,
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

/// What the release a run came from listed it with, as its `meta.json`
/// says, for the run as it stands at step `seq` of its run file.
pub(crate) fn listed(meta: &RunMeta, seq: u64) -> ListedAs {
    ListedAs {
        model: meta.model.clone(),
        num_stages: crate::context::n32(meta.num_stages),
        max_child_depth: crate::context::n32(meta.max_child_depth),
        blueprint_digest: meta.blueprint_digest.clone(),
        seq,
        last_progress_at: meta.last_progress_at,
        clock: meta.active.map(|a| leviath_runtime::state::Clock {
            banked_secs: a.banked_secs,
            since: a.since,
        }),
        empty_output: meta.flags.empty_output,
    }
}

/// Whether the run finished for good: complete, or failed. A cancelled run
/// can be resumed.
fn finished(meta: &RunMeta) -> bool {
    matches!(
        meta.status,
        OldStatus::Complete | OldStatus::CompleteInteractive | OldStatus::Error
    )
}

/// The blueprint file a run is listed with: the one its record names. A run
/// that was not finished was brought back from the copy of its blueprint it
/// kept, and every earlier release listed it with that copy while it was;
/// converted, the copy is under `legacy/`. One that kept none, whose record
/// names a manifest that is no longer there, is listed with the `agent.toml`
/// its agent was upgraded to.
fn manifest(old: &LegacyRun) -> String {
    let meta = old.meta();
    // A 0.1.0 worker's question is never reopened, so its run is not
    // brought back.
    let brought_back = old.question.is_none()
        && matches!(
            meta.status,
            OldStatus::Starting | OldStatus::Running | OldStatus::WaitingInput | OldStatus::Paused
        );
    let kept = match &old.blueprint.source {
        BlueprintSource::Snapshot => Some(
            old.dir
                .join(crate::write::LEGACY_DIR)
                .join(crate::legacy::BLUEPRINT_SNAPSHOT_FILE),
        ),
        _ => old
            .blueprint
            .migrated
            .as_ref()
            .map(|(path, _)| path.clone())
            .filter(|_| !Path::new(&meta.agent_path).is_file()),
    };
    match (brought_back, kept) {
        (true, Some(path)) => path.to_string_lossy().into_owned(),
        _ => meta.agent_path.clone(),
    }
}

/// The installed blueprint a fan-out worker of this run is started from.
/// The old manifest's digest can never match a file this build reads, so
/// the pin is the `agent.toml` it was migrated to.
fn pin(old: &LegacyRun, report: &mut Report) -> Option<Digest> {
    match &old.blueprint.migrated {
        Some((_, bytes)) => {
            let d = Digest::of(bytes);
            report.note(format!(
                "origin.blueprint.digest is the installed agent.toml's ({d}), which the run's workers are started from"
            ));
            Some(d)
        }
        None => {
            report.fill(
                "origin.blueprint.digest",
                "None",
                "no agent.toml is installed for the run's blueprint, so a worker it starts takes whichever one is installed then",
            );
            None
        }
    }
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

fn launch(
    graph: &RunGraph,
    meta: &RunMeta,
    lookup: Option<&dyn StageLookup>,
    report: &mut Report,
) -> LaunchPolicy {
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
    // A run that recorded no limit ran under its graph's, else the
    // operator's default.
    let tree = match meta.max_child_depth {
        0 => graph
            .max_child_depth
            .or_else(|| lookup.map(|l| l.default_max_depth(graph)))
            .map_or(0, usize::from),
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
