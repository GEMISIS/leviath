//! [`AgentWorld`]: the embedder-facing runtime. Build one with plain values,
//! spawn agents, watch the event stream, answer their questions, shut down.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{broadcast, oneshot};

use super::spawner::{EmbedSpawner, StagedBlueprints, mint_run_id};
use super::{BasicToolService, EmbedError, EventStream};
use crate::components::AgentStatus;
use crate::host::{ControlOp, SpawnArgs, WorldEvent, WorldHost};
use crate::inference_pool::InferencePoolConfig;
use crate::interaction_hub::InteractionHub;
use crate::pipeline::{ModelDefaults, ToolService};
use crate::provider_creds::{ProviderCreds, build_provider_registry};
use crate::providers::ProviderRegistry;
use crate::world::PipelineWorld;

/// An opaque run identifier, minted by [`AgentWorld::spawn`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunId(String);

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RunId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Where a spawn's blueprint comes from.
pub enum BlueprintSource {
    /// A `.leviath` manifest file on disk.
    Path(PathBuf),
    /// Manifest TOML held in memory (parsed and validated at spawn).
    Toml(String),
    /// An already-constructed blueprint value (boxed: a Blueprint is a
    /// large value, and boxing keeps the enum small).
    Inline(Box<leviath_core::Blueprint>),
}

/// One spawn request. Build with [`SpawnSpec::new`], then set the optional
/// fields directly; the struct is non-exhaustive so new options stay
/// additive.
#[non_exhaustive]
pub struct SpawnSpec {
    /// The agent's blueprint.
    pub blueprint: BlueprintSource,
    /// The task prompt, seeded into the blueprint's `task` region.
    pub task: String,
    /// Working directory tools are confined to. Must exist.
    pub workdir: PathBuf,
    /// Optional model override (`provider/model` or a bare model name).
    pub model: Option<String>,
    /// Seed content for named caller-input regions.
    pub regions: HashMap<String, String>,
    /// Custom key/value metadata carried in the run's metadata.
    pub metadata: HashMap<String, String>,
}

impl SpawnSpec {
    /// A spec with the required fields; optional fields start empty.
    pub fn new(
        blueprint: BlueprintSource,
        task: impl Into<String>,
        workdir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            blueprint,
            task: task.into(),
            workdir: workdir.into(),
            model: None,
            regions: HashMap::new(),
            metadata: HashMap::new(),
        }
    }
}

/// Builds an [`AgentWorld`] from plain values - no config file, no daemon.
///
/// ```ignore
/// let world = AgentWorld::builder()
///     .provider(ProviderCreds::anthropic(api_key))
///     .build()?;
/// ```
pub struct AgentWorldBuilder {
    creds: Vec<ProviderCreds>,
    custom_providers: Vec<(String, Arc<dyn leviath_providers::Provider>)>,
    tool_service: Option<Arc<dyn ToolService>>,
    pool_config: InferencePoolConfig,
    tool_concurrency: usize,
    state_dir: Option<PathBuf>,
    defaults: ModelDefaults,
    runtime: Option<Handle>,
}

impl AgentWorldBuilder {
    fn new() -> Self {
        Self {
            creds: Vec::new(),
            custom_providers: Vec::new(),
            tool_service: None,
            pool_config: InferencePoolConfig::new(),
            tool_concurrency: 4,
            state_dir: None,
            defaults: ModelDefaults::default(),
            runtime: None,
        }
    }

    /// Add a provider from credentials (repeatable). See [`ProviderCreds`]
    /// for the supported providers.
    pub fn provider(mut self, creds: ProviderCreds) -> Self {
        self.creds.push(creds);
        self
    }

    /// Register a custom [`Provider`](leviath_providers::Provider)
    /// implementation under `name` (repeatable). Wins over a credentials
    /// entry with the same name.
    pub fn register_provider(
        mut self,
        name: impl Into<String>,
        provider: Arc<dyn leviath_providers::Provider>,
    ) -> Self {
        self.custom_providers.push((name.into(), provider));
        self
    }

    /// The user-default provider/model, the fallback when none of a stage's
    /// listed models has a registered provider.
    pub fn default_model(mut self, provider: impl Into<String>, model: impl Into<String>) -> Self {
        self.defaults = ModelDefaults {
            provider: provider.into(),
            model: Some(model.into()),
        };
        self
    }

