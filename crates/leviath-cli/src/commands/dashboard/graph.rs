//! Where the dashboard gets a blueprint's [`StageGraph`]: an installed one
//! read from its `agent.toml`, or one shipped inside the binary. A run's own
//! graph comes from its run file (see the run loader).

use std::sync::Arc;

use leviath_runtime::spec::graph::RunGraph;

use crate::tui::flowgraph::StageGraph;

/// The graph of the blueprint at `agent_path`: its directory, or its
/// `agent.toml` itself. `None` when the file cannot be read or is not a
/// blueprint.
pub(super) fn load_blueprint(agent_path: &str) -> Option<RunGraph> {
    leviath_blueprint::load(std::path::Path::new(agent_path))
        .ok()
        .map(|loaded| loaded.graph)
}

/// The stage graph of the blueprint at `agent_path`; see [`load_blueprint`].
pub(super) fn load_stage_graph(agent_path: &str) -> Option<Arc<StageGraph>> {
    load_blueprint(agent_path).map(|graph| Arc::new(StageGraph::from_graph(&graph)))
}

/// The graph of a blueprint shipped inside the binary, by name, so the
/// new-run screen can preview one that `lev setup` has not installed.
pub(super) fn bundled_blueprint(name: &str) -> Option<RunGraph> {
    let agent = crate::bundled::BUNDLED_AGENTS
        .iter()
        .find(|a| a.name == name)?;
    // Every bundled agent has an `agent.toml` and it parses (the bundle tests
    // say so), hence no fallible arms of our own past this point.
    let content = agent
        .files
        .iter()
        .find(|(path, _)| *path == leviath_blueprint::FILE_NAME)
        .map(|(_, content)| *content)
        .unwrap_or_default();
    leviath_blueprint::BlueprintFile::parse(content)
        .ok()
        .map(|file| file.run_graph())
}

/// The stage graph of a bundled blueprint; see [`bundled_blueprint`].
pub(super) fn bundled_stage_graph(name: &str) -> Option<Arc<StageGraph>> {
    bundled_blueprint(name).map(|graph| Arc::new(StageGraph::from_graph(&graph)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{tiny_blueprint, write_test_agent};

    #[test]
    fn a_missing_directory_and_a_malformed_blueprint_yield_none() {
        assert!(load_stage_graph("/nonexistent/path/to/agent").is_none());
        let dir = tempfile::tempdir().unwrap();
        write_test_agent(dir.path(), "this is not toml [[[");
        assert!(load_stage_graph(dir.path().to_str().unwrap()).is_none());
    }

    #[test]
    fn the_blueprint_file_and_its_directory_both_load() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_test_agent(dir.path(), tiny_blueprint("tiny"));
        let via_dir = load_stage_graph(dir.path().to_str().unwrap()).expect("directory form");
        let via_file = load_stage_graph(file.to_str().unwrap()).expect("file form");
        assert_eq!(via_dir, via_file);
        assert!(!via_dir.is_branching, "a one-stage graph is a list");
        assert_eq!(via_dir.entry, "main");
        assert!(via_dir.edges.is_empty());
    }

    #[test]
    fn a_bundled_blueprint_loads_by_name_and_an_unknown_name_does_not() {
        let name = crate::bundled::BUNDLED_AGENTS
            .first()
            .expect("the binary bundles agents")
            .name;
        let graph = bundled_stage_graph(name).expect("bundled parses");
        assert!(graph.nodes.len() > 1);
        assert!(bundled_stage_graph("no-such-blueprint").is_none());
    }
}
