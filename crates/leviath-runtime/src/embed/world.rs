//! [`AgentWorld`]: the embedder-facing runtime. Build one with plain values,
//! spawn runs, watch the event stream, answer their questions, shut down.
//!
//! A run is asked for with a [`SpawnRequest`], the same typed request every
//! other front door takes. It names a blueprint the world was given (see
//! [`AgentWorldBuilder::blueprint`]) with its inputs, or carries a whole
//! graph. The world resolves it against an [`EmbedEnv`](super::EmbedEnv),
//! binds it and places it, exactly as the daemon does, and a request it
//! refuses comes back with every problem at once.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{broadcast, oneshot};

use super::spawner::{Blueprints, EmbedStarter};
use super::{BasicToolService, EmbedError, EventStream};
use crate::components::AgentStatus;
use crate::host::{ControlOp, WorldEvent, WorldHost};
use crate::inference_pool::InferencePoolConfig;
use crate::interaction_hub::InteractionHub;
use crate::pipeline::{ModelDefaults, ToolService};
use crate::provider_creds::ProviderCreds;
use crate::providers::ProviderRegistry;
use crate::spec::env::LoadedBlueprint;
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::RunId;
use crate::spec::request::SpawnRequest;
use crate::spec::summary::SpawnSummary;
use crate::state::RunState;
use crate::world::PipelineWorld;

