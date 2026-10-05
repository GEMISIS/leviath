//! Run directories laid down for tests: a run file whose spec and state read
//! back as the record a test describes.
//!
//! A test describes a run the way the readers report one, as a [`RunMeta`],
//! a [`ContextSnapshot`] or a list of [`StageRecord`]s. Each writer here turns
//! that into what the daemon would have recorded: a spec and a state in the
//! run's file. A field the run file has no room for (`pid`, `agent_path`,
//! `read_paths`, `title_error`, a `Starting` status) reads back as the run
//! file says it, the same as for a real run.
//!
//! A write that changes only the state is one more step on the file. One that
//! changes the spec (a different workdir, a region the graph did not declare)
//! starts the file again from the new spec, with the state carried over.

use std::collections::BTreeMap;
use std::path::Path;

use leviath_core::run_meta::{ContextSnapshot, RunMeta, RunStatus, StageRecord};
use leviath_runtime::runfile::{CheckpointPolicy, RunFileWriter};
use leviath_runtime::spec::graph::{RegionDef, RunGraph};
use leviath_runtime::spec::launch::{Callback, Delivery, LaunchPolicy, Placement, Secret};
use leviath_runtime::spec::names::{
    BlueprintName, BlueprintRef, Digest, HttpUrl, ModelId, ModelRef, ProviderName, RegionName,
    RunId, StageName,
};
use leviath_runtime::spec::run_spec::{
    AutoAnswers, EnvFingerprint, RunSpec, SeededContent, SpecOrigin, StagePlan,
};
use leviath_runtime::state::{
    Clock, ContextState, FinalOutputState, Flags, PipelinePhase, RunState, RunStatus as State,
    Spend, StageStatus, Totals,
};

use super::run_dir;

/// The name of the stage at `index` of a fixture's graph, when the record
/// names no stage there.
fn filler(index: usize) -> String {
    format!("stage{index}")
}

/// The graph a record describes: as many stages as it says, with the one it
/// is in at its index, and the regions of `regions` (kept from an earlier
/// write) declared.
fn graph_of(meta: &RunMeta, title: Option<String>, regions: Vec<RegionDef>) -> RunGraph {
    let count = meta.num_stages.max(meta.stage_index + 1);
    let current = StageName::new(meta.current_stage.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|_| filler(meta.stage_index));
    let stages: Vec<serde_json::Value> = (0..count)
        .map(|i| match i == meta.stage_index {
            true => serde_json::json!({ "name": current }),
            false => serde_json::json!({ "name": filler(i) }),
        })
        .collect();
    let mut graph: RunGraph = serde_json::from_value(serde_json::json!({
        "title": title,
        "stages": stages,
        "layout": { "total_budget_tokens": 100_000, "regions": [] },
    }))
    .expect("a fixture graph reads");
    graph.layout.regions = regions;
    graph
}

/// What a record says the run started from: the blueprint directory its
/// `agent_path` names when it names one, an installed blueprint by its name
/// (read from the file `agent_path` names) when the name is one, else a graph
/// titled with it.
fn origin_of(meta: &RunMeta) -> (SpecOrigin, Option<String>) {
    let file = std::path::Path::new(&meta.agent_path);
    let dir = match file.file_name().and_then(|n| n.to_str()) {
        Some(leviath_core::files::BLUEPRINT_MANIFEST) => file.parent().unwrap_or(file),
        _ => file,
    };
    let path = dir
        .is_dir()
        .then(|| leviath_runtime::spec::names::BlueprintPath::new(dir.to_string_lossy()).ok())
        .flatten();
    if let (Some(path), Ok(name)) = (path, BlueprintName::new(meta.agent_name.as_str())) {
        return (
            SpecOrigin::BlueprintFile {
                path,
                name,
                digest: meta
                    .blueprint_digest
                    .as_deref()
                    .and_then(|d| Digest::new(d).ok()),
                version: "0.0.0".to_string(),
            },
            None,
        );
    }
    match BlueprintName::new(meta.agent_name.as_str()) {
        Ok(name) => (
            SpecOrigin::Blueprint {
                blueprint: BlueprintRef {
                    name,
                    digest: meta
                        .blueprint_digest
                        .as_deref()
                        .and_then(|d| Digest::new(d).ok()),
                },
                version: "0.0.0".to_string(),
                manifest: meta.agent_path.clone(),
            },
            None,
        ),
        Err(_) => (SpecOrigin::Raw, Some(meta.agent_name.clone())),
    }
}

