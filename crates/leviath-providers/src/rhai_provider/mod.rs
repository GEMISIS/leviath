//! Drop-in LLM providers defined by a Rhai script.
//!
//! A `.rhai` script in `~/.leviath/providers/` implements the API-specific
//! *format mapping* (request → HTTP body, response JSON → Leviath types); this
//! Rust [`RhaiProvider`] wraps it with the full [`Provider`]
//! trait and owns the hard runtime concerns - HTTP transport, rate limiting,
//! per-stage timeouts, retry/error-classification, and token counting.
//!
//! ## Async ↔ sync bridge
//!
//! Rhai is synchronous but `Provider` is async. The script runs on a
//! `spawn_blocking` thread; its HTTP host functions send a [`BrokerJob`] over a
//! channel and block that thread on a reply, while an async **broker** on the
//! runtime services the job with the shared [`HttpExecutor`]. This is
//! runtime-flavor-agnostic (unlike `Handle::block_on` inside `spawn_blocking`,
//! which can deadlock on a current-thread runtime).
//!
//! ## Script contract
//!
//! Required: `initialize(config) -> Map` (runs **offline** - no network host
//! functions are registered for it) and `inference(state, request) -> Map`.
//! Optional: `stream(state, request, on_chunk)`, `count_tokens(state, text,
//! model) -> int`, `list_models(state) -> Array`.

mod convert;
mod engine;
pub mod host;
mod meta;

use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_core::Stream;
use rhai::{AST, Dynamic, Engine, FnPtr, Scope};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::provider::{
    InferenceRequest, InferenceResponse, ModelCapabilities, ModelCapabilityOverride, ModelInfo,
    Provider, ProviderError, RateLimitConfig, Result, StreamChunk, ToolCallDelta,
};
use crate::rate_limit::RateLimiter;

use convert::{map_rhai_err, parse_inference_dynamic};
use engine::{ExecConfig, build_exec_engine, build_init_engine};
use host::{BrokerJob, HostHttpError, HttpExecutor};

pub use meta::{ProviderMeta, parse_provider_annotations};

/// The entry points every provider script must define, as the error messages
/// spell them: the name, how many parameters it takes, and the parameter list
/// to show whoever has to fix it.
const REQUIRED_FNS: [(&str, usize, &str); 2] = [
    ("initialize", 1, "config"),
    ("inference", 2, "state, request"),
];

/// Compile a provider script and check its shape without running any of it.
///
/// The counterpart to `leviath_scripting::tool::check_source` and
/// `region_hook::compile`: what lets an editor find out whether a script is
/// usable before a run does. It stops at the AST on purpose, because
/// `initialize` is script code and the caller compiling arbitrary submitted
/// text is an ungated HTTP route.
///
/// The engine is the one [`RhaiProvider::from_source`] compiles with rather
/// than a bare `Engine::new`, so the verdict is the one a load would reach.
/// That is not cosmetic: the shared hardening raises Rhai's expression-depth
/// limits, which are low enough in a debug build to reject legitimate scripts.
pub fn check_source(label: &str, src: &str) -> Result<ProviderMeta> {
    let meta = parse_provider_annotations(src);
    let ast = build_init_engine(Arc::new(Vec::new()))
        .compile(src)
        .map_err(|e| ProviderError::Other(format!("compile provider script {label}: {e}")))?;
    require_entry_points(label, &ast)?;
    Ok(meta)
}

/// Check that a compiled script defines `initialize` and `inference`.
///
/// Both are required, but only `initialize` used to be caught at load, because
/// loading calls it. A script with no `inference` compiled, initialized and
/// cached, then failed at the first real inference - by which point a run had
/// started and the failure looked like a provider outage rather than a typo.
/// Reading it off the AST moves that to the moment the script is read.
fn require_entry_points(label: &str, ast: &AST) -> Result<()> {
    for (name, params, signature) in REQUIRED_FNS {
        if has_fn(ast, name, params) {
            continue;
        }
        let found = ast
            .iter_functions()
            .find(|f| f.name == name)
            .map(|f| f.params.len());
        return Err(ProviderError::Other(match found {
            Some(n) => {
                format!("{label}: fn {name} must take {params} parameters ({signature}), found {n}")
            }
            None => format!("{label}: script must define fn {name}({signature})"),
        }));
    }
    Ok(())
}

