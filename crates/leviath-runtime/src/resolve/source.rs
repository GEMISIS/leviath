//! Step 1: where the graph comes from.

use super::fan_out::WorkerInputs;
use super::{Source, rebase};
use crate::spec::env::ResolveEnv;
use crate::spec::graph::{RunGraph, StageMode, WorkerSource};
use crate::spec::issues::{SpawnIssues, SpecPath};
use crate::spec::request::{SpawnRequest, SpawnSource};
use crate::spec::run_spec::SpecOrigin;

/// The request's graph: the named blueprint's or the one in the named
/// directory, loaded, or the caller's own.
/// `None` when the blueprint cannot be loaded, which is the one problem no
/// later step can work around.
pub(super) async fn load(
    request: &SpawnRequest,
    env: &dyn ResolveEnv,
    issues: &mut SpawnIssues,
) -> Option<Source> {
    let at = SpecPath::root().field("source");
    match &request.source {
        SpawnSource::Blueprint(reference) => {
            let at = at.field("blueprint");
            match env.blueprint(reference).await {
                Ok(loaded) => Some(Source {
                    graph: loaded.graph,
                    origin: SpecOrigin::Blueprint {
                        blueprint: loaded.reference,
                        version: loaded.version,
                        manifest: loaded
                            .base_dir
                            .join(leviath_core::files::BLUEPRINT_MANIFEST)
                            .to_string_lossy()
                            .into_owned(),
                    },
                    base: Some(loaded.base_dir),
                    at,
                }),
                Err(issue) => {
                    issues.push(rebase(&at, issue));
                    None
                }
            }
        }
        SpawnSource::BlueprintFile(path) => {
            let at = at.field("blueprint_file");
            match env.blueprint_file(path).await {
                Ok(loaded) => Some(Source {
                    graph: loaded.graph,
                    origin: SpecOrigin::BlueprintFile {
                        path: path.clone(),
                        name: loaded.reference.name,
                        digest: loaded.reference.digest,
                        version: loaded.version,
                    },
                    base: Some(loaded.base_dir),
                    at,
                }),
                Err(issue) => {
                    issues.push(rebase(&at, issue));
                    None
                }
            }
        }
        SpawnSource::Raw(graph) => Some(Source {
            graph: (**graph).clone(),
            origin: SpecOrigin::Raw,
            base: None,
            at: at.field("raw"),
        }),
    }
}

/// Pin each fan-out stage's installed worker blueprints to the revision
/// installed now, so every worker of the run (and of the run resumed) runs
/// the blueprint the run was resolved against, and say what inputs each
/// declared worker takes, for the stage's `fan_out` tool to list.
///
/// A worker blueprint that cannot be loaded is reported at `at`'s
/// `stages.<stage>.mode.fan_out.workers_available[i]`, with everything else
/// wrong with the request: found here, it costs nothing, where found when the
/// first worker starts it costs every stage the run paid for on the way there.
pub(super) async fn pin_workers(
    graph: &mut RunGraph,
    at: &SpecPath,
    env: &dyn ResolveEnv,
    issues: &mut SpawnIssues,
) -> WorkerInputs {
    let own: Vec<String> = graph.inputs.iter().map(|d| d.name.to_string()).collect();
    let mut inputs = WorkerInputs::new();
    for stage in &mut graph.stages {
        let StageMode::FanOut(fan) = &mut stage.mode else {
            continue;
        };
        for (i, worker) in fan.workers_available.iter_mut().enumerate() {
            let key = (stage.name.clone(), worker.name.clone());
            let reference = match &mut worker.runs {
                WorkerSource::Stage(_) => {
                    inputs.insert(key, own.clone());
                    continue;
                }
                WorkerSource::Blueprint(reference) => reference,
                WorkerSource::BlueprintFile(_) | WorkerSource::Query(_) => continue,
            };
            match env.blueprint(reference).await {
                Ok(loaded) => {
                    reference.digest = reference.digest.take().or(loaded.reference.digest);
                    let names = loaded.graph.inputs.iter().map(|d| d.name.to_string());
                    inputs.insert(key, names.collect());
                }
                Err(issue) => {
                    let worker = at
                        .field("stages")
                        .key(stage.name.as_str())
                        .field("mode")
                        .field("fan_out")
                        .field("workers_available")
                        .index(i);
                    issues.push(rebase(&worker, issue));
                }
            }
        }
    }
    inputs
}
