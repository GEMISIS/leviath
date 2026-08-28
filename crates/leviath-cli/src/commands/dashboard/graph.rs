//! Where the dashboard gets a run's or a blueprint's [`StageGraph`].

use std::sync::Arc;

use crate::tui::flowgraph::StageGraph;

/// The stage graph of the blueprint at `agent_path`: a manifest directory, or
/// the manifest file itself (the daemon records `agent_path` as the file,
/// which once made every daemon-spawned graph agent read as linear here).
/// `None` when the manifest cannot be read or parsed.
pub(super) fn load_stage_graph(agent_path: &str) -> Option<Arc<StageGraph>> {
    let path = std::path::Path::new(agent_path);
    let manifest_path = if path
        .file_name()
        .is_some_and(|f| f == leviath_core::files::MANIFEST_FILENAME)
    {
        path.to_path_buf()
    } else {
        path.join(leviath_core::files::MANIFEST_FILENAME)
    };
    let content = std::fs::read_to_string(&manifest_path).ok()?;
    let blueprint = leviath_core::manifest::parse_manifest(&content).ok()?;
    Some(Arc::new(StageGraph::from_blueprint(&blueprint)))
}

/// The stage graph of a blueprint shipped inside the binary, by name, so
/// the new-run screen can preview one that `lev setup` has not installed.
pub(super) fn bundled_stage_graph(name: &str) -> Option<Arc<StageGraph>> {
    let agent = crate::bundled::BUNDLED_AGENTS
        .iter()
        .find(|a| a.name == name)?;
    // Every bundled agent has a manifest and it parses (the bundle tests
    // say so), hence no fallible arms of our own past this point.
    let content = agent
        .files
        .iter()
        .find(|(path, _)| *path == leviath_core::files::MANIFEST_FILENAME)
        .map(|(_, content)| *content)
        .unwrap_or_default();
    leviath_core::manifest::parse_manifest(content)
        .ok()
        .map(|blueprint| Arc::new(StageGraph::from_blueprint(&blueprint)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_test_agent;

    #[test]
    fn a_missing_directory_and_a_malformed_manifest_yield_none() {
        assert!(load_stage_graph("/nonexistent/path/to/agent").is_none());
        let dir = tempfile::tempdir().unwrap();
        write_test_agent(dir.path(), "this is not toml [[[");
        assert!(load_stage_graph(dir.path().to_str().unwrap()).is_none());
    }

    #[test]
    fn the_manifest_file_path_and_its_directory_both_load_and_linear_agents_count() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_test_agent(
            dir.path(),
            r#"
[agent]
name = "linear"
[stages.first]
[stages.second]
"#,
        );
        let via_dir = load_stage_graph(dir.path().to_str().unwrap()).expect("directory form");
        let via_file = load_stage_graph(manifest.to_str().unwrap()).expect("file form");
        assert_eq!(via_dir, via_file);
        assert!(!via_dir.is_branching, "a linear agent is a graph too");
        assert_eq!(via_dir.entry, "first");
        assert_eq!(via_dir.edges.len(), 1);
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
