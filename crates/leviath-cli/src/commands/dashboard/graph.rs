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
    #[allow(dead_code)]
    pub(super) transform: String,
}

/// Load graph transition info from an agent manifest directory.
/// Returns `None` for linear agents or if the manifest can't be read/parsed.
pub(super) fn load_graph_info(agent_path: &str) -> Option<GraphTransitionInfo> {
    let manifest_path = std::path::Path::new(agent_path).join("agent.leviath");
    let content = std::fs::read_to_string(&manifest_path).ok()?;
    let blueprint = crate::commands::run::parse_manifest_public(&content).ok()?;

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
                        TransitionCondition::Custom(s) => s.clone(),
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
}
