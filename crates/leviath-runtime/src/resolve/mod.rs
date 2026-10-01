//! Turning a [`SpawnRequest`] into a [`RunSpec`].
//!
//! The resolver is the one place a request meets the machine. It asks the
//! host everything through [`ResolveEnv`] (which blueprint, which model, which
//! tools, what a seed produces) and decides everything else itself: the
//! checked inputs, the slots they fill, the launch policy, the region budgets,
//! the output shape each stage asks for.
//!
//! It never stops at the first problem. Every check that does not depend on an
//! earlier one runs, and the caller gets the whole list as one
//! [`SpawnIssues`], each with a path, what was expected, what arrived and how
//! to fix it. An agent that spawns runs can then fix a request in one go.
//!
//! Paths in an issue read from the request's root: `inputs.depth`,
//! `attachments[1].data`, `source.raw.stages.plan.model`. An issue the host
//! reports through [`ResolveEnv`] carries a path relative to the thing it was
//! asked about (a stage's model, a stage's tools, a blueprint), and the
//! resolver puts that thing's own path in front of it.
//!
//! The steps, in order:
//!
//! 1. The source: an installed blueprint, or the caller's own graph.
//! 2. The graph's own consistency ([`RunGraph::validate`]). A graph that fails
//!    it is not resolved further, since every later step walks its names; the
//!    request-level checks below still run so their issues come back too.
//! 3. The attachments: unique names, the size limit, their types by the
//!    run's own mime registry (the machine's rows with the graph's on top).
//! 4. The inputs, checked against the graph's declarations, then the checks
//!    that need the attachments or the workdir.
//! 5. The launch policy, where the run sits, and what its unattended setting
//!    answers without a person.
//! 6. The inputs' slots applied to the graph, then the operator's defaults
//!    for whatever the graph leaves open (iteration ceilings, prompt hints,
//!    the nudge, taint tracking).
//! 7. Every piece of code the graph names, read and checked once.
//! 8. The graph's dependencies, and the compaction model.
//! 9. Each stage's model, tools, budgets, output cap and output shape.
//! 10. The run's id, the spawn-time seeds, then what each region holds at
//!     spawn.
//! 11. The fingerprint of what the run relies on from this machine.
//! 12. The run's creation time.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::spec::env::{Caller, CodeFiles, ResolveEnv};
use crate::spec::graph::RunGraph;
use crate::spec::issues::{SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::launch::Placement;
use crate::spec::names::Digest;
use crate::spec::request::SpawnRequest;
use crate::spec::run_spec::{RunSpec, SpecOrigin};

mod attach;
mod code;
mod defaults;
mod inputs;
mod launch;
mod output;
mod regions;
mod seeds;
mod source;
mod stages;

/// A resolved run and everything its run file stores beside the spec.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The run, decided.
    pub spec: RunSpec,
    /// The code the graph names, by digest.
    pub code: CodeFiles,
    /// The attached files' bytes, by digest.
    pub blobs: BTreeMap<Digest, Vec<u8>>,
}

/// How far resolution goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveMode {
    /// Resolve for a real spawn: every seed runs.
    Spawn,
    /// A dry run. Seeds with side effects (a shell command, code, tool calls)
    /// never run; they are only checked to be allowed and well formed. Seeds
    /// that only read (files, globs, literal text) still run, so what the run
    /// would start with is shown.
    Check,
}

