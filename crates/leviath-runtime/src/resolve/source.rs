//! Step 1: where the graph comes from.

use super::{Source, rebase};
use crate::spec::env::ResolveEnv;
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