/// The spec a record describes, declaring `regions`.
fn spec_of(meta: &RunMeta, regions: Vec<RegionDef>) -> RunSpec {
    let (origin, title) = origin_of(meta);
    let graph = graph_of(meta, title, regions);
    let stages = meta
        .model
        .as_deref()
        .and_then(|model| {
            let (provider, model) = model.split_once('/')?;
            Some(StagePlan {
                stage: graph.stages[0].name.clone(),
                provider: ProviderName::new(provider).ok()?,
                model: ModelId::new(model).ok()?,
                context_window: 100_000,
                max_output_tokens: None,
                fallbacks: Vec::new(),
                tools: Vec::new(),
                output: None,
                region_budgets: BTreeMap::new(),
                notes: Vec::new(),
            })
        })
        .into_iter()
        .collect();
    let mut seeded = BTreeMap::new();
    if !meta.task.is_empty() {
        seeded.insert(
            RegionName::new("task").expect("a region name"),
            SeededContent {
                text: meta.task.clone(),
                parts: Vec::new(),
            },
        );
    }
    RunSpec {
        run_id: RunId::new(meta.run_id.as_str()).expect("a fixture's run id is a valid id"),
        origin,
        graph,
        inputs: Default::default(),
        stages,
        seeded,
        code: Vec::new(),
        requested_output: meta
            .output_request
            .as_ref()
            .and_then(|o| leviath_runtime::spec::graph::OutputDef::from_output_spec(o).ok()),
        requested_model: meta
            .model_override
            .as_deref()
            .and_then(|m| ModelRef::parse(m).ok()),
        launch: LaunchPolicy {
            unattended: meta.unattended.clone(),
            allow: Vec::new(),
            max_depth: u8::try_from(meta.max_child_depth).unwrap_or(u8::MAX),
            seed_commands: true,
            capture_model_input: false,
        },
        auto_answers: AutoAnswers::default(),
        placement: Placement {
            workdir: meta.workdir.clone().into(),
            parent: meta
                .parent_run_id
                .as_deref()
                .and_then(|p| RunId::new(p).ok()),
            depth: u8::try_from(meta.depth).unwrap_or(u8::MAX),
            worker_stage: None,
            work_item: None,
        },
        delivery: Delivery {
            callback: meta.callback_url.as_deref().and_then(|url| {
                Some(Callback {
                    url: HttpUrl::new(url).ok()?,
                    secret: meta.callback_secret.clone().map(Secret::new),
                })
            }),
            metadata: meta
                .metadata
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        },
        env: EnvFingerprint::default(),
        created_at: meta.started_at,
        listed: None,
    }
}

