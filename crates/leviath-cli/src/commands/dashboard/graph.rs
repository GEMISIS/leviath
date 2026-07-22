//! Graph transition info for non-linear (graph-mode) agents.

use leviath_core::{EdgeTransform, TransitionCondition};

/// Cached transition info parsed from the agent's blueprint.
#[derive(Debug, Clone)]
pub(super) struct GraphTransitionInfo {
    /// Map: source_stage → Vec<(target_stage, hint, condition_label, transform_label)>
    pub(super) edges: std::collections::HashMap<String, Vec<GraphEdge>>,
    /// Entry stage name
    pub(super) entry_stage: String,
    /// All stage names in definition order
    pub(super) stage_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct GraphEdge {
    pub(super) target: String,
    pub(super) hint: Option<String>,
    pub(super) condition: String,
    pub(super) transform: String,
}

/// Load graph transition info from an agent manifest directory.
/// Returns `None` for linear agents or if the manifest can't be read/parsed.
pub(super) fn load_graph_info(agent_path: &str) -> Option<GraphTransitionInfo> {
    let manifest_path = std::path::Path::new(agent_path).join("agent.leviath");
    let content = std::fs::read_to_string(&manifest_path).ok()?;
    let blueprint = leviath_core::manifest::parse_manifest(&content).ok()?;

    // Check if any stage has transitions (graph mode)
    let is_graph = blueprint.stages.iter().any(|s| s.transitions.is_some());
    if !is_graph {
        return None;
    }

    let mut edges = std::collections::HashMap::new();
    let stage_names: Vec<String> = blueprint.stages.iter().map(|s| s.name.clone()).collect();

    for stage in &blueprint.stages {
        if let Some(ref transitions) = stage.transitions {
            let stage_edges: Vec<GraphEdge> = transitions
                .iter()
                .map(|(target, edge)| {
                    let condition = match &edge.condition {
                        TransitionCondition::Always => "always".to_string(),
                        TransitionCondition::Error => "error".to_string(),
                        TransitionCondition::MaxIterations => "max_iterations".to_string(),
                        TransitionCondition::LlmChoice => "llm_choice".to_string(),
                    };
                    let transform = match &edge.transform {
                        EdgeTransform::Direct => "direct".to_string(),
                        EdgeTransform::Clear => "clear".to_string(),
                        EdgeTransform::Compact { .. } => "compact".to_string(),
                        EdgeTransform::Custom { .. } => "custom".to_string(),
                    };
                    GraphEdge {
                        target: target.clone(),
                        hint: edge.hint.clone(),
                        condition,
                        transform,
                    }
                })
                .collect();
            edges.insert(stage.name.clone(), stage_edges);
        }
    }

    Some(GraphTransitionInfo {
        edges,
        entry_stage: blueprint.resolve_entry_stage_name(),
        stage_names,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_test_agent;

    #[test]
    fn graph_edge_debug_format() {
        let edge = GraphEdge {
            target: "plan".to_string(),
            hint: Some("on success".to_string()),
            condition: "always".to_string(),
            transform: "direct".to_string(),
        };
        let dbg = format!("{:?}", edge);
        assert!(dbg.contains("plan"));
        assert!(dbg.contains("on success"));
    }

    #[test]
    fn graph_transition_info_empty() {
        let info = GraphTransitionInfo {
            edges: std::collections::HashMap::new(),
            entry_stage: "main".to_string(),
            stage_names: vec!["main".to_string()],
        };
        assert_eq!(info.stage_names.len(), 1);
        assert!(info.edges.is_empty());
    }

    #[test]
    fn graph_transition_info_with_edges() {
        let mut edges = std::collections::HashMap::new();
        edges.insert(
            "plan".to_string(),
            vec![GraphEdge {
                target: "implement".to_string(),
                hint: None,
                condition: "always".to_string(),
                transform: "direct".to_string(),
            }],
        );
        edges.insert(
            "implement".to_string(),
            vec![
                GraphEdge {
                    target: "review".to_string(),
                    hint: Some("done".to_string()),
                    condition: "llm_choice".to_string(),
                    transform: "compact".to_string(),
                },
                GraphEdge {
                    target: "plan".to_string(),
                    hint: Some("needs rework".to_string()),
                    condition: "llm_choice".to_string(),
                    transform: "clear".to_string(),
                },
            ],
        );
        let info = GraphTransitionInfo {
            edges,
            entry_stage: "plan".to_string(),
            stage_names: vec![
                "plan".to_string(),
                "implement".to_string(),
                "review".to_string(),
            ],
        };
        assert_eq!(info.stage_names.len(), 3);
        assert_eq!(info.entry_stage, "plan");
        assert_eq!(info.edges.get("plan").unwrap().len(), 1);
        assert_eq!(info.edges.get("implement").unwrap().len(), 2);
        assert!(!info.edges.contains_key("review"));
    }

    #[test]
    fn load_graph_info_nonexistent_path() {
        let result = load_graph_info("/nonexistent/path/to/agent");
        assert!(result.is_none());
    }

    #[test]
    fn graph_edge_clone() {
        let edge = GraphEdge {
            target: "x".to_string(),
            hint: None,
            condition: "always".to_string(),
            transform: "direct".to_string(),
        };
        let cloned = edge.clone();
        assert_eq!(cloned.target, "x");
    }

    #[test]
    fn graph_transition_info_clone() {
        let info = GraphTransitionInfo {
            edges: std::collections::HashMap::new(),
            entry_stage: "start".to_string(),
            stage_names: vec!["start".to_string(), "end".to_string()],
        };
        let cloned = info.clone();
        assert_eq!(cloned.entry_stage, "start");
        assert_eq!(cloned.stage_names.len(), 2);
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    #[test]
    fn graph_edge_all_fields() {
        let edge = GraphEdge {
            target: "review".to_string(),
            hint: Some("when done".to_string()),
            condition: "llm_choice".to_string(),
            transform: "compact".to_string(),
        };
        assert_eq!(edge.target, "review");
        assert_eq!(edge.hint, Some("when done".to_string()));
        assert_eq!(edge.condition, "llm_choice");
        assert_eq!(edge.transform, "compact");
    }

    #[test]
    fn graph_edge_no_hint() {
        let edge = GraphEdge {
            target: "next".to_string(),
            hint: None,
            condition: "always".to_string(),
            transform: "direct".to_string(),
        };
        assert!(edge.hint.is_none());
    }

    #[test]
    fn graph_transition_info_entry_stage() {
        let info = GraphTransitionInfo {
            edges: std::collections::HashMap::new(),
            entry_stage: "analyze".to_string(),
            stage_names: vec!["analyze".to_string(), "implement".to_string()],
        };
        assert_eq!(info.entry_stage, "analyze");
    }

    #[test]
    fn load_graph_info_empty_dir() {
        let dir = std::env::temp_dir().join("leviath_test_empty_graph");
        let _ = std::fs::create_dir_all(&dir);
        let result = load_graph_info(dir.to_str().unwrap());
        assert!(result.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn graph_transition_info_debug_format() {
        let info = GraphTransitionInfo {
            edges: std::collections::HashMap::new(),
            entry_stage: "start".to_string(),
            stage_names: vec!["start".to_string()],
        };
        let dbg = format!("{:?}", info);
        assert!(dbg.contains("start"));
    }

    #[test]
    fn graph_edge_multiple_edges_from_same_source() {
        let mut edges = std::collections::HashMap::new();
        edges.insert(
            "plan".to_string(),
            vec![
                GraphEdge {
                    target: "implement".to_string(),
                    hint: Some("go ahead".to_string()),
                    condition: "llm_choice".to_string(),
                    transform: "direct".to_string(),
                },
                GraphEdge {
                    target: "abort".to_string(),
                    hint: Some("too complex".to_string()),
                    condition: "llm_choice".to_string(),
                    transform: "clear".to_string(),
                },
            ],
        );

        let info = GraphTransitionInfo {
            edges,
            entry_stage: "plan".to_string(),
            stage_names: vec![
                "plan".to_string(),
                "implement".to_string(),
                "abort".to_string(),
            ],
        };
        let plan_edges = info.edges.get("plan").unwrap();
        assert_eq!(plan_edges.len(), 2);
        assert_eq!(plan_edges[0].target, "implement");
        assert_eq!(plan_edges[1].target, "abort");
    }

    // ─── load_graph_info ────────────────────────────────────────────────────

    fn write_agent_manifest(dir: &std::path::Path, content: &str) {
        write_test_agent(dir, content);
    }

    #[test]
    fn load_graph_info_missing_directory_returns_none() {
        assert!(load_graph_info("/nonexistent/path/to/agent").is_none());
    }

    #[test]
    fn load_graph_info_malformed_manifest_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_manifest(dir.path(), "not valid toml [[[");
        assert!(load_graph_info(dir.path().to_str().unwrap()).is_none());
    }

    #[test]
    fn load_graph_info_linear_agent_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_manifest(
            dir.path(),
            r#"
[agent]
name = "linear-agent"

[stages.main]
mode = "autonomous"
"#,
        );
        assert!(load_graph_info(dir.path().to_str().unwrap()).is_none());
    }

    #[test]
    fn load_graph_info_graph_agent_builds_edges_with_all_condition_and_transform_kinds() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_manifest(
            dir.path(),
            r#"
[agent]
name = "graph-agent"
entry_stage = "plan"

[stages.plan]
mode = "autonomous"

[stages.plan.transitions.implement]
hint = "approved"
condition = "always"
transform = "direct"

[stages.implement]
mode = "autonomous"

[stages.implement.transitions.review]
condition = "llm_choice"
transform = "clear"

[stages.implement.transitions.error_recovery]
condition = "error"
transform = "compact"

[stages.implement.transitions.plan]
condition = "max_iterations"

[stages.review]
mode = "autonomous"

[stages.review.transitions.implement]
condition = "always"
transform = "custom"

[stages.review.transitions.implement.transform_config]
carry = ["task"]

[stages.error_recovery]
mode = "autonomous"

[stages.error_recovery.transitions.implement]
hint = "retry"
"#,
        );

        let info = load_graph_info(dir.path().to_str().unwrap()).expect("graph mode expected");
        assert_eq!(info.entry_stage, "plan");
        assert_eq!(info.stage_names.len(), 4);
        assert!(info.stage_names.contains(&"plan".to_string()));

        let plan_edges = info.edges.get("plan").unwrap();
        assert_eq!(plan_edges.len(), 1);
        assert_eq!(plan_edges[0].target, "implement");
        assert_eq!(plan_edges[0].hint.as_deref(), Some("approved"));
        assert_eq!(plan_edges[0].condition, "always");
        assert_eq!(plan_edges[0].transform, "direct");

        let implement_edges = info.edges.get("implement").unwrap();
        assert_eq!(implement_edges.len(), 3);
        let review_edge = implement_edges
            .iter()
            .find(|e| e.target == "review")
            .unwrap();
        assert_eq!(review_edge.condition, "llm_choice");
        assert_eq!(review_edge.transform, "clear");
        let error_edge = implement_edges
            .iter()
            .find(|e| e.target == "error_recovery")
            .unwrap();
        assert_eq!(error_edge.condition, "error");
        assert_eq!(error_edge.transform, "compact");
        let plan_edge = implement_edges.iter().find(|e| e.target == "plan").unwrap();
        assert_eq!(plan_edge.condition, "max_iterations");
        assert_eq!(plan_edge.transform, "direct"); // default when omitted

        let review_edges = info.edges.get("review").unwrap();
        assert_eq!(review_edges[0].condition, "always");
        assert_eq!(review_edges[0].transform, "custom");

        let error_recovery_edges = info.edges.get("error_recovery").unwrap();
        assert_eq!(error_recovery_edges[0].hint.as_deref(), Some("retry"));
    }

    #[test]
    fn load_graph_info_no_entry_stage_falls_back_to_first_stage() {
        let dir = tempfile::tempdir().unwrap();
        write_agent_manifest(
            dir.path(),
            r#"
[agent]
name = "no-entry-agent"

[stages.first]
mode = "autonomous"

[stages.first.transitions.second]
condition = "always"

[stages.second]
mode = "autonomous"
"#,
        );
        let info = load_graph_info(dir.path().to_str().unwrap()).expect("graph mode expected");
        assert_eq!(info.entry_stage, "first");
    }
}
