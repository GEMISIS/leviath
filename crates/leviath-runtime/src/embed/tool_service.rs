//! The batteries-included [`ToolService`] for embedded worlds: the built-in
//! file/shell tools over each agent's workdir, with the `ask_user_*` /
//! `present_for_review` / `edit_document` interaction tools routed through an
//! [`InteractionHub`] so the host application answers them (surfaced as
//! [`Interaction`](crate::host::WorldEvent::Interaction) events).
//!
//! Deliberately smaller than the daemon's tool service: no MCP tools, no Rhai
//! script tools, no sandboxes, no per-tool approval policy - the embedder is
//! code, not an unattended model, and can install its own [`ToolService`] for
//! anything richer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use bevy_ecs::entity::Entity;
use leviath_tools::{BuiltinTools, ToolContext};

use crate::dynamic_interaction::dispatch_dynamic_interaction;
use crate::interaction_hub::{HubInteractionBackend, InteractionHub};
use crate::pipeline::{ToolProgress, ToolService};
use crate::tool_bridge::BoxedToolExec;

/// One registered agent's tool state: its confined built-in tools and its
/// hub-backed interaction channel.
struct AgentTools {
    tools: BuiltinTools,
    backend: HubInteractionBackend,
    /// The agent's current stage name, stamped into interaction requests so
    /// the host knows which stage is asking. Updated by `sync_stage`.
    stage_name: Mutex<String>,
}

/// The default tool service for embedded worlds. See the module docs for what
/// it does and does not provide.
pub struct BasicToolService {
    hub: InteractionHub,
    /// Keyed by raw `Entity`, deliberately.
    ///
    /// Two worlds mint the same entity id, so an entity-keyed map shared between
    /// them would hand one world's tool state to the other's agent. This map is
    /// not shared: a `BasicToolService` is built once per `AgentWorld`, and the
    /// only writer is that world's own spawner. An [`crate::world::AgentId`] key
    /// would carry a world the `ToolService::execute` signature has no way to
    /// supply - it is handed a bare `Entity` by the tool lane, with no world in
    /// reach - so it would buy nothing and cost the trait.
    agents: Mutex<HashMap<Entity, Arc<AgentTools>>>,
}

impl BasicToolService {
    /// A service whose interaction tools ask through `hub`.
    pub fn new(hub: InteractionHub) -> Self {
        Self {
            hub,
            agents: Mutex::new(HashMap::new()),
        }
    }

    /// The tool definitions this service can execute for an agent working in
    /// `workdir` - the set stage resolution filters `available_tools` against.
    pub fn tool_defs(workdir: &Path) -> Vec<leviath_providers::Tool> {
        BuiltinTools::new(ToolContext::new(workdir.to_path_buf())).tool_defs()
    }

    /// Register `entity`'s tool state: built-ins confined to `workdir`, and
    /// interactions attributed to `agent_id`. The embed spawner calls this for
    /// every agent it creates; a host spawning agents directly on the world
    /// does the same.
    pub fn register(&self, entity: Entity, agent_id: &str, workdir: PathBuf) {
        let state = AgentTools {
            tools: BuiltinTools::new(ToolContext::new(workdir)),
            backend: self.hub.backend_for(agent_id),
            stage_name: Mutex::new(String::new()),
        };
        self.agents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(entity, Arc::new(state));
    }

    /// Drop `entity`'s tool state. Called by the reaper when a terminal agent
    /// is unloaded, so the map stays bounded by the set of live agents.
    pub fn unregister(&self, entity: Entity) {
        self.agents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&entity);
    }
}