    /// Replace the default [`BasicToolService`] with a custom tool service.
    /// The embed spawner then skips per-agent tool registration; the custom
    /// service sees agents through its own `exec_for`.
    pub fn tool_service(mut self, service: Arc<dyn ToolService>) -> Self {
        self.tool_service = Some(service);
        self
    }

    /// Persist run state on disk under `dir`, in the daemon's layout
    /// (`<dir>/runs/<run_id>/`, machine id at `<dir>/machine-id`). Without
    /// this the world runs entirely in memory and never touches disk.
    pub fn state_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.state_dir = Some(dir.into());
        self
    }

    /// Per-model inference concurrency limits.
    pub fn inference_pool(mut self, config: InferencePoolConfig) -> Self {
        self.pool_config = config;
        self
    }

    /// How many tool batches may execute concurrently (default 4).
    pub fn tool_concurrency(mut self, n: usize) -> Self {
        self.tool_concurrency = n;
        self
    }

    /// Run the world on `handle` instead of the ambient Tokio runtime.
    pub fn runtime(mut self, handle: Handle) -> Self {
        self.runtime = Some(handle);
        self
    }

    /// Assemble the world and start its serve loop on the Tokio runtime.
    pub fn build(self) -> Result<AgentWorld, EmbedError> {
        if self.creds.is_empty() && self.custom_providers.is_empty() {
            return Err(EmbedError::NoProviders);
        }
        let handle = match self.runtime {
            Some(handle) => handle,
            None => Handle::try_current().map_err(|_| EmbedError::NoRuntime)?,
        };

        let mut registry: ProviderRegistry = build_provider_registry(&self.creds);
        for (name, provider) in self.custom_providers {
            registry.register(name, provider);
        }

        let hub = InteractionHub::new();
        let (service, basic_tools): (Arc<dyn ToolService>, Option<Arc<BasicToolService>>) =
            match self.tool_service {
                Some(service) => (service, None),
                None => {
                    let basic = Arc::new(BasicToolService::new(hub.clone()));
                    (basic.clone(), Some(basic))
                }
            };

        let mut world = PipelineWorld::new(
            registry,
            service,
            self.pool_config,
            self.tool_concurrency,
            self.state_dir.map(|d| d.join("runs")),
            handle.clone(),
        );
        world.insert_interaction_hub(hub.clone());
        let mut host = WorldHost::with_interactions(world, hub.clone());

        let staged: StagedBlueprints = Arc::new(Mutex::new(HashMap::new()));
        let spawner = EmbedSpawner {
            basic_tools: basic_tools.clone(),
            defaults: self.defaults,
            staged: staged.clone(),
        };
        host.set_spawner(Box::new(move |world, args| spawner.spawn(world, args)));
        if let Some(tools) = basic_tools {
            host.set_reaper(Box::new(move |_world, entity| tools.unregister(entity)));
        }

        let events = host.event_sender();
        let (control, control_rx) = tokio::sync::mpsc::unbounded_channel();
        let serve_task = handle.spawn(async move {
            host.serve(control_rx).await;
            host
        });

        Ok(AgentWorld {
            control,
            events,
            hub,
            staged,
            serve_task,
        })
    }
}

/// A running embedded world: agents spawn into it, events stream out of it.
///
/// Internally this is the same [`WorldHost`] the daemon serves - addressed
/// in-process over a channel instead of over the control socket.
pub struct AgentWorld {
    control: UnboundedSender<ControlOp>,
    events: broadcast::Sender<WorldEvent>,
    hub: InteractionHub,
    staged: StagedBlueprints,
    serve_task: tokio::task::JoinHandle<WorldHost>,
}

impl AgentWorld {
    /// Start building a world.
    pub fn builder() -> AgentWorldBuilder {
        AgentWorldBuilder::new()
    }