/// A boxed script-function call: builds the `Scope` and invokes one Rhai
/// function on the given engine, returning its `Dynamic` result. Boxed (not a
/// generic) so [`RhaiProvider::dispatch`] has a single instantiation.
type ScriptCall = Box<dyn FnOnce(&Engine) -> Result<Dynamic> + Send>;

/// A provider whose request/response mapping is implemented by a Rhai script.
pub struct RhaiProvider {
    name: String,
    ast: Arc<AST>,
    /// Persisted `initialize(config)` result, pushed (cloned) into each call.
    state: Arc<Dynamic>,
    executor: Arc<dyn HttpExecutor>,
    rate_limiter: Option<RateLimiter>,
    request_timeout_secs: Option<u64>,
    meta: ProviderMeta,
    capability_overrides: HashMap<String, ModelCapabilityOverride>,
    has_stream: bool,
    has_count_tokens: bool,
    has_list_models: bool,
    /// `[security] allow_env_vars`: credential-shaped environment variables this
    /// script may read via `env_var`. Held so each per-call execution engine is
    /// built with the same policy the init engine was.
    env_allowlist: Arc<Vec<String>>,
}

/// What a script provider is configured with, as distinct from the script
/// itself and the executor it runs against.
///
/// Held apart from those two because they are the parts a test substitutes: the
/// source is what is under test and the executor is what keeps it off the
/// network, while everything here comes from `config.toml` either way.
pub struct ScriptProviderSettings {
    /// The provider name this script is registered under.
    pub name: String,
    /// The `initialize` block from the config.
    pub init_config: serde_json::Value,
    /// Per-model capabilities the config declares.
    pub caps: HashMap<String, ModelCapabilityOverride>,
    /// Rate limiting, when configured.
    pub rate_limit: Option<RateLimitConfig>,
    /// Per-request timeout, when configured.
    pub request_timeout_secs: Option<u64>,
    /// Environment variables the script may read.
    pub env_allowlist: Arc<Vec<String>>,
}

impl RhaiProvider {
    /// Load a provider from a script file. Reads + compiles the script and runs
    /// `initialize(config)` **offline**; any failure (missing file, compile
    /// error, `initialize` throw) is returned as an error so the caller can
    /// skip-with-warning.
    ///
    /// The HTTP executor is supplied rather than built here: constructing one
    /// can fail (it reads the machine's root certificate store), and the caller
    /// builds it once for every script provider instead of once per script.
    pub fn from_script(
        script_path: &Path,
        executor: Arc<dyn HttpExecutor>,
        settings: ScriptProviderSettings,
    ) -> Result<Self> {
        let src = std::fs::read_to_string(script_path).map_err(|e| {
            ProviderError::Other(format!(
                "read provider script {}: {e}",
                script_path.display()
            ))
        })?;
        Self::from_source(&src, executor, settings)
    }