impl ToolService for BasicToolService {
    fn exec_for(
        &self,
        entity: Entity,
        calls: Vec<leviath_providers::ToolCall>,
        progress: ToolProgress,
    ) -> BoxedToolExec {
        let state = self
            .agents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&entity)
            .cloned();
        Box::new(move || {
            Box::pin(async move {
                let mut results = Vec::with_capacity(calls.len());
                let Some(state) = state else {
                    // Never registered (an agent spawned around the embed
                    // spawner): answer every call rather than dropping the
                    // batch, which would strand the agent.
                    for call in calls {
                        let answer = "[error] no tool state registered for this agent";
                        progress(&call.id, answer);
                        results.push((call.id, answer.to_string()));
                    }
                    return results;
                };
                let stage_name = state
                    .stage_name
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                for call in calls {
                    // Interaction tools block on the hub (the host answers);
                    // everything else runs on the built-ins.
                    let result = match dispatch_dynamic_interaction(
                        &state.backend,
                        &call.name,
                        &call.id,
                        &call.arguments,
                        &stage_name,
                    )
                    .await
                    {
                        Some(result) => result,
                        None => state.tools.execute(&call.name, call.arguments).await,
                    };
                    progress(&call.id, &result);
                    results.push((call.id, result));
                }
                results
            })
        })
    }

    fn sync_stage(&self, entity: Entity, _stage_index: usize, stage_name: &str) {
        if let Some(state) = self
            .agents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&entity)
        {
            *state
                .stage_name
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = stage_name.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::noop_progress;
    use leviath_core::interaction::InteractionResponse;

    fn call(id: &str, name: &str, args: serde_json::Value) -> leviath_providers::ToolCall {
        leviath_providers::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args,
            thought_signature: None,
        }
    }

    fn entity(index: u32) -> Entity {
        Entity::from_raw_u32(index).expect("a small literal index is always a valid entity id")
    }

    #[tokio::test]
    async fn executes_builtin_tools_in_the_registered_workdir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hi there").unwrap();
        let svc = BasicToolService::new(InteractionHub::new());
        let e = entity(1);
        svc.register(e, "agent-a", dir.path().to_path_buf());

        let exec = svc.exec_for(
            e,
            vec![
                call("c1", "read_file", serde_json::json!({"path": "hello.txt"})),
                call(
                    "c2",
                    "write_file",
                    serde_json::json!({"path": "out.txt", "content": "made"}),
                ),
            ],
            noop_progress(),
        );
        let results = exec().await;
        assert_eq!(results[0].0, "c1");
        assert!(results[0].1.contains("hi there"));
        assert_eq!(results[1].0, "c2");
        assert!(!results[1].1.starts_with("[error]"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "made"
        );
    }

    #[tokio::test]
    async fn unknown_tool_reports_an_error_result() {
        let dir = tempfile::tempdir().unwrap();
        let svc = BasicToolService::new(InteractionHub::new());
        let e = entity(1);
        svc.register(e, "agent-a", dir.path().to_path_buf());

        let results = svc.exec_for(
            e,
            vec![call("c1", "no_such_tool", serde_json::Value::Null)],
            noop_progress(),
        )()
        .await;
        assert!(results[0].1.starts_with("[error]"));
    }

    #[tokio::test]
    async fn unregistered_entity_answers_instead_of_stranding_the_batch() {
        let svc = BasicToolService::new(InteractionHub::new());
        let results = svc.exec_for(
            entity(9),
            vec![call("c1", "read_file", serde_json::json!({"path": "x"}))],
            noop_progress(),
        )()
        .await;
        assert_eq!(results.len(), 1);
        assert!(results[0].1.contains("no tool state"));
    }

    #[tokio::test]
    async fn ask_user_text_routes_through_the_hub_and_resumes_on_answer() {
        let dir = tempfile::tempdir().unwrap();
        let hub = InteractionHub::new();
        let svc = BasicToolService::new(hub.clone());
        let e = entity(1);
        svc.register(e, "agent-a", dir.path().to_path_buf());
        svc.sync_stage(e, 0, "plan");

        let exec = svc.exec_for(
            e,
            vec![call(
                "c1",
                "ask_user_text",
                serde_json::json!({"prompt": "Which database?"}),
            )],
            noop_progress(),
        );
        let worker = tokio::spawn(async move { exec().await });

        // The request lands on the hub, attributed to the agent + stage.
        let (agent_id, request) = loop {
            let pending = hub.pending();
            if let Some(p) = pending.into_iter().next() {
                break p;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        assert_eq!(agent_id, "agent-a");
        assert_eq!(request.stage_name, "plan");
        assert!(request.prompt.contains("Which database?"));

        // Answering unblocks the tool call, which carries the answer back.
        assert!(hub.answer(InteractionResponse::text(request.id.clone(), "postgres")));
        let results = worker.await.unwrap();
        assert!(results[0].1.contains("postgres"));
    }

    #[tokio::test]
    async fn sync_stage_for_an_unknown_entity_is_a_no_op() {
        let svc = BasicToolService::new(InteractionHub::new());
        svc.sync_stage(entity(3), 0, "plan"); // nothing to update, no panic
    }

    #[tokio::test]
    async fn unregister_drops_the_agent_state() {
        let dir = tempfile::tempdir().unwrap();
        let svc = BasicToolService::new(InteractionHub::new());
        let e = entity(1);
        svc.register(e, "agent-a", dir.path().to_path_buf());
        svc.unregister(e);
        let results = svc.exec_for(
            e,
            vec![call("c1", "read_file", serde_json::json!({"path": "x"}))],
            noop_progress(),
        )()
        .await;
        assert!(results[0].1.contains("no tool state"));
    }

    #[test]
    fn tool_defs_cover_the_builtin_set() {
        let dir = tempfile::tempdir().unwrap();
        let defs = BasicToolService::tool_defs(dir.path());
        let names: Vec<&str> = defs.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"shell"));
        assert!(names.contains(&"ask_user_text"));
    }
}