    /// Send one control op and await its reply.
    async fn ask<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<T>) -> ControlOp,
    ) -> Result<T, EmbedError> {
        let (reply, rx) = oneshot::channel();
        self.control
            .send(build(reply))
            .map_err(|_| EmbedError::ChannelClosed)?;
        rx.await.map_err(|_| EmbedError::ChannelClosed)
    }

    /// Spawn an agent. Returns its [`RunId`] once the agent is live in the
    /// world (blueprint loaded, stages resolved, seeds applied).
    pub async fn spawn(&self, spec: SpawnSpec) -> Result<RunId, EmbedError> {
        // Resolve the blueprint source: a path passes through to the spawner;
        // in-memory blueprints validate here and park in the staged map under
        // the freshly minted run id.
        enum Resolved {
            Path(PathBuf),
            Inline(Box<leviath_core::Blueprint>),
        }
        let resolved = match spec.blueprint {
            BlueprintSource::Path(path) => Resolved::Path(path),
            BlueprintSource::Toml(toml) => Resolved::Inline(Box::new(
                leviath_core::manifest::parse_manifest(&toml)
                    .map_err(|e| EmbedError::Blueprint(format!("parse manifest: {e}")))?,
            )),
            BlueprintSource::Inline(blueprint) => Resolved::Inline(blueprint),
        };
        if let Resolved::Inline(blueprint) = &resolved {
            blueprint
                .validate()
                .map_err(|e| EmbedError::Blueprint(format!("invalid blueprint: {e}")))?;
        }
        let stem = match &resolved {
            Resolved::Path(path) => path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Resolved::Inline(blueprint) => blueprint.name.clone(),
        };
        let run_id = mint_run_id(&stem);
        let blueprint_path = match resolved {
            Resolved::Path(path) => path.to_string_lossy().into_owned(),
            Resolved::Inline(blueprint) => {
                self.staged
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(run_id.clone(), *blueprint);
                format!("inline:{run_id}")
            }
        };
        let args = SpawnArgs {
            run_id,
            blueprint_path,
            task: spec.task,
            regions: spec.regions,
            model: spec.model,
            workdir: spec.workdir.to_string_lossy().into_owned(),
            metadata: spec.metadata,
            ..Default::default()
        };
        let run_id = self
            .ask(|reply| ControlOp::Spawn {
                args: Box::new(args),
                reply,
            })
            .await?
            .map_err(EmbedError::Spawn)?;
        Ok(RunId(run_id))
    }

    /// Subscribe to the world's events, from this moment on.
    pub fn events(&self) -> EventStream {
        EventStream::new(self.events.subscribe())
    }

    /// A run's current status, or `None` if the world doesn't know it.
    pub async fn status(&self, id: &RunId) -> Option<AgentStatus> {
        self.ask(|reply| ControlOp::Status {
            run_id: id.0.clone(),
            reply,
        })
        .await
        .ok()
        .flatten()
    }

    /// Deliver a message into a running agent's inbox. `false` when the
    /// world can no longer accept messages (shut down or shutting down).
    pub async fn send_message(&self, id: &RunId, content: &str) -> bool {
        self.ask(|reply| ControlOp::Message {
            agent_id: id.0.clone(),
            content: content.to_string(),
            target_region: None,
            reply,
        })
        .await
        .unwrap_or(false)
    }

    /// Pause a run. `false` if there is no such live run.
    pub async fn pause(&self, id: &RunId) -> bool {
        self.ask(|reply| ControlOp::Pause {
            run_id: id.0.clone(),
            reply,
        })
        .await
        .unwrap_or(false)
    }

    /// Resume a paused run. `false` if there is no such live run.
    pub async fn resume(&self, id: &RunId) -> bool {
        self.ask(|reply| ControlOp::Resume {
            run_id: id.0.clone(),
            reply,
        })
        .await
        .unwrap_or(false)
    }

    /// Cancel a run. `false` if there is no such live run.
    pub async fn cancel(&self, id: &RunId) -> bool {
        self.ask(|reply| ControlOp::Cancel {
            run_id: id.0.clone(),
            reply,
        })
        .await
        .unwrap_or(false)
    }

    /// Every open question agents are waiting on, as `(run, request)`. Each
    /// also arrived as an [`Interaction`](WorldEvent::Interaction) event.
    pub fn pending_inputs(&self) -> Vec<(RunId, leviath_core::interaction::InteractionRequest)> {
        self.hub
            .pending()
            .into_iter()
            .map(|(agent_id, request)| (RunId(agent_id), request))
            .collect()
    }

    /// Answer an open question (matched by the response's `request_id`).
    /// `false` if no such request is open.
    pub fn answer(&self, response: leviath_core::interaction::InteractionResponse) -> bool {
        self.hub.answer(response)
    }

    /// Shut the world down and wait for it to finish. The serve loop drains
    /// every queued persistence write before it returns (its own
    /// flush-and-stop), so once this resolves nothing is left in flight.
    pub async fn shutdown(self) {
        let _ = self.ask(|reply| ControlOp::Shutdown { reply }).await;
        // Joining is enough: `WorldHost::serve` flushes on its way out, and a
        // second flush would tick a world whose persistence resource is
        // already gone. `Err` here means the task was aborted; there is
        // nothing left to wait for either way.
        drop(self.serve_task.await);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_providers::{
        FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, Provider,
        ProviderError, TokenUsage, ToolCall,
    };
    use std::collections::VecDeque;

    /// A scripted provider: pops one canned response per inference call.
    struct Mock {
        responses: Mutex<VecDeque<InferenceResponse>>,
    }

    #[async_trait::async_trait]
    impl Provider for Mock {
        async fn infer(&self, _r: InferenceRequest) -> Result<InferenceResponse, ProviderError> {
            self.responses
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()
                .ok_or_else(|| ProviderError::Other("script exhausted".to_string()))
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "mock"
        }
        fn capabilities(&self, _m: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    fn text(content: &str) -> InferenceResponse {
        InferenceResponse {
            content: content.to_string(),
            tool_calls: vec![],
            tokens_used: TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            finish_reason: FinishReason::Complete,
        }
    }

    fn with_tool(id: &str, name: &str, args: serde_json::Value) -> InferenceResponse {
        let mut r = text("");
        r.tool_calls.push(ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args,
            thought_signature: None,
        });
        r
    }

    fn mock_world(responses: Vec<InferenceResponse>) -> AgentWorld {
        AgentWorld::builder()
            .register_provider(
                "mock",
                Arc::new(Mock {
                    responses: Mutex::new(responses.into_iter().collect()),
                }),
            )
            .build()
            .expect("world builds inside the test runtime")
    }

    const TWO_STAGE: &str = r#"[agent]
name = "embedded"
version = "0.0.0"
description = "Two stage embedded test agent."
entry_stage = "work"

[stages.work]
mode = "autonomous"
model = { provider = "mock", model = "m" }
description = "Do the work"
available_tools = ["read_file"]
system_prompt = "Work."
[stages.work.transitions.wrap]
transform = "direct"

[stages.wrap]
mode = "autonomous"
model = { provider = "mock", model = "m" }
description = "Wrap up"
allow_complete = true
system_prompt = "Wrap."

[context.regions]
conversation = { kind = "sliding_window", max_items = 40, max_tokens = 20000 }
"#;

    const ASKER: &str = r#"[agent]
name = "asker"
version = "0.0.0"
description = "Asks one question then finishes."
entry_stage = "chat"

[stages.chat]
mode = "autonomous"
model = { provider = "mock", model = "m" }
description = "Chat"
available_tools = ["ask_user_text"]
allow_complete = true
system_prompt = "Ask."

[context.regions]
conversation = { kind = "sliding_window", max_items = 40, max_tokens = 20000 }
"#;

    /// Drain events until `pred` matches (or the stream ends), collecting
    /// everything seen. Bounded by the caller's `tokio::time::timeout`.
    async fn events_until(
        stream: &mut EventStream,
        pred: impl Fn(&WorldEvent) -> bool,
    ) -> Vec<WorldEvent> {
        let mut seen = Vec::new();
        while let Some(event) = stream.next().await {
            let done = pred(&event);
            seen.push(event);
            if done {
                break;
            }
        }
        seen
    }

    #[tokio::test]
    async fn build_without_providers_is_refused() {
        let err = AgentWorld::builder().build().map(|_| ()).unwrap_err();
        assert_eq!(err, EmbedError::NoProviders);
    }

    #[test]
    fn build_outside_a_tokio_runtime_is_refused() {
        let err = AgentWorld::builder()
            .register_provider(
                "mock",
                Arc::new(Mock {
                    responses: Mutex::new(VecDeque::new()),
                }),
            )
            .build()
            .map(|_| ())
            .unwrap_err();
        assert_eq!(err, EmbedError::NoRuntime);
    }

    #[test]
    fn build_accepts_an_explicit_runtime_handle() {
        // A plain test (no ambient runtime): the handle passed via
        // `.runtime()` is what makes build succeed.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let world = AgentWorld::builder()
            .register_provider(
                "mock",
                Arc::new(Mock {
                    responses: Mutex::new(VecDeque::new()),
                }),
            )
            .runtime(rt.handle().clone())
            .build()
            .expect("explicit handle suffices");
        rt.block_on(world.shutdown());
    }

    #[tokio::test]
    async fn agent_runs_to_completion_with_stage_and_tool_events() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "the notes").unwrap();
        // The wrap stage makes no tool calls, so its text-only responses get
        // the "use your tools" nudge up to the cap before the last is
        // accepted; script enough of them.
        let world = mock_world(vec![
            with_tool("c1", "read_file", serde_json::json!({"path": "notes.txt"})),
            text("moving on"),
            text("done"),
            text("done"),
            text("done"),
            text("done"),
        ]);
        let mut events = world.events();

        let run_id = world
            .spawn(SpawnSpec::new(
                BlueprintSource::Toml(TWO_STAGE.to_string()),
                "summarize the notes",
                dir.path(),
            ))
            .await
            .expect("spawns");
        assert!(run_id.as_ref().starts_with("embedded-"));

        let seen = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            events_until(&mut events, |e| matches!(e, WorldEvent::Completed { .. })),
        )
        .await
        .expect("completed before timeout");

        let spawned = seen
            .iter()
            .any(|e| matches!(e, WorldEvent::Spawned { run_id: r, .. } if r == run_id.as_ref()));
        assert!(spawned, "saw Spawned: {seen:?}");
        let transitioned = seen.iter().any(|e| {
            matches!(e, WorldEvent::StageTransition { from, to, .. }
                if from == "work" && to == "wrap")
        });
        assert!(transitioned, "saw StageTransition: {seen:?}");
        let started = seen
            .iter()
            .any(|e| matches!(e, WorldEvent::ToolCallStarted { tool, .. } if tool == "read_file"));
        assert!(started, "saw ToolCallStarted: {seen:?}");
        let finished = seen.iter().any(|e| {
            matches!(e, WorldEvent::ToolCallFinished { tool, ok, summary, .. }
                if tool == "read_file" && *ok && summary.contains("the notes"))
        });
        assert!(finished, "saw ToolCallFinished: {seen:?}");
        let completed = seen
            .iter()
            .any(|e| matches!(e, WorldEvent::Completed { status, .. } if status == "complete"));
        assert!(completed, "saw Completed: {seen:?}");

        world.shutdown().await;
    }

    #[tokio::test]
    async fn ask_user_surfaces_as_interaction_and_resumes_on_answer() {
        let dir = tempfile::tempdir().unwrap();
        let world = mock_world(vec![
            with_tool(
                "c1",
                "ask_user_text",
                serde_json::json!({"prompt": "Which database?"}),
            ),
            text("done"),
        ]);
        let mut events = world.events();
        let run_id = world
            .spawn(SpawnSpec::new(
                BlueprintSource::Toml(ASKER.to_string()),
                "pick a database",
                dir.path(),
            ))
            .await
            .expect("spawns");

        // The question arrives on the event stream and in pending_inputs.
        let seen = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            events_until(&mut events, |e| matches!(e, WorldEvent::Interaction { .. })),
        )
        .await
        .expect("interaction before timeout");
        let request = seen
            .iter()
            .find_map(|e| match e {
                WorldEvent::Interaction {
                    run_id: r, request, ..
                } if r == run_id.as_ref() => Some(request.clone()),
                _ => None,
            })
            .expect("interaction event carries the request");
        assert!(request.prompt.contains("Which database?"));
        let pending = world.pending_inputs();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, run_id);

        // A live, parked agent still accepts messages. (Pause is refused
        // while the agent waits on input - see the capacity test below for
        // the pause/resume round-trip.)
        assert!(!world.pause(&run_id).await);
        assert!(world.send_message(&run_id, "prefer something boring").await);

        // Answering resumes the run to completion.
        assert!(
            world.answer(leviath_core::interaction::InteractionResponse::text(
                request.id.clone(),
                "postgres"
            ))
        );
        let seen = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            events_until(&mut events, |e| matches!(e, WorldEvent::Completed { .. })),
        )
        .await
        .expect("completed before timeout");
        assert!(
            seen.iter()
                .any(|e| matches!(e, WorldEvent::Completed { .. }))
        );

        world.shutdown().await;
    }

    #[tokio::test]
    async fn spawn_reports_blueprint_and_workdir_errors() {
        let dir = tempfile::tempdir().unwrap();
        let world = mock_world(vec![]);

        // Unparseable TOML.
        let err = world
            .spawn(SpawnSpec::new(
                BlueprintSource::Toml("not = [valid".to_string()),
                "t",
                dir.path(),
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("blueprint error"));

        // A manifest path that does not exist fails in the spawner.
        let err = world
            .spawn(SpawnSpec::new(
                BlueprintSource::Path(dir.path().join("missing.leviath")),
                "t",
                dir.path(),
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("spawn error"));

        // A workdir that does not exist is refused before anything spawns.
        let err = world
            .spawn(SpawnSpec::new(
                BlueprintSource::Toml(TWO_STAGE.to_string()),
                "t",
                dir.path().join("nope"),
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("spawn error"));

        world.shutdown().await;
    }

    #[tokio::test]
    async fn spawn_from_a_manifest_file_works() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("embedded.leviath");
        std::fs::write(&manifest, TWO_STAGE).unwrap();
        // Both stages are text-only here, so each needs its nudge budget.
        let world = mock_world(vec![
            text("moving on"),
            text("moving on"),
            text("moving on"),
            text("moving on"),
            text("done"),
            text("done"),
            text("done"),
            text("done"),
        ]);
        let mut events = world.events();

        let run_id = world
            .spawn(SpawnSpec::new(
                BlueprintSource::Path(manifest),
                "just finish",
                dir.path(),
            ))
            .await
            .expect("spawns from the file");
        assert!(run_id.as_ref().starts_with("embedded-"));

        let seen = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            events_until(&mut events, |e| matches!(e, WorldEvent::Completed { .. })),
        )
        .await
        .expect("completed before timeout");
        assert!(
            seen.iter()
                .any(|e| matches!(e, WorldEvent::Completed { .. }))
        );
        world.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_runs_answer_negatively() {
        let world = mock_world(vec![]);
        let ghost = RunId("no-such-run".to_string());
        assert_eq!(world.status(&ghost).await, None);
        assert!(!world.pause(&ghost).await);
        assert!(!world.resume(&ghost).await);
        assert!(!world.cancel(&ghost).await);
        assert!(world.pending_inputs().is_empty());
        world.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_ends_the_event_stream_and_further_requests_fail() {
        let world = mock_world(vec![]);
        let mut events = world.events();
        let control = world.control.clone();
        world.shutdown().await;
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), events.next())
                .await
                .expect("stream ends"),
            None
        );
        // The serve loop is gone (shutdown consumed the world after joining
        // it), so its receiver is dropped and a late op cannot be delivered.
        assert!(control.is_closed());
        let (reply, _rx) = oneshot::channel();
        assert!(control.send(ControlOp::List { reply }).is_err());
    }

    #[tokio::test]
    async fn cancel_stops_a_parked_run() {
        let dir = tempfile::tempdir().unwrap();
        let world = mock_world(vec![with_tool(
            "c1",
            "ask_user_text",
            serde_json::json!({"prompt": "?"}),
        )]);
        let mut events = world.events();
        let run_id = world
            .spawn(SpawnSpec::new(
                BlueprintSource::Toml(ASKER.to_string()),
                "ask",
                dir.path(),
            ))
            .await
            .expect("spawns");
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            events_until(&mut events, |e| matches!(e, WorldEvent::Interaction { .. })),
        )
        .await
        .expect("parked on the question");

        assert!(world.cancel(&run_id).await);
        let seen = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            events_until(&mut events, |e| matches!(e, WorldEvent::Completed { .. })),
        )
        .await
        .expect("terminal event after cancel");
        assert!(seen.iter().any(|e| {
            matches!(e, WorldEvent::Completed { status, .. } if status == "cancelled")
        }));
        world.shutdown().await;
    }

    /// A tool service that answers every call with a canned string; used to
    /// exercise the custom-service seam (no per-agent registration).
    struct CannedService;
    impl crate::pipeline::ToolService for CannedService {
        fn exec_for(
            &self,
            _entity: bevy_ecs::entity::Entity,
            calls: Vec<leviath_providers::ToolCall>,
        ) -> crate::tool_bridge::BoxedToolExec {
            Box::new(move || {
                Box::pin(async move {
                    calls
                        .into_iter()
                        .map(|c| (c.id, "canned".to_string()))
                        .collect()
                })
            })
        }
    }

    #[tokio::test]
    async fn every_builder_option_composes_and_state_dir_persists_runs() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let world = AgentWorld::builder()
            .provider(ProviderCreds::simple("ollama"))
            .register_provider(
                "mock",
                Arc::new(Mock {
                    responses: Mutex::new(
                        vec![
                            with_tool("c1", "read_file", serde_json::json!({"path": "x"})),
                            text("done"),
                            text("done"),
                        ]
                        .into_iter()
                        .collect(),
                    ),
                }),
            )
            .default_model("mock", "m")
            .state_dir(state.path())
            .inference_pool(InferencePoolConfig::new())
            .tool_concurrency(2)
            .build()
            .expect("all options compose");
        let mut events = world.events();
        let run_id = world
            .spawn(SpawnSpec::new(
                BlueprintSource::Toml(TWO_STAGE.to_string()),
                "persist me",
                dir.path(),
            ))
            .await
            .expect("spawns");
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            events_until(&mut events, |e| matches!(e, WorldEvent::Completed { .. })),
        )
        .await
        .expect("completes");
        world.shutdown().await;

        // The daemon's on-disk layout appeared under the state dir.
        let run_dir = state.path().join("runs").join(run_id.as_ref());
        assert!(run_dir.join("meta.json").exists());
        assert!(state.path().join("machine-id").exists());
    }

    #[tokio::test]
    async fn a_custom_tool_service_replaces_the_builtin_one() {
        let dir = tempfile::tempdir().unwrap();
        let world = AgentWorld::builder()
            .register_provider(
                "mock",
                Arc::new(Mock {
                    responses: Mutex::new(
                        vec![
                            with_tool("c1", "read_file", serde_json::json!({"path": "x"})),
                            text("done"),
                            text("done"),
                        ]
                        .into_iter()
                        .collect(),
                    ),
                }),
            )
            .tool_service(Arc::new(CannedService))
            .build()
            .expect("builds with a custom service");
        let mut events = world.events();
        world
            .spawn(SpawnSpec::new(
                BlueprintSource::Toml(TWO_STAGE.to_string()),
                "use the canned tools",
                dir.path(),
            ))
            .await
            .expect("spawns");
        let seen = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            events_until(&mut events, |e| matches!(e, WorldEvent::Completed { .. })),
        )
        .await
        .expect("completes");
        // The canned result (not a real file read) came back through the lane.
        assert!(seen.iter().any(|e| {
            matches!(e, WorldEvent::ToolCallFinished { summary, .. } if summary == "canned")
        }));
        world.shutdown().await;
    }

    #[tokio::test]
    async fn manifest_files_that_do_not_parse_or_validate_fail_the_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let world = mock_world(vec![]);

        let garbled = dir.path().join("garbled.leviath");
        std::fs::write(&garbled, "not = [valid").unwrap();
        let err = world
            .spawn(SpawnSpec::new(
                BlueprintSource::Path(garbled),
                "t",
                dir.path(),
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("parse manifest"));

        // A path with no file stem still spawns an attempt (and fails to read).
        let err = world
            .spawn(SpawnSpec::new(
                BlueprintSource::Path(PathBuf::from("")),
                "t",
                dir.path(),
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().starts_with("spawn error"));

        // A required caller-input region that was not provided fails before
        // any inference.
        let demanding = format!(
            "{TWO_STAGE}\nspec = {{ kind = \"pinned\", max_tokens = 2000, seed = \"input\", required = true }}\n"
        );
        let err = world
            .spawn(SpawnSpec::new(
                BlueprintSource::Toml(demanding),
                "t",
                dir.path(),
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("required region"));

        world.shutdown().await;
    }

    #[tokio::test]
    async fn requests_after_the_world_closes_fail_closed() {
        let world = mock_world(vec![]);
        // Stop the serve loop out from under the handle (without consuming
        // the AgentWorld, as shutdown() would).
        let (reply, _rx) = oneshot::channel();
        world
            .control
            .send(ControlOp::Shutdown { reply })
            .expect("world is up");
        // Wait until the serve loop is really gone (its rx dropped).
        while !world.control.is_closed() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let ghost = RunId("ghost".to_string());
        assert_eq!(world.status(&ghost).await, None);
        assert!(!world.pause(&ghost).await);
        assert!(!world.send_message(&ghost, "hello").await);
        let err = world
            .spawn(SpawnSpec::new(
                BlueprintSource::Toml(TWO_STAGE.to_string()),
                "t",
                std::env::temp_dir(),
            ))
            .await
            .unwrap_err();
        assert_eq!(err, EmbedError::ChannelClosed);
    }

    #[tokio::test]
    async fn inline_blueprints_spawn_and_invalid_ones_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let world = mock_world(vec![text("done"), text("done"), text("done"), text("done")]);
        let mut events = world.events();

        // An invalid inline blueprint (entry stage names nothing) is refused
        // before it reaches the world.
        let mut invalid = leviath_core::manifest::parse_manifest(TWO_STAGE).unwrap();
        invalid.entry_stage = Some("ghost".to_string());
        let err = world
            .spawn(SpawnSpec::new(
                BlueprintSource::Inline(Box::new(invalid)),
                "t",
                dir.path(),
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid blueprint"));

        // A valid one runs. Trim it to a single text-only stage.
        let mut valid = leviath_core::manifest::parse_manifest(TWO_STAGE).unwrap();
        valid.stages.truncate(1);
        valid.stages[0].transitions = None;
        valid.entry_stage = Some(valid.stages[0].name.clone());
        let run_id = world
            .spawn(SpawnSpec::new(
                BlueprintSource::Inline(Box::new(valid)),
                "just answer",
                dir.path(),
            ))
            .await
            .expect("inline blueprint spawns");
        assert!(run_id.as_ref().starts_with("embedded-"));
        let seen = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            events_until(&mut events, |e| matches!(e, WorldEvent::Completed { .. })),
        )
        .await
        .expect("completes");
        assert!(
            seen.iter()
                .any(|e| matches!(e, WorldEvent::Completed { .. }))
        );
        world.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_survives_an_aborted_serve_loop() {
        let world = mock_world(vec![]);
        // Kill the serve task out from under the world: shutdown must not
        // hang or panic when the join fails.
        world.serve_task.abort();
        world.shutdown().await;
    }

    #[tokio::test]
    async fn the_mock_provider_is_a_minimal_stub() {
        // Pins the fixture's inert answers so its impl stays measured (the
        // pipeline only calls infer when exact token counting is off).
        let mock = Mock {
            responses: Mutex::new(VecDeque::new()),
        };
        assert_eq!(mock.count_tokens("x", "m").await, 1);
        assert_eq!(mock.max_context_tokens("m"), 100_000);
        assert_eq!(mock.name(), "mock");
        let _ = mock.capabilities("m");
        assert!(
            mock.infer(
                serde_json::from_value(serde_json::json!({
                    "messages": [],
                    "model": "m",
                    "max_tokens": 1,
                    "temperature": 0.0,
                    "tools": [],
                    "extra": null,
                }))
                .unwrap()
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn pause_and_resume_round_trip_on_an_active_run() {
        // Zero inference permits for the model: the agent stays Active,
        // parked on the pool, which is exactly when pause applies.
        let dir = tempfile::tempdir().unwrap();
        let mut pool = InferencePoolConfig::new();
        pool.set_limit("m", 0);
        let world = AgentWorld::builder()
            .register_provider(
                "mock",
                Arc::new(Mock {
                    responses: Mutex::new(VecDeque::new()),
                }),
            )
            .inference_pool(pool)
            .build()
            .expect("builds");
        let run_id = world
            .spawn(SpawnSpec::new(
                BlueprintSource::Toml(ASKER.to_string()),
                "wait around",
                dir.path(),
            ))
            .await
            .expect("spawns");

        // Agents spawn Active, and with no permits nothing can change that.
        assert_eq!(world.status(&run_id).await, Some(AgentStatus::Active));
        assert!(world.pause(&run_id).await);
        assert!(world.resume(&run_id).await);
        assert!(world.cancel(&run_id).await);
        world.shutdown().await;
    }

    #[test]
    fn run_id_displays_as_its_string() {
        let id = RunId("coder-1-2".to_string());
        assert_eq!(id.to_string(), "coder-1-2");
        assert_eq!(id.as_ref(), "coder-1-2");
    }
}