    /// Build a provider from in-memory source with an injected [`HttpExecutor`]
    /// (used by tests to avoid real network I/O).
    pub fn from_source(
        src: &str,
        executor: Arc<dyn HttpExecutor>,
        settings: ScriptProviderSettings,
    ) -> Result<Self> {
        let ScriptProviderSettings {
            name,
            init_config,
            caps,
            rate_limit,
            request_timeout_secs,
            env_allowlist,
        } = settings;
        let meta = parse_provider_annotations(src);
        let init_engine = build_init_engine(env_allowlist.clone());
        let ast = init_engine
            .compile(src)
            .map_err(|e| ProviderError::Other(format!("compile provider script {name}: {e}")))?;
        // Before `initialize` runs, so a script with no `inference` is refused
        // here rather than at the first inference. A refused script is skipped
        // with a warning and selection falls through to the next model, the
        // same way a syntax error already behaves.
        require_entry_points(&name, &ast)?;

        // Run initialize(config) offline (no network host fns registered).
        let config_dyn = rhai::serde::to_dynamic(init_config).unwrap_or(Dynamic::UNIT);
        let mut scope = Scope::new();
        let state = init_engine
            .call_fn::<Dynamic>(&mut scope, &ast, "initialize", (config_dyn,))
            .map_err(map_rhai_err)?;

        let has_stream = has_fn(&ast, "stream", 3);
        let has_count_tokens = has_fn(&ast, "count_tokens", 3);
        let has_list_models = has_fn(&ast, "list_models", 1);

        Ok(Self {
            name,
            ast: Arc::new(ast),
            state: Arc::new(state),
            executor,
            rate_limiter: rate_limit.as_ref().map(RateLimiter::new),
            request_timeout_secs,
            meta,
            capability_overrides: caps,
            has_stream,
            has_count_tokens,
            has_list_models,
            env_allowlist,
        })
    }

    /// The script's declared metadata.
    pub fn meta(&self) -> &ProviderMeta {
        &self.meta
    }

    /// Effective per-request timeout for a call: the stage value wins, else the
    /// provider default.
    fn effective_timeout(&self, request: &InferenceRequest) -> Option<u64> {
        request.request_timeout_secs.or(self.request_timeout_secs)
    }

    /// Run a script function on a blocking thread while an async broker services
    /// its HTTP host-function jobs, returning the function's `Dynamic` result.
    ///
    /// `call` is boxed (not a generic) so this method has a single instantiation:
    /// every caller's regions merge into one, keeping coverage exact.
    async fn dispatch(&self, timeout_secs: Option<u64>, call: ScriptCall) -> Result<Dynamic> {
        let (job_tx, mut job_rx) = mpsc::unbounded_channel::<BrokerJob>();
        let env_allowlist = self.env_allowlist.clone();
        let mut join: JoinHandle<Result<Dynamic>> = tokio::task::spawn_blocking(move || {
            let engine = build_exec_engine(ExecConfig {
                jobs: job_tx,
                timeout_secs,
                chunk_tx: None,
                env_allowlist,
            });
            call(&engine)
        });
        let script_result = loop {
            tokio::select! {
                biased;
                res = &mut join => break res,
                // When the script finishes, its job sender drops and `recv()`
                // yields `None`: the pattern stops matching, disabling this
                // branch so `select!` settles on the `join` branch. No spin.
                Some(job) = job_rx.recv() => {
                    serve_job(&self.executor, &self.rate_limiter, job).await
                }
            }
        };
        script_result.map_err(task_failed)?
    }

    /// Call the script's optional `count_tokens(state, text, model)`.
    async fn script_count_tokens(&self, text: &str, model: &str) -> Result<usize> {
        let ast = self.ast.clone();
        let state = (*self.state).clone();
        let text = text.to_string();
        let model = model.to_string();
        let value = self
            .dispatch(
                self.request_timeout_secs,
                Box::new(move |engine| {
                    let mut scope = Scope::new();
                    engine
                        .call_fn::<Dynamic>(&mut scope, &ast, "count_tokens", (state, text, model))
                        .map_err(map_rhai_err)
                }),
            )
            .await?;
        value
            .as_int()
            .map(|n| n.max(0) as usize)
            .map_err(|_| ProviderError::InvalidResponse("count_tokens must return an int".into()))
    }
}

/// Whether the compiled AST defines a script function of the given name/arity.
fn has_fn(ast: &AST, name: &str, params: usize) -> bool {
    ast.iter_functions()
        .any(|f| f.name == name && f.params.len() == params)
}

/// Map a blocking-task join failure (a panic in the script thread) to a
/// [`ProviderError`]. A free function so its single region is covered directly.
fn task_failed(e: tokio::task::JoinError) -> ProviderError {
    ProviderError::Other(format!("provider script task failed: {e}"))
}

