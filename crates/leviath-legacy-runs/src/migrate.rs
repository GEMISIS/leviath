//! Converting an `agent.leviath` manifest into an `agent.toml` blueprint.
//!
//! The manifest is read by [`parse_manifest`], then read as a run graph, so
//! the new file describes exactly the run the old one did. A key the parser
//! does not read is reported rather than left behind. What
//! `lev blueprint migrate` writes is [`migrate`]'s output.

use leviath_blueprint::{BlueprintFile, BlueprintMeta};
use leviath_runtime::spec::graph::RunGraph;
use leviath_runtime::spec::names::BlueprintName;

use crate::manifest::{parse_manifest, read_manifest_tables, unread_keys};

/// Convert the text of an `agent.leviath` into the text of an `agent.toml`.
/// On failure, every problem found, one per line.
pub fn migrate(manifest: &str) -> Result<String, Vec<String>> {
    // The one thing TOML cannot hold is a JSON `null`, and every JSON
    // document in a manifest's graph (an output schema, a seed tool's
    // arguments) was itself read from TOML.
    Ok(migrate_file(manifest)?
        .to_toml()
        .expect("a graph read from TOML writes as TOML"))
}

/// Convert the text of an `agent.leviath` into a blueprint file.
pub fn migrate_file(manifest: &str) -> Result<BlueprintFile, Vec<String>> {
    let blueprint = parse_manifest(manifest).map_err(|e| vec![e.to_string()])?;
    let graph = RunGraph::from_blueprint(&blueprint)
        .and_then(|mut graph| read_manifest_tables(&mut graph, manifest).map(|()| graph))
        .map_err(|issues| issues.iter().map(ToString::to_string).collect::<Vec<_>>());
    let name =
        BlueprintName::new(blueprint.name.as_str()).map_err(|e| format!("[agent] name: {e}"));
    // A key the parser skipped would be missing from the new file with
    // nothing to say so, which is a problem like any other.
    let unread = unread_keys(&toml::from_str(manifest).expect("a manifest that parsed is TOML"));
    let (mut graph, name) = match (graph, name) {
        (Ok(graph), Ok(name)) if unread.is_empty() => (graph, name),
        (graph, name) => {
            let mut problems = graph.err().unwrap_or_default();
            problems.extend(name.err());
            problems.extend(unread);
            return Err(problems);
        }
    };
    // `[blueprint]` carries the name and description, and a graph read from
    // the file takes them from there, so the graph does not repeat them.
    let description = graph.description.take();
    graph.title = graph.title.filter(|t| t != name.as_str());
    Ok(BlueprintFile {
        blueprint: BlueprintMeta {
            name,
            version: blueprint.version,
            description,
        },
        graph,
    })
}

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod tests;