/// The state a record describes, on top of `base` (what an earlier write
/// left: the window, the ledger).
fn state_of(meta: &RunMeta, spec: &RunSpec, base: Option<RunState>) -> RunState {
    let stage = spec.graph.stages[meta.stage_index].name.clone();
    let mut state =
        base.unwrap_or_else(|| RunState::initial(stage.clone(), ContextState::default(), true));
    let status = match meta.status {
        RunStatus::Starting => State::Idle,
        RunStatus::Running => State::Active,
        RunStatus::WaitingInput => State::Waiting,
        RunStatus::Paused => State::Paused,
        RunStatus::Complete | RunStatus::CompleteInteractive => State::Complete,
        RunStatus::Error => State::Error(meta.error.clone().unwrap_or_default()),
        RunStatus::Cancelled => State::Cancelled,
    };
    state.phase = match &status {
        State::Complete | State::Error(_) | State::Cancelled => PipelinePhase::Done,
        _ => PipelinePhase::ReadyToInfer,
    };
    state.status = status;
    // A record that names its stage was in it; one that names none entered
    // no stage.
    if !meta.current_stage.is_empty() {
        state.visits.entry(stage.clone()).or_insert(1);
    }
    state.cursor.stage = stage;
    state.cursor.iteration = u32::try_from(meta.iteration).unwrap_or(u32::MAX);
    let n = |x: usize| x as u64;
    let (priced_usd, unpriced_calls) = match meta.cost_usd {
        Some(usd) => (usd, 0),
        None => (meta.cost_priced_usd, meta.unpriced_calls.max(1)),
    };
    state.totals = Totals {
        spend: Spend {
            prompt_tokens: n(meta.prompt_tokens),
            completion_tokens: n(meta.completion_tokens),
            cached_tokens: n(meta.cached_tokens),
            cache_write_tokens: n(meta.cache_write_tokens),
            priced_usd,
            reported_calls: 0,
            computed_calls: u32::from(!meta.cost_is_exact),
            unpriced_calls: unpriced_calls as u32,
            cost_unknown: false,
        },
        tool_calls: n(meta.tool_calls),
    };
    state.clock = meta.active.map_or(Clock::default(), |a| Clock {
        banked_secs: a.banked_secs,
        since: a.since,
    });
    let f = &meta.flags;
    state.flags = Flags {
        modified_files: f.modified_files.clone(),
        modified_file_count: f.modified_file_count as u32,
        empty_output: f.empty_output,
        no_output_tools: f.no_output_tools,
        searches_run: f.searches_run as u32,
        searches_empty: f.searches_empty as u32,
        max_iterations_hit: f.max_iterations_hit as u32,
        gates_forced: f.gates_forced as u32,
        required_regions_abandoned: f.required_regions_abandoned.clone(),
        workspace_lost: f.workspace_lost,
        produced_output: f.produced_output,
        output_forced: f.output_forced as u32,
        splits_degraded: f.splits_degraded as u32,
        broken_scripts: f.broken_scripts.clone(),
    };
    state.children = meta
        .children
        .iter()
        .filter_map(|c| RunId::new(c.as_str()).ok())
        .collect();
    state.title = meta.title.clone();
    state.wait_reason = meta
        .waiting_on
        .as_ref()
        .map(leviath_runtime::state::WaitState::from);
    // The models a record says the run used live on its ledger.
    if state.ledger.is_empty() && !meta.stage_models.is_empty() {
        state.ledger.push(leviath_runtime::state::StageRecord {
            stage: state.cursor.stage.clone(),
            status: StageStatus::Active,
            entered: true,
            spend: Spend::default(),
            models: meta
                .stage_models
                .iter()
                .filter_map(|m| {
                    Some(ModelRef {
                        provider: ProviderName::new(m.provider.as_str()).ok(),
                        model: ModelId::new(m.model.as_str()).ok()?,
                    })
                })
                .collect(),
            visits: Vec::new(),
            region_tokens: BTreeMap::new(),
            first_call_prompt_tokens: None,
            runaway_warned: false,
            output_cap_raised: false,
            started_at: None,
            ended_at: None,
            clock: Clock::default(),
        });
    }
    state.final_output = meta.final_output.as_ref().map(|out| FinalOutputState {
        bytes: out.bytes as u64,
        format: out.format.clone(),
        stage: StageName::new(out.stage.as_str()).unwrap_or_else(|_| state.cursor.stage.clone()),
        submitted_at: out.submitted_at,
        truncated: out.truncated,
        artifacts: out
            .artifacts
            .iter()
            .map(|a| leviath_runtime::state::journal::ArtifactState {
                name: a.name.clone(),
                path: a.path.clone(),
                mime_type: a.mime_type.as_str().to_string(),
                size: a.size,
                sha256: a.sha256.clone(),
            })
            .collect(),
    });
    state
}

/// The spec and state the run file in `dir` holds now, or `None` when there
/// is none.
fn recorded(dir: &Path) -> Option<(RunSpec, RunState)> {
    let reader = super::run_file::open_in(dir).ok()?;
    let state = reader.latest_state().ok()?;
    Some((reader.spec().clone(), state))
}

/// Write `spec` and `state` as the run file in `dir`, as one more step
/// stamped `at` when the file already holds that spec, or as a new file.
fn write(dir: &Path, spec: &RunSpec, state: RunState, at: i64) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = super::run_file::path_in(dir);
    let same_spec = recorded(dir).is_some_and(|(old, _)| old == *spec);
    let mut writer = match same_spec {
        true => RunFileWriter::open(&path, CheckpointPolicy::default())?,
        false => {
            let _ = leviath_sys::secure_dir_perms(dir);
            let mut start = state.clone();
            start.seq = 0;
            let writer = RunFileWriter::create(
                &path,
                spec,
                &Default::default(),
                &start,
                CheckpointPolicy::default(),
            )?;
            if at == spec.created_at {
                return Ok(());
            }
            writer
        }
    };
    // A step that changes nothing is still a step: it is when the record
    // says the run last moved.
    let mut delta = leviath_runtime::state::StateDelta::between(writer.state(), &state, at, vec![]);
    delta.seq = writer.seq() + 1;
    writer.append_delta(&delta)?;
    Ok(())
}