/// Resolve `request` for `caller` against this machine.
///
/// Returns every problem found when there is any. In [`ResolveMode::Spawn`],
/// a seed with side effects runs only when nothing else is wrong, so a spawn
/// that is refused never runs a command on the way to being refused.
pub async fn resolve(
    request: &SpawnRequest,
    caller: &Caller,
    env: &dyn ResolveEnv,
    mode: ResolveMode,
) -> Result<Resolved, SpawnIssues> {
    let mut issues = SpawnIssues::new();
    let limits = env.limits();
    let loaded = source::load(request, env, &mut issues).await;

    let workdir = match env.workdir(request.workdir.as_deref()) {
        Ok(dir) => Some(dir),
        Err(message) => {
            issues.push(workdir_issue(request, message));
            None
        }
    };
    let Some(src) = loaded else {
        return Err(issues);
    };
    let graph_ok = issues.take(src.graph.validate(&src.at)).is_some();

    let registry = attach::registry(&src, env, &mut issues);
    let files = attach::read(request, env, &registry, &limits, &mut issues);
    let checked = inputs::check(
        &src.graph,
        request,
        caller,
        &files,
        workdir.as_deref(),
        env,
        &mut issues,
    );
    let (launch, placement) = launch::decide(request, caller, &src.graph, &limits, &mut issues);
    let auto_answers = launch::auto_answers(&launch, env, &mut issues);
    if !graph_ok {
        return Err(issues);
    }

    let mut graph = src.graph.clone();
    inputs::apply_slots(&mut graph, &checked.values);
    defaults::grant_mode_tools(&mut graph);
    defaults::fold(&mut graph, &limits);
    let mut code = code::read_all(&graph, request, &src, env, &mut issues).await;
    let mut notes = Vec::new();
    for (i, dependency) in graph.dependencies.iter().enumerate() {
        let check = match &dependency.needs {
            crate::spec::graph::Needs::Check(reference) => code.bytes(reference),
            _ => None,
        };
        if let Err(message) = env.dependency(dependency, check).await {
            let at = src.at.field("dependencies").index(i);
            match dependency.required {
                true => issues.push(dependency_issue(at, dependency, message)),
                false => notes.push(format!(
                    "optional dependency '{}' is not met: {message}",
                    dependency.name
                )),
            }
        }
    }
    defaults::check_compaction(&graph, &src.at, env, &mut issues);

    let plans = stages::plan_all(
        &graph,
        request,
        &auto_answers,
        &code.files,
        &src,
        env,
        &mut issues,
    )
    .await;
    let plans = plans.map(|(plans, found)| {
        code.add_found(found);
        plans
    });

    let title = src.origin.blueprint_name().map_or_else(
        || graph.title.clone().unwrap_or_else(|| "run".to_string()),
        str::to_string,
    );
    let run_id = env.new_run_id(&title);
    let agent = src.origin.blueprint_name().map_or_else(
        || graph.title.clone().unwrap_or_else(|| "raw".to_string()),
        str::to_string,
    );
    let placed = regions::place(&graph, &checked, &files, request, &mut issues);
    let seeded = match &workdir {
        Some(dir) => {
            let cx = seeds::SeedRun {
                run_id: &run_id,
                agent: &agent,
                graph: &graph,
                at: &src.at,
                workdir: dir,
                launch: &launch,
                code: &code.files,
                code_refs: &code.refs,
                inputs: &checked.values,
                mode,
            };
            seeds::run_all(cx, env, &mut issues, &mut notes).await
        }
        None => seeds::Seeded::default(),
    };
    let contents = regions::combine(seeded, placed);
    regions::require_filled(
        &graph,
        &contents,
        &checked,
        request,
        caller,
        &src.at,
        &mut issues,
    );

    let Some(mut stages) = plans else {
        return Err(issues);
    };
    let entry = graph
        .entry_stage()
        .and_then(|e| stages.iter().position(|p| p.stage == e.name))
        .unwrap_or(0);
    stages[entry].notes.extend(notes);
    let env_fingerprint = stages::fingerprint(&graph, &stages, env);
    let spec = RunSpec {
        run_id,
        origin: src.origin,
        graph,
        inputs: checked.values,
        stages,
        seeded: contents.regions,
        code: code.refs,
        requested_output: request.output.clone(),
        requested_model: request.model.clone(),
        launch,
        auto_answers,
        placement: Placement {
            workdir: workdir.unwrap_or_default(),
            ..placement
        },
        delivery: request.delivery.clone(),
        env: env_fingerprint,
        created_at: now_secs(),
    };
    issues.into_result(Resolved {
        spec,
        code: code.files,
        blobs: files.blobs(),
    })
}

/// An issue the host reported about part of the request, placed under that
/// part's path.
pub(crate) fn rebase(base: &SpecPath, mut issue: SpawnIssue) -> SpawnIssue {
    issue.path = SpecPath(base.0.iter().cloned().chain(issue.path.0).collect());
    issue
}

fn workdir_issue(request: &SpawnRequest, message: String) -> SpawnIssue {
    let got = request
        .workdir
        .as_ref()
        .map_or_else(|| "no workdir".to_string(), |p| p.display().to_string());
    SpawnIssue::new(
        SpecPath::root().field("workdir"),
        crate::spec::issues::IssueCode::Unresolvable,
        message,
    )
    .got(got)
    .hint("name an existing directory this server may work in")
}

fn dependency_issue(
    at: SpecPath,
    dependency: &crate::spec::graph::DependencyDef,
    message: String,
) -> SpawnIssue {
    let issue = SpawnIssue::new(
        at,
        crate::spec::issues::IssueCode::Unresolvable,
        format!(
            "required dependency '{}' is not met: {message}",
            dependency.name
        ),
    );
    match &dependency.remedy {
        Some(remedy) => issue.hint(remedy.clone()),
        None => issue.hint("run `lev deps` to see what the run needs and how to get it"),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Where the graph came from, and what the resolver needs to know about that.
pub(crate) struct Source {
    /// The graph, as loaded or as sent.
    pub(crate) graph: RunGraph,
    /// Where it came from.
    pub(crate) origin: SpecOrigin,
    /// The blueprint's directory, when it came from one. A graph without one
    /// may not name code by file.
    pub(crate) base: Option<PathBuf>,
    /// Where the graph sits in the request, so its issues read from the root.
    pub(crate) at: SpecPath,
}

#[cfg(test)]
mod tests;