/// The issue a request gets when the world has shut down under it.
fn closed() -> SpawnIssues {
    SpawnIssue::new(
        SpecPath::root(),
        IssueCode::Unavailable,
        EmbedError::ChannelClosed.to_string(),
    )
    .into()
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
    hints: leviath_core::config::PromptHints,
    runtime: Option<Handle>,
    blueprints: BTreeMap<String, LoadedBlueprint>,
    workdir: Option<PathBuf>,
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
            // Off unless asked for: an embedder owns its prompts, and the
            // daemon's `config.toml` defaults do not reach this path.
            hints: leviath_core::config::PromptHints {
                batch_tool: false,
                shell: false,
            },
            runtime: None,
            blueprints: BTreeMap::new(),
            workdir: None,
        }
    }

    /// Add a provider from credentials (repeatable). See [`ProviderCreds`]
    /// for the supported providers.
    pub fn provider(mut self, creds: ProviderCreds) -> Self {
        self.creds.push(creds);
        self
    }

    /// Offer a blueprint to the world's requests, under the name it carries
    /// (repeatable; a second blueprint of the same name replaces the first).
    /// Load one from its `agent.toml` with `leviath_blueprint::load`. A
    /// request names it with `SpawnSource::Blueprint`.
    pub fn blueprint(mut self, blueprint: LoadedBlueprint) -> Self {
        self.blueprints
            .insert(blueprint.reference.name.to_string(), blueprint);
        self
    }

    /// The directory a run whose request names no workdir works in. Without
    /// this, every request has to name one.
    pub fn workdir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.workdir = Some(dir.into());
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

    /// The provider bare model names route to, and the model every stage
    /// that allows a user default starts on while this is set - ahead of the
    /// models its blueprint names. The embedded form of `override_model`.
    pub fn override_model(mut self, provider: impl Into<String>, model: impl Into<String>) -> Self {
        self.defaults.provider = provider.into();
        self.defaults.override_model = Some(model.into());
        self
    }

    /// The provider bare model names route to, with no override: each stage
    /// keeps its blueprint's choice and open routes are asked of this
    /// provider first. The embedded form of `default_provider` on its own.
    pub fn default_provider(mut self, provider: impl Into<String>) -> Self {
        self.defaults.provider = provider.into();
        self
    }

    /// A model on the default provider tried after every model a stage names
    /// and before any [`fallback_route`](Self::fallback_route), never ahead
    /// of the blueprint's own choices. The embedded form of `fallback_model`.
    pub fn fallback_model(mut self, model: impl Into<String>) -> Self {
        self.defaults.fallback_model = Some(model.into());
        self
    }

    /// Append a host-wide failover target, tried after a stage's own entries
    /// and the user's models when the provider in use stops answering.
    ///
    /// Call it once per target, best first. This is what keeps a blueprint
    /// that names exactly one model running when that provider runs out of
    /// credits. A provider or model id that is not a valid name (one with
    /// whitespace in it, say) names nowhere a request could go, and is left
    /// out with a warning.
    pub fn fallback_route(mut self, provider: impl Into<String>, model: impl Into<String>) -> Self {
        let (provider, model) = (provider.into(), model.into());
        match crate::spec::names::ModelRef::parse(&format!("{provider}/{model}")) {
            Ok(route) => self.defaults.fallback_order.push(route),
            Err(e) => tracing::warn!(%provider, %model, "ignoring a fallback route: {e}"),
        }
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

    /// Opt in to the framework-authored system-prompt hints, off by default on
    /// this path. `shell` is worth turning on for a blueprint that grants the
    /// shell tool and may run on Windows: it tells the model that commands go
    /// through `cmd.exe` rather than a POSIX shell. A blueprint's `[agent]` or
    /// `[stages.<name>]` `batch_tool_hint` / `shell_hint` still overrides this.
    pub fn prompt_hints(mut self, hints: leviath_core::config::PromptHints) -> Self {
        self.hints = hints;
        self
    }

    /// Run the world on `handle` instead of the ambient Tokio runtime.
    pub fn runtime(mut self, handle: Handle) -> Self {
        self.runtime = Some(handle);
        self
    }

    /// Assemble the world and start its serve loop on the Tokio runtime.
    pub fn build(self) -> Result<AgentWorld, EmbedError> {
        self.build_with(&leviath_providers::provider::build_http_client)
    }

    /// [`build`](Self::build), with outbound-HTTPS-client construction injected.
    ///
    /// Exists so the "no usable client" path is reachable from a test: `reqwest`
    /// cannot be made to fail from the outside, so without a seam this error
    /// would be unreachable code.
    pub fn build_with(
        self,
        build_client: leviath_providers::provider::HttpClientFactory<'_>,
    ) -> Result<AgentWorld, EmbedError> {
        if self.creds.is_empty() && self.custom_providers.is_empty() {
            return Err(EmbedError::NoProviders);
        }
        let handle = match self.runtime {
            Some(handle) => handle,
            None => Handle::try_current().map_err(|_| EmbedError::NoRuntime)?,
        };

        let mut registry: ProviderRegistry =
            crate::provider_creds::build_provider_registry_with(&self.creds, build_client)
                .map_err(|e| EmbedError::ProviderClient(e.to_string()))?;
        for (name, provider) in self.custom_providers {
            registry.register(name, provider);
        }
        // Shares every provider with the world's copy, so a list read through
        // it is the list the spawn resolves against, and the starter chooses
        // models from the same set the world infers on.
        let unread_registry = registry.clone();
        // The providers a bare model name may route to: the only ones whose
        // unread list can hold a stage back, so the only ones worth asking.
        let preferred: Vec<String> = self
            .defaults
            .order()
            .into_iter()
            .map(str::to_string)
            .collect();

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

        let blueprints: Blueprints = Arc::new(Mutex::new(self.blueprints));
        host.set_starter(Arc::new(EmbedStarter {
            registry: unread_registry,
            creds: self.creds,
            defaults: self.defaults,
            hints: self.hints,
            basic_tools: basic_tools.clone(),
            blueprints: blueprints.clone(),
            workdir: self.workdir,
            preferred,
        }));
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
            blueprints,
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
    blueprints: Blueprints,
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

    /// Offer one more blueprint to the world's requests, as
    /// [`AgentWorldBuilder::blueprint`] does before the world starts.
    pub fn add_blueprint(&self, blueprint: LoadedBlueprint) {
        leviath_core::sync::lock(&self.blueprints)
            .insert(blueprint.reference.name.to_string(), blueprint);
    }

    /// Start the run `request` asks for. Returns its id once the run is live
    /// in the world: resolved, bound and placed. A request that cannot run
    /// comes back with every reason at once, each naming the place in the
    /// request it is about. A run its graph may keep from ever finishing
    /// still starts; [`validate`](Self::validate) answers with those
    /// warnings, and the world's log says them as the run starts.
    pub async fn spawn(&self, request: SpawnRequest) -> Result<RunId, SpawnIssues> {
        self.ask(|reply| ControlOp::Spawn {
            request: Box::new(request),
            reply,
        })
        .await
        .unwrap_or_else(|_| Err(closed()))
        .map(|spawned| spawned.run_id)
    }

    /// What `request` would run, without running it: the same checks
    /// [`spawn`](Self::spawn) makes, answered with a summary of the run.
    pub async fn validate(&self, request: SpawnRequest) -> Result<SpawnSummary, SpawnIssues> {
        self.ask(|reply| ControlOp::ValidateSpawn {
            request: Box::new(request),
            reply,
        })
        .await
        .unwrap_or_else(|_| Err(closed()))
    }

    /// A run's whole state: where it is, its context, what it is waiting on
    /// and what it has spent. Read live while the run is in the world, or
    /// from its run file once it has left (with a
    /// [`state_dir`](AgentWorldBuilder::state_dir)). `None` for a run the
    /// world does not know.
    pub async fn inspect(&self, id: &RunId) -> Option<RunState> {
        self.ask(|reply| ControlOp::Inspect {
            run_id: id.to_string(),
            reply,
        })
        .await
        .ok()
        .flatten()
        .map(|state| *state)
    }

    /// What a run handed back, or `None` if it has not submitted anything (or
    /// the world does not know the run).
    ///
    /// The counterpart to [`status`](Self::status): that says whether a run is
    /// done, this says what it concluded. An embedder watching [`EventStream`]
    /// for a `Completed` event has no other way to read a result short of
    /// scraping the log stream.
    pub async fn result(&self, id: &RunId) -> Option<leviath_core::output::FinalOutput> {
        self.ask(|reply| ControlOp::Result {
            run_id: id.to_string(),
            reply,
        })
        .await
        .ok()
        .flatten()
    }

    /// The bytes of one file a run handed back: an entry of
    /// [`result`](Self::result)'s `artifacts`.
    ///
    /// `None` for an artifact the run's store does not hold (one too large to
    /// store, or from a run this world never ran). An embedder used to get the
    /// artifact's name, type and hash from `result` and no way to read it
    /// short of knowing where the world keeps its files.
    pub async fn artifact_bytes(
        &self,
        id: &RunId,
        artifact: &leviath_core::output::Artifact,
    ) -> Option<Vec<u8>> {
        if artifact.sha256.is_empty() {
            return None;
        }
        self.ask(|reply| ControlOp::Blob {
            run_id: id.to_string(),
            sha256: artifact.sha256.clone(),
            reply,
        })
        .await
        .ok()
        .flatten()
    }

    /// Subscribe to the world's events, from this moment on.
    pub fn events(&self) -> EventStream {
        EventStream::new(self.events.subscribe())
    }

    /// A run's current status, or `None` if the world doesn't know it.
    pub async fn status(&self, id: &RunId) -> Option<AgentStatus> {
        self.ask(|reply| ControlOp::Status {
            run_id: id.to_string(),
            reply,
        })
        .await
        .ok()
        .flatten()
    }

    /// Deliver a message into a running agent's inbox. `false` when the
    /// world can no longer accept messages (shut down or shutting down), or
    /// when the message has neither text nor files.
    pub async fn send_message(&self, id: &RunId, content: &str) -> bool {
        self.send_message_with(id, content, Vec::new()).await
    }

    /// [`send_message`](Self::send_message) with files: the text and every
    /// part bound for its region land as one entry, and a part naming
    /// another region lands there on its own.
    pub async fn send_message_with(
        &self,
        id: &RunId,
        content: &str,
        parts: Vec<leviath_core::mime::InboundPart>,
    ) -> bool {
        self.ask(|reply| ControlOp::Message {
            agent_id: id.to_string(),
            content: content.to_string(),
            target_region: None,
            parts,
            reply,
        })
        .await
        .is_ok_and(|delivered| delivered == Ok(true))
    }

    /// Pause a run. `false` if there is no such live run.
    pub async fn pause(&self, id: &RunId) -> bool {
        self.ask(|reply| ControlOp::Pause {
            run_id: id.to_string(),
            reply,
        })
        .await
        .unwrap_or(false)
    }

    /// Resume a paused run. `false` if there is no such live run.
    pub async fn resume(&self, id: &RunId) -> bool {
        self.ask(|reply| ControlOp::Resume {
            run_id: id.to_string(),
            reply,
        })
        .await
        .unwrap_or(false)
    }

    /// Cancel a run. `false` if there is no such live run.
    pub async fn cancel(&self, id: &RunId) -> bool {
        self.ask(|reply| ControlOp::Cancel {
            run_id: id.to_string(),
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
            .map(|(agent_id, request)| {
                let id = RunId::new(agent_id).expect("the hub holds the ids the world minted");
                (id, request)
            })
            .collect()
    }

    /// Answer an open question (matched by the response's `request_id`).
    /// `false` if no such request is open, or if the answer is not one the
    /// question can take (text for a choice, say); [`Self::try_answer`] says
    /// which.
    pub fn answer(&self, response: leviath_core::interaction::InteractionResponse) -> bool {
        self.hub.answer(response)
    }

    /// [`Self::answer`], saying why an answer did not land. A refused answer
    /// leaves the question open.
    pub fn try_answer(
        &self,
        response: leviath_core::interaction::InteractionResponse,
    ) -> Result<(), crate::interaction_hub::AnswerError> {
        self.hub.try_answer(response)
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
    use std::path::Path;

    use crate::spec::inputs::RawInput;
    use crate::spec::names::BlueprintRef;
    use crate::spec::request::SpawnSource;

    /// A scripted provider: pops one canned response per inference call.
    struct Mock {
        responses: Mutex<VecDeque<InferenceResponse>>,
    }

    #[async_trait::async_trait]
    impl Provider for Mock {
        async fn infer(&self, _r: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
            leviath_core::sync::lock(&self.responses)
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
            parts: Vec::new(),
            content: content.to_string(),
            tool_calls: vec![],
            tokens_used: TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0,
                reported_cost_usd: None,
            },
            finish_reason: FinishReason::Complete,
            reasoning: None,
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

    /// A provider that records the system blocks of every request it is handed,
    /// then answers "done". For asserting what the framework prepends.
    struct Recorder {
        seen: Arc<Mutex<Vec<Vec<String>>>>,
    }

    #[async_trait::async_trait]
    impl Provider for Recorder {
        async fn infer(&self, r: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
            leviath_core::sync::lock(&self.seen)
                .push(r.system.iter().map(|b| b.text.clone()).collect());
            Ok(text("done"))
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

    /// A two-stage graph, as the `[graph]` table of an `agent.toml` holds it.
    const TWO_STAGE: &str = r#"title = "embedded"
description = "Two stage embedded test agent."
entry = "work"
edges = [{ name = "wrap", from = "work", to = "wrap" }]

[[stages]]
name = "work"
description = "Do the work"
system_prompt = "Work."
model = { models = [{ provider = "mock", model = "m" }] }
tools = ["read_file"]

[[stages]]
name = "wrap"
description = "Wrap up"
system_prompt = "Wrap."
model = { models = [{ provider = "mock", model = "m" }] }

[layout]
total_budget_tokens = 20000

[[layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 40 }
budget = 20000
"#;

    const ASKER: &str = r#"title = "asker"
description = "Asks one question then finishes."
entry = "chat"

[[stages]]
name = "chat"
description = "Chat"
system_prompt = "Ask."
model = { models = [{ provider = "mock", model = "m" }] }
tools = ["ask_user_text"]

[layout]
total_budget_tokens = 20000

[[layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 40 }
budget = 20000
"#;

    /// `graph` (one of the graphs above, whose regions share 20000 tokens)
    /// with one more pinned region of `budget` tokens, filled from a text
    /// input of the same name.
    fn with_input(graph: &str, name: &str, budget: u32, required: bool) -> String {
        let total = 20_000 + budget;
        format!(
            "{}\n[[layout.regions]]\nname = \"{name}\"\nkind = \"pinned\"\nbudget = {budget}\n\
             required = {required}\n\n[[inputs]]\nname = \"{name}\"\n\
             type = {{ kind = \"text\", multiline = true }}\nrequired = {required}\n\
             binds = [{{ region = \"{name}\" }}]\n",
            graph.replace(
                "total_budget_tokens = 20000",
                &format!("total_budget_tokens = {total}"),
            )
        )
    }

    /// The blueprint `graph` describes, named after its title and pinned to
    /// its text, reading its files from `dir`.
    fn blueprint(graph: &str, dir: &Path) -> LoadedBlueprint {
        let graph_def: crate::spec::graph::RunGraph =
            toml::from_str(graph).expect("the test graph reads");
        let name = graph_def.title.clone().expect("the test graph has a title");
        LoadedBlueprint {
            graph: graph_def,
            reference: BlueprintRef {
                name: crate::spec::names::BlueprintName::new(name).expect("a valid name"),
                digest: Some(crate::spec::names::Digest::of(graph.as_bytes())),
            },
            version: "0.0.0".to_string(),
            base_dir: dir.to_path_buf(),
        }
    }

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

    /// The blueprint `graph` describes, offered to `world` under its own
    /// name, and a request for it working in `dir`, with `task` as its task
    /// when the blueprint takes one.
    fn request(world: &AgentWorld, graph: &str, task: &str, dir: &Path) -> SpawnRequest {
        let loaded = blueprint(graph, dir);
        let takes_task = loaded
            .graph
            .inputs
            .iter()
            .any(|d| d.name.as_str() == "task");
        let name = loaded.reference.name.clone();
        world.add_blueprint(loaded);
        let mut request =
            SpawnRequest::new(SpawnSource::Blueprint(BlueprintRef { name, digest: None }));
        if takes_task {
            request = request.input("task", RawInput::Text(task.to_string()));
        }
        request.workdir = Some(dir.to_path_buf());
        request
    }

    /// A check resolves the registered blueprint the request names without
    /// starting it; one that names nothing registered is refused.
    #[tokio::test]
    async fn a_check_resolves_a_registered_blueprint_and_refuses_an_unknown_one() {
        use crate::host::RunStarter;
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(Mock {
                responses: Mutex::new(VecDeque::new()),
            }),
        );
        let blueprints: Blueprints = Default::default();
        let loaded = blueprint(TWO_STAGE, &std::env::temp_dir());
        leviath_core::sync::lock(&blueprints).insert("embedded".to_string(), loaded);
        let starter = EmbedStarter {
            registry,
            creds: Vec::new(),
            defaults: Default::default(),
            hints: Default::default(),
            basic_tools: None,
            blueprints,
            workdir: Some(std::env::temp_dir()),
            preferred: Vec::new(),
        };
        let named = |name: &str| {
            SpawnRequest::new(SpawnSource::Blueprint(BlueprintRef::parse(name).unwrap()))
        };
        let summary = starter
            .check(named("embedded"), crate::spec::env::Caller::TopLevel)
            .await
            .expect("the registered blueprint checks, in the default workdir");
        assert_eq!(summary.title, "embedded");
        assert!(
            starter
                .check(named("nowhere"), crate::spec::env::Caller::TopLevel)
                .await
                .is_err()
        );
        // A graph of the caller's own needs nothing registered.
        let loaded = blueprint(TWO_STAGE, &std::env::temp_dir());
        let raw = SpawnRequest::new(SpawnSource::Raw(Box::new(loaded.graph)));
        assert!(
            starter
                .check(raw, crate::spec::env::Caller::TopLevel)
                .await
                .is_ok()
        );
    }

    /// A blueprint with a task region takes the spawn's task.
    #[tokio::test]
    async fn a_task_reaches_a_blueprint_with_a_task_region() {
        let dir = tempfile::tempdir().unwrap();
        let world = mock_world(vec![text("done"), text("done"), text("done")]);
        let graph = with_input(ASKER, "task", 500, false);
        world
            .spawn(request(&world, &graph, "say hello", dir.path()))
            .await
            .expect("spawns");
        world.shutdown().await;
    }

    /// A dry run answers with what would run and starts nothing; the issues
    /// it finds are the ones a spawn of the same request would get.
    #[tokio::test]
    async fn a_dry_run_says_what_would_run_or_everything_wrong() {
        let dir = tempfile::tempdir().unwrap();
        let world = mock_world(vec![]);
        let summary = world
            .validate(request(&world, TWO_STAGE, "t", dir.path()))
            .await
            .expect("valid");
        assert_eq!(summary.title, "embedded");
        assert_eq!(summary.stages.len(), 2);
        assert_eq!(summary.stages[0].model.as_str(), "m");

        let mut bad = request(&world, TWO_STAGE, "t", dir.path());
        bad = bad.input("nonsense", RawInput::Int(1));
        bad.workdir = Some(dir.path().join("nope"));
        let issues = world.validate(bad.clone()).await.unwrap_err();
        let paths: Vec<String> = issues.iter().map(|i| i.path.to_string()).collect();
        assert!(paths.contains(&"inputs.nonsense".to_string()), "{issues}");
        assert!(paths.contains(&"workdir".to_string()), "{issues}");
        assert_eq!(world.spawn(bad).await.unwrap_err(), issues);
        world.shutdown().await;
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
            .spawn(request(
                &world,
                TWO_STAGE,
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
            .spawn(request(&world, ASKER, "pick a database", dir.path()))
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
        assert!(
            world
                .send_message_with(
                    &run_id,
                    "and see @sketch.png",
                    vec![leviath_core::mime::InboundPart::from_bytes(
                        "sketch.png",
                        b"\x89PNG\r\n\x1a\nsketch".to_vec()
                    )],
                )
                .await
        );
        // A message that says nothing is refused, not delivered.
        assert!(!world.send_message(&run_id, "  ").await);

        // An answer the question cannot take is refused with the reason, and
        // the question stays open for a right one.
        assert_eq!(
            world.try_answer(leviath_core::interaction::InteractionResponse::choice(
                request.id.clone(),
                0
            )),
            Err(crate::interaction_hub::AnswerError::Refused(format!(
                "'{}' is a text question: answer it with text",
                request.id
            )))
        );
        assert_eq!(world.pending_inputs().len(), 1);

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

    /// A request naming a blueprint the world was not given, in a workdir
    /// that is not there, hears about both at once, each at its place.
    #[tokio::test]
    async fn spawn_reports_every_problem_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let world = mock_world(vec![]);
        let _ = request(&world, TWO_STAGE, "t", dir.path());
        let mut unknown = SpawnRequest::new(SpawnSource::Blueprint(
            BlueprintRef::parse("nowhere").unwrap(),
        ));
        unknown.workdir = Some(dir.path().join("nope"));
        let issues = world.spawn(unknown).await.unwrap_err();
        let paths: Vec<String> = issues.iter().map(|i| i.path.to_string()).collect();
        assert_eq!(paths[0], "source.blueprint", "{issues}");
        assert_eq!(issues.0[0].code, IssueCode::Unresolvable);
        assert_eq!(issues.0[0].known, ["embedded"]);
        assert!(
            issues.iter().any(|i| i.path.to_string() == "workdir"),
            "{issues}"
        );
        world.shutdown().await;
    }

    /// A graph the embedder wrote runs as it is, in the builder's default
    /// workdir, and its state reads back while it is live.
    #[tokio::test]
    async fn a_graph_of_the_callers_own_runs_and_can_be_inspected() {
        let dir = tempfile::tempdir().unwrap();
        // No inference permits: the run stays in its first stage, live.
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
            .workdir(dir.path())
            .build()
            .unwrap();
        let graph = blueprint(TWO_STAGE, dir.path()).graph;
        let run_id = world
            .spawn(SpawnRequest::new(SpawnSource::Raw(Box::new(graph))))
            .await
            .expect("spawns the graph");
        let live = world
            .inspect(&run_id)
            .await
            .expect("a live run has a state");
        assert_eq!(live.cursor.stage.as_str(), "work");
        assert!(world.cancel(&run_id).await);
        world.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_runs_answer_negatively() {
        let world = mock_world(vec![]);
        let ghost = RunId::new("no-such-run").unwrap();
        assert_eq!(world.status(&ghost).await, None);
        assert!(world.inspect(&ghost).await.is_none());
        assert_eq!(world.result(&ghost).await, None);
        // An artifact the store never held, and one that was never stored at
        // all (no hash), both answer nothing rather than erroring.
        let mut artifact = leviath_core::output::Artifact::from_path("out/scene.glb");
        assert_eq!(world.artifact_bytes(&ghost, &artifact).await, None);
        artifact.sha256 = "ab".repeat(32);
        assert_eq!(world.artifact_bytes(&ghost, &artifact).await, None);
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
            .spawn(request(&world, ASKER, "ask", dir.path()))
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
            _progress: crate::pipeline::ToolProgress,
        ) -> crate::tool_bridge::BoxedToolExec {
            Box::new(move || {
                Box::pin(
                    async move { calls.into_iter().map(|c| (c.id, "canned".into())).collect() },
                )
            })
        }
    }

    /// `default_provider` names the route for bare model names and nothing
    /// else; `fallback_model` is the model behind every stage's own list.
    /// Neither touches the override, so each stage keeps its blueprint's
    /// choice.
    #[test]
    fn a_default_provider_and_a_fallback_model_leave_the_override_unset() {
        let builder = AgentWorldBuilder::new()
            .default_provider("openrouter")
            .fallback_model("deepseek-v4-flash");
        assert_eq!(builder.defaults.provider, "openrouter");
        assert_eq!(builder.defaults.override_model, None);
        assert_eq!(
            builder.defaults.fallback_model.as_deref(),
            Some("deepseek-v4-flash")
        );
    }

    /// The failover chain is ordered and additive, and setting a default model
    /// afterwards must not wipe it.
    #[test]
    fn fallback_models_accumulate_in_order_beside_the_default() {
        let builder = AgentWorldBuilder::new()
            .fallback_route("anthropic", "sonnet")
            .fallback_route("openai", "gpt")
            .override_model("openrouter", "deepseek");
        assert_eq!(builder.defaults.provider, "openrouter");
        assert_eq!(builder.defaults.override_model.as_deref(), Some("deepseek"));
        assert_eq!(
            builder
                .defaults
                .fallback_order
                .iter()
                .map(|e| (e.provider_or_empty(), e.model.as_str()))
                .collect::<Vec<_>>(),
            vec![("anthropic", "sonnet"), ("openai", "gpt")]
        );
    }

    /// A route whose model id is not a valid name goes nowhere, so it is left
    /// out rather than carried into every stage's failover chain.
    #[test]
    fn a_fallback_route_that_names_nothing_is_left_out() {
        crate::test_support::with_tracing(|| {
            let builder = AgentWorldBuilder::new().fallback_route("openai", "has space");
            assert!(builder.defaults.fallback_order.is_empty());
        });
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
            .override_model("mock", "m")
            // Repeated on purpose: the chain is ordered, so it must accumulate
            // rather than replace, and it must not disturb the default model.
            .fallback_route("ollama", "llama")
            .fallback_route("mock", "spare")
            .state_dir(state.path())
            .inference_pool(InferencePoolConfig::new())
            .tool_concurrency(2)
            .blueprint(blueprint(TWO_STAGE, dir.path()))
            .workdir(dir.path())
            .build()
            .expect("all options compose");
        let mut events = world.events();
        // The builder's blueprint, in the builder's workdir.
        let run_id = world
            .spawn(SpawnRequest::new(SpawnSource::Blueprint(
                BlueprintRef::parse("embedded").unwrap(),
            )))
            .await
            .expect("spawns");
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            events_until(&mut events, |e| matches!(e, WorldEvent::Completed { .. })),
        )
        .await
        .expect("completes");
        // A finished run's state still reads back.
        assert!(world.inspect(&run_id).await.is_some());
        world.shutdown().await;

        // The daemon's on-disk layout appeared under the state dir.
        let run_dir = state.path().join("runs").join(run_id.as_ref());
        assert!(run_dir.join(leviath_core::files::RUN_FILE).exists());
        assert!(state.path().join("machine-id").exists());
    }

    /// The single-stage blueprint the hint tests drive, with a shell tool so the
    /// shell hint's tool guard is satisfied.
    const SHELL_STAGE: &str = r#"title = "shelly"
description = "One stage that can run commands."
entry = "work"

[[stages]]
name = "work"
description = "Do the work"
system_prompt = "Work."
model = { models = [{ provider = "mock", model = "m" }] }
tools = ["shell"]

[layout]
total_budget_tokens = 22000

[[layout.regions]]
name = "instructions"
kind = "pinned"
budget = 2000

[[layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 40 }
budget = 20000
"#;

    /// Run `SHELL_STAGE` once against a [`Recorder`] and hand back the system
    /// blocks of the first request, with `hints` as the world's global toggles.
    async fn system_blocks_with(hints: leviath_core::config::PromptHints) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let world = AgentWorld::builder()
            .register_provider(
                "mock",
                Arc::new(Recorder {
                    seen: Arc::clone(&seen),
                }),
            )
            .prompt_hints(hints)
            .build()
            .expect("world builds inside the test runtime");
        let mut events = world.events();
        world
            .spawn(request(&world, SHELL_STAGE, "go", dir.path()))
            .await
            .expect("spawns");
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            events_until(&mut events, |e| matches!(e, WorldEvent::Completed { .. })),
        )
        .await
        .expect("completes");
        world.shutdown().await;
        let seen = leviath_core::sync::lock(&seen);
        seen.first().cloned().expect("one inference happened")
    }

    #[tokio::test]
    async fn prompt_hints_reach_the_request_and_are_off_by_default() {
        // Off unless asked for: an embedder that never calls `prompt_hints`
        // gets exactly the blueprint's own prompt.
        let default_blocks = system_blocks_with(leviath_core::config::PromptHints {
            batch_tool: false,
            shell: false,
        })
        .await;
        assert!(
            default_blocks
                .iter()
                .all(|b| b != crate::pipeline::BATCH_TOOL_HINT),
        );
        assert!(default_blocks.iter().any(|b| b.contains("Work.")));

        // Turned on, the hint leads the prefix. The shell hint rides the same
        // path but only says anything on Windows, so this asserts on the batch
        // hint, which is the platform-independent half of the plumbing.
        let hinted = system_blocks_with(leviath_core::config::PromptHints {
            batch_tool: true,
            shell: true,
        })
        .await;
        assert_eq!(
            hinted.first().map(String::as_str),
            Some(crate::pipeline::BATCH_TOOL_HINT)
        );
        // Whatever the host OS says about its shell is what the run carries.
        let shell_hint = crate::pipeline::shell_guidance_for(std::env::consts::OS);
        assert_eq!(
            hinted.iter().any(|b| Some(b.as_str()) == shell_hint),
            shell_hint.is_some(),
        );
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
            .spawn(request(
                &world,
                TWO_STAGE,
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

    /// An input the blueprint requires and the request leaves out fails the
    /// spawn before any inference, naming the input.
    #[tokio::test]
    async fn a_missing_required_input_fails_the_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let world = mock_world(vec![]);
        let demanding = with_input(TWO_STAGE, "spec", 2000, true);
        let issues = world
            .spawn(request(&world, &demanding, "t", dir.path()))
            .await
            .unwrap_err();
        assert!(
            issues.iter().any(|i| i.path.to_string() == "inputs.spec"),
            "{issues}"
        );
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
        leviath_testkit::wait_until("the serve loop shut down", || world.control.is_closed()).await;
        let ghost = RunId::new("ghost").unwrap();
        assert_eq!(world.status(&ghost).await, None);
        assert!(!world.pause(&ghost).await);
        assert!(!world.send_message(&ghost, "hello").await);
        // A spawn or a dry run says the world is gone, as an issue.
        let spawn = request(&world, TWO_STAGE, "t", &std::env::temp_dir());
        let issues = world.spawn(spawn.clone()).await.unwrap_err();
        assert_eq!(issues.0[0].code, IssueCode::Unavailable);
        assert!(issues.to_string().contains("shut down"), "{issues}");
        assert_eq!(world.validate(spawn).await.unwrap_err(), issues);
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

        // Same for the recording fixture, which answers identically and is
        // registered under the same name.
        let recorder = Recorder {
            seen: Arc::new(Mutex::new(Vec::new())),
        };
        assert_eq!(recorder.count_tokens("x", "m").await, 1);
        assert_eq!(recorder.max_context_tokens("m"), 100_000);
        assert_eq!(recorder.name(), "mock");
        let _ = recorder.capabilities("m");
        assert!(
            mock.infer(
                &serde_json::from_value(serde_json::json!({
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
            .spawn(request(&world, ASKER, "wait around", dir.path()))
            .await
            .expect("spawns");

        // Agents spawn Active, and with no permits nothing can change that.
        assert_eq!(world.status(&run_id).await, Some(AgentStatus::Active));
        assert!(world.pause(&run_id).await);
        assert!(world.resume(&run_id).await);
        assert!(world.cancel(&run_id).await);
        world.shutdown().await;
    }

    #[tokio::test]
    async fn a_world_whose_provider_client_will_not_build_reports_it() {
        // The failure a machine with an unreadable root certificate store would
        // hit. Reachable only through the injected factory: reqwest cannot be
        // made to fail from the outside.
        let failing = |_t: Option<u64>| Err(leviath_providers::provider::malformed_url_error());
        let mut cred = ProviderCreds::simple("anthropic");
        cred.api_key = Some("k".to_string());
        let err = AgentWorld::builder()
            .provider(cred)
            .build_with(&failing)
            .err()
            .expect("a failing client factory should fail the build");
        // Discriminant rather than `matches!`: the macro expands to a match
        // with a `_ => false` arm that nothing reaches, which the 100% gate
        // reads as an uncovered region. Static assert messages for the same
        // reason - an interpolated one is only evaluated on failure.
        assert_eq!(
            std::mem::discriminant(&err),
            std::mem::discriminant(&EmbedError::ProviderClient(String::new()))
        );
        assert!(err.to_string().contains("provider HTTPS client error"));
    }
}