/// Finalize a streaming script task: on a script or task error, push it as the
/// stream's terminal item. A free function so all three arms are unit-testable.
fn finalize_stream(
    outcome: std::result::Result<Result<()>, tokio::task::JoinError>,
    err_tx: &mpsc::UnboundedSender<Result<StreamChunk>>,
) {
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let _ = err_tx.send(Err(e));
        }
        Err(join_err) => {
            let _ = err_tx.send(Err(task_failed(join_err)));
        }
    }
}

/// Service one broker job: perform the HTTP request and (for unary jobs) reply.
/// Rate-limit accounting lives here so the provider owns it, not the executor.
async fn serve_job(
    executor: &Arc<dyn HttpExecutor>,
    rate_limiter: &Option<RateLimiter>,
    job: BrokerJob,
) {
    match job {
        BrokerJob::Unary(j) => {
            let result = executor.execute(j.request).await;
            match &result {
                Ok(_) => {
                    if let Some(rl) = rate_limiter {
                        rl.reset_backoff().await;
                    }
                }
                Err(HostHttpError::RateLimited { retry_after }) => {
                    if let Some(rl) = rate_limiter {
                        rl.handle_rate_limit(*retry_after).await;
                    }
                }
                Err(_) => {}
            }
            let _ = j.reply.send(result);
        }
        BrokerJob::Stream(j) => {
            executor.execute_stream(j.request, j.events).await;
        }
    }
}

/// Serialize an [`InferenceRequest`] into the Rhai `request` map handed to the
/// script. Both steps are infallible for a well-formed request (derive-Serialize
/// with string keys → JSON → Dynamic), so a failure is a programmer error.
fn request_to_dynamic(request: &InferenceRequest) -> Dynamic {
    let json = serde_json::to_value(request).expect("InferenceRequest serializes to JSON");
    rhai::serde::to_dynamic(json).expect("a JSON value converts to Dynamic")
}

/// Build the single collapsed chunk the default (non-native) streaming path
/// emits from a full [`InferenceResponse`].
fn collapse_chunk(response: InferenceResponse) -> StreamChunk {
    StreamChunk {
        delta: response.content,
        tool_calls: response
            .tool_calls
            .iter()
            .enumerate()
            .map(|(i, tc)| ToolCallDelta {
                index: i,
                id: Some(tc.id.clone()),
                name: Some(tc.name.clone()),
                arguments_delta: tc.arguments.to_string(),
            })
            .collect(),
        tokens: Some(response.tokens_used),
        finish_reason: Some(response.finish_reason),
    }
}

