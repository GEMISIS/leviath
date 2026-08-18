//! Where the dashboard gets a run's or a blueprint's [`StageGraph`].

use std::sync::Arc;

use crate::tui::flowgraph::StageGraph;

/// The stage graph of the blueprint at `agent_path`: a manifest directory, or
/// the manifest file itself (the daemon records `agent_path` as the file,
/// which once made every daemon-spawned graph agent read as linear here).
/// `None` when the manifest cannot be read or parsed.
pub(super) fn load_stage_graph(agent_path: &str) -> Option<Arc<StageGraph>> {
    let path = std::path::Path::new(agent_path);
    let manifest_path = if path.file_name().is_some_and(|f| f == "agent.leviath") {
        path.to_path_buf()
    } else {
        path.join("agent.leviath")
    };
    let content = std::fs::read_to_string(&manifest_path).ok()?;
    let blueprint = leviath_core::manifest::parse_manifest(&content).ok()?;
    Some(Arc::new(StageGraph::from_blueprint(&blueprint)))
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
}