/// Name, in the run file in `dir`, the files `edit` names beside it, as one
/// more step: what the persistence lane does as it writes them. A directory
/// with no run file is left alone.
pub(crate) fn name_files(dir: &Path, edit: impl FnOnce(&mut leviath_runtime::state::RunFiles)) {
    let Ok(mut writer) =
        RunFileWriter::open(&super::run_file::path_in(dir), CheckpointPolicy::default())
    else {
        return;
    };
    let mut next = writer.state().clone();
    edit(&mut next.files);
    let at = writer.state().seq as i64 + 1;
    writer
        .record(next, at, Vec::new())
        .expect("a test's run file takes a step");
}

/// Create the run directory and lay down the run `meta` describes.
pub(crate) fn create_run(meta: &RunMeta) -> anyhow::Result<()> {
    create_run_in(&run_dir(&meta.run_id), meta)
}

/// [`create_run`] into an explicit run directory.
pub(crate) fn create_run_in(dir: &Path, meta: &RunMeta) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(super::run_file::path_in(dir));
    write_meta_to(dir, meta)
}

/// Record the run `meta` describes, over what its file already holds.
pub(crate) fn write_meta(meta: &RunMeta) -> anyhow::Result<()> {
    write_meta_to(&run_dir(&meta.run_id), meta)
}

/// [`write_meta`] into an explicit run directory.
pub(crate) fn write_meta_to(dir: &Path, meta: &RunMeta) -> anyhow::Result<()> {
    let before = recorded(dir);
    let regions = before
        .as_ref()
        .map(|(spec, _)| spec.graph.layout.regions.clone())
        .unwrap_or_default();
    let spec = spec_of(meta, regions);
    let state = state_of(meta, &spec, before.map(|(_, state)| state));
    write(dir, &spec, state, meta.updated_at)
}

/// The run record a fixture writes when a test lays down only a window or a
/// ledger for `run_id`.
fn record_for(run_id: &str) -> RunMeta {
    RunMeta::new(
        run_id.to_string(),
        "fixture".to_string(),
        String::new(),
        String::new(),
        None,
        "/work".to_string(),
        1,
    )
}

/// The record the run in `dir` reads as, or a plain one for `run_id`.
fn meta_in(dir: &Path, run_id: &str) -> RunMeta {
    super::read_meta_from(dir).unwrap_or_else(|_| record_for(run_id))
}

/// A region as a graph declares it, for a window region of `kind`, budget
/// `max_tokens`. `None` for one the run shapes the same undeclared (a pinned
/// region, the runtime's own) or a custom one, which a fixture leaves
/// undeclared so the spec, and with it the run's history, stays as it was.
fn region_def(name: &str, kind: &str, max_tokens: usize) -> Option<RegionDef> {
    if matches!(name, "conversation" | "tool_results") {
        return None;
    }
    let kind = match kind {
        "temporary" | "clearable" | "checklist" | "compact_history" => serde_json::json!(kind),
        "sliding_window" => serde_json::json!({ "kind": "sliding_window", "max_items": 50 }),
        "compacting" => serde_json::json!({ "kind": "compacting" }),
        "keyed" => serde_json::json!({ "kind": "keyed" }),
        _ => return None,
    };
    serde_json::from_value(serde_json::json!({
        "name": name,
        "kind": kind,
        "budget": max_tokens,
    }))
    .ok()
}

/// Record `snap` as the run's window as of its last step.
pub(crate) fn write_context_snapshot(run_id: &str, snap: &ContextSnapshot) -> anyhow::Result<()> {
    let dir = run_dir(run_id);
    let meta = meta_in(&dir, run_id);
    let (old_spec, old_state) = recorded(&dir).unwrap_or_else(|| {
        let spec = spec_of(&meta, Vec::new());
        let state = state_of(&meta, &spec, None);
        (spec, state)
    });
    let mut regions = old_spec.graph.layout.regions.clone();
    for region in &snap.regions {
        if regions.iter().all(|r| r.name.as_str() != region.name)
            && let Some(def) = region_def(&region.name, &region.kind, region.max_tokens)
        {
            regions.push(def);
        }
    }
    let mut spec = old_spec;
    spec.graph.layout.regions = regions;
    let mut window = leviath_runtime::ContextWindow::new(snap.max_tokens);
    for region in &snap.regions {
        let mut live = leviath_core::Region::new(
            region.name.clone(),
            leviath_core::RegionKind::Pinned,
            region.max_tokens,
        );
        for entry in &region.entries {
            live.content.push(leviath_core::region::RegionEntry {
                content: entry.content.clone(),
                tokens: entry.tokens,
                timestamp: 0,
                metadata: entry.metadata.clone(),
                kind: entry.kind.clone(),
                key: entry.key.clone(),
                reasoning: entry.reasoning.clone(),
            });
        }
        live.current_tokens = region.current_tokens;
        window.add_region(live);
    }
    let mut state = old_state;
    state.context = leviath_runtime::state::inspect::context_of(&window);
    write(&dir, &spec, state, meta.updated_at)
}