#[async_trait]
impl Provider for RhaiProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        if let Some(rl) = &self.rate_limiter {
            rl.acquire().await?;
        }
        let timeout = self.effective_timeout(request);
        let request_dyn = request_to_dynamic(request);
        let state_dyn = (*self.state).clone();
        let ast = self.ast.clone();

        let dynamic = self
            .dispatch(
                timeout,
                Box::new(move |engine| {
                    let mut scope = Scope::new();
                    engine
                        .call_fn::<Dynamic>(&mut scope, &ast, "inference", (state_dyn, request_dyn))
                        .map_err(map_rhai_err)
                }),
            )
            .await?;

        let response = parse_inference_dynamic(dynamic)?;
        if let Some(rl) = &self.rate_limiter {
            rl.record_tokens(response.tokens_used.total_tokens).await;
        }
        Ok(response)
    }

    async fn infer_stream(
        &self,
        request: &InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        if !self.has_stream {
            // No native `stream` fn: collapse a full inference into one chunk.
            let response = self.infer(request).await?;
            return Ok(Box::pin(tokio_stream::once(Ok(collapse_chunk(response)))));
        }

        if let Some(rl) = &self.rate_limiter {
            rl.acquire().await?;
        }
        let timeout = self.effective_timeout(request);
        let request_dyn = request_to_dynamic(request);
        let state_dyn = (*self.state).clone();
        let ast = self.ast.clone();

        let (job_tx, mut job_rx) = mpsc::unbounded_channel::<BrokerJob>();
        let (chunk_tx, chunk_rx) = mpsc::unbounded_channel::<Result<StreamChunk>>();
        let err_tx = chunk_tx.clone();
        let env_allowlist = self.env_allowlist.clone();

        let mut join: JoinHandle<Result<()>> = tokio::task::spawn_blocking(move || {
            let engine = build_exec_engine(ExecConfig {
                jobs: job_tx,
                timeout_secs: timeout,
                chunk_tx: Some(chunk_tx),
                env_allowlist,
            });
            // "__emit_chunk" is a fixed valid identifier registered above.
            let on_chunk = FnPtr::new("__emit_chunk").expect("__emit_chunk is a valid identifier");
            let mut scope = Scope::new();
            engine
                .call_fn::<Dynamic>(
                    &mut scope,
                    &ast,
                    "stream",
                    (state_dyn, request_dyn, on_chunk),
                )
                .map(|_| ())
                .map_err(map_rhai_err)
        });

        let executor = self.executor.clone();
        let rate_limiter = self.rate_limiter.clone();
        tokio::spawn(async move {
            let outcome = loop {
                tokio::select! {
                    biased;
                    res = &mut join => break res,
                    Some(job) = job_rx.recv() => {
                        serve_job(&executor, &rate_limiter, job).await
                    }
                }
            };
            finalize_stream(outcome, &err_tx);
            // err_tx dropped here → chunk stream ends.
        });

        Ok(Box::pin(
            tokio_stream::wrappers::UnboundedReceiverStream::new(chunk_rx),
        ))
    }

    async fn count_tokens(&self, text: &str, model: &str) -> usize {
        // Fall through to the heuristic on any script/transport error.
        if self.has_count_tokens
            && let Ok(n) = self.script_count_tokens(text, model).await
        {
            return n;
        }
        crate::tokenizer::count_tokens(text, model)
    }

    fn pricing(&self, model: &str) -> Option<crate::ModelPricing> {
        // Config is the only source there is here. A script provider fronts an
        // endpoint whose rates Leviath cannot look up, so without the user's own
        // number the model has no price and every call on it reports unpriced.
        self.capability_overrides
            .get(model)
            .and_then(|o| o.pricing())
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        self.capability_overrides
            .get(model)
            .and_then(|c| c.max_context_tokens)
            .unwrap_or(self.meta.max_context_tokens)
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        // What the script itself declares, before any `[model_capabilities]`
        // entry is merged onto it.
        let base = ModelCapabilities {
            max_context_tokens: self.meta.max_context_tokens,
            max_output_tokens: self.meta.max_output_tokens,
            supports_streaming: true,
            ..Default::default()
        };
        match self.capability_overrides.get(model) {
            Some(o) => o.apply_to(base),
            None => base,
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        if !self.has_list_models {
            return Ok(Vec::new());
        }
        let ast = self.ast.clone();
        let state = (*self.state).clone();
        let provider = self.name.clone();
        let value = self
            .dispatch(
                self.request_timeout_secs,
                Box::new(move |engine| {
                    let mut scope = Scope::new();
                    engine
                        .call_fn::<Dynamic>(&mut scope, &ast, "list_models", (state,))
                        .map_err(map_rhai_err)
                }),
            )
            .await?;
        Ok(parse_models(value, &provider))
    }
}

/// Convert the array returned by a script's `list_models` into [`ModelInfo`]s.
fn parse_models(value: Dynamic, provider: &str) -> Vec<ModelInfo> {
    let json: serde_json::Value = match rhai::serde::from_dynamic(&value) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    json.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("id").and_then(|v| v.as_str())?.to_string();
                    Some(ModelInfo {
                        display_name: m
                            .get("display_name")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        capabilities: ModelCapabilities {
                            max_context_tokens: m
                                .get("max_context_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(8192)
                                as usize,
                            max_output_tokens: m
                                .get("max_output_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(4096)
                                as usize,
                            ..Default::default()
                        },
                        id,
                        provider: provider.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