/// Record `stages` as the run's per-stage ledger as of its last step.
pub(crate) fn write_stages_index(run_id: &str, stages: &[StageRecord]) -> anyhow::Result<()> {
    use leviath_core::run_meta::StageRunStatus as S;
    let dir = run_dir(run_id);
    let meta = meta_in(&dir, run_id);
    let (spec, mut state) = recorded(&dir).unwrap_or_else(|| {
        let spec = spec_of(&meta, Vec::new());
        let state = state_of(&meta, &spec, None);
        (spec, state)
    });
    let spend = |tokens: [usize; 4], usd: f64, calls: [usize; 3]| Spend {
        prompt_tokens: tokens[0] as u64,
        completion_tokens: tokens[1] as u64,
        cached_tokens: tokens[2] as u64,
        cache_write_tokens: tokens[3] as u64,
        priced_usd: usd,
        reported_calls: calls[0] as u32,
        computed_calls: calls[1] as u32,
        unpriced_calls: calls[2] as u32,
        cost_unknown: false,
    };
    state.ledger = stages
        .iter()
        .map(|r| leviath_runtime::state::StageRecord {
            stage: StageName::new(r.name.as_str()).expect("a fixture stage name"),
            status: match r.status {
                S::Pending => StageStatus::Pending,
                S::Active => StageStatus::Active,
                S::WaitingInput => StageStatus::WaitingInput,
                S::Paused => StageStatus::Paused,
                S::Complete => StageStatus::Complete,
                S::Error => StageStatus::Error,
                S::Cancelled => StageStatus::Cancelled,
                S::Skipped => StageStatus::Skipped,
            },
            entered: r.entered,
            spend: spend(
                [
                    r.prompt_tokens,
                    r.completion_tokens,
                    r.cached_tokens,
                    r.cache_write_tokens,
                ],
                r.cost_usd.unwrap_or(r.cost_priced_usd),
                [
                    r.reported_calls,
                    r.computed_calls.max(usize::from(!r.cost_is_exact)),
                    r.unpriced_calls.max(usize::from(r.cost_usd.is_none())),
                ],
            ),
            models: r
                .models
                .iter()
                .filter_map(|m| {
                    Some(ModelRef {
                        provider: ProviderName::new(m.provider.as_str()).ok(),
                        model: ModelId::new(m.model.as_str()).ok()?,
                    })
                })
                .collect(),
            visits: r
                .visits
                .iter()
                .map(|v| leviath_runtime::state::VisitRecord {
                    id: v.id.clone(),
                    entered_at: v.entered_at,
                    left_at: v.left_at,
                    spend: spend(
                        [
                            v.prompt_tokens,
                            v.completion_tokens,
                            v.cached_tokens,
                            v.cache_write_tokens,
                        ],
                        v.cost_usd.unwrap_or(v.cost_priced_usd),
                        [
                            v.reported_calls,
                            v.computed_calls.max(usize::from(!v.cost_is_exact)),
                            v.unpriced_calls.max(usize::from(v.cost_usd.is_none())),
                        ],
                    ),
                    clock: v.active.map_or(Clock::default(), |a| Clock {
                        banked_secs: a.banked_secs,
                        since: a.since,
                    }),
                })
                .collect(),
            region_tokens: r
                .region_tokens
                .iter()
                .map(|(k, v)| (k.clone(), *v as u64))
                .collect(),
            first_call_prompt_tokens: r.first_call_prompt_tokens.map(|t| t as u64),
            runaway_warned: r.runaway_warned,
            output_cap_raised: r.output_cap_raised,
            started_at: r.started_at,
            ended_at: r.ended_at,
            clock: r.active.map_or(Clock::default(), |a| Clock {
                banked_secs: a.banked_secs,
                since: a.since,
            }),
        })
        .collect();
    for r in stages {
        if let Ok(stage) = StageName::new(r.name.as_str()) {
            state.visits.insert(stage, r.visit_count as u32);
        }
    }
    write(&dir, &spec, state, meta.updated_at)
}
