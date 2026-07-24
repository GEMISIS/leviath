//! Drop-in LLM providers defined by a Rhai script (issue #101).
//!
//! A `.rhai` script in `~/.leviath/providers/` implements the API-specific
//! *format mapping* (request → HTTP body, response JSON → Leviath types); this
//! Rust [`RhaiProvider`] wraps it with the full [`Provider`]
//! trait and owns the hard runtime concerns — HTTP transport, rate limiting,
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
//! Required: `initialize(config) -> Map` (runs **offline** — no network host
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
    InferenceRequest, InferenceResponse, ModelCapabilities, ModelInfo, Provider, ProviderError,
    RateLimitConfig, Result, StreamChunk, ToolCallDelta,
};
use crate::rate_limit::RateLimiter;

use convert::{map_rhai_err, parse_inference_dynamic};
use engine::{ExecConfig, build_exec_engine, build_init_engine};
use host::{BrokerJob, HostHttpError, HttpExecutor, ReqwestExecutor};

pub use meta::{ProviderMeta, parse_provider_annotations};

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
    capability_overrides: HashMap<String, ModelCapabilities>,
    has_stream: bool,
    has_count_tokens: bool,
    has_list_models: bool,
}

impl RhaiProvider {
    /// Load a provider from a script file, using the production reqwest-backed
    /// HTTP executor. Reads + compiles the script and runs `initialize(config)`
    /// **offline**; any failure (missing file, compile error, `initialize`
    /// throw) is returned as an error so the caller can skip-with-warning.
    pub fn from_script(
        name: String,
        script_path: &Path,
        init_config: serde_json::Value,
        caps: HashMap<String, ModelCapabilities>,
        rate_limit: Option<RateLimitConfig>,
        request_timeout_secs: Option<u64>,
    ) -> Result<Self> {
        let src = std::fs::read_to_string(script_path).map_err(|e| {
            ProviderError::Other(format!(
                "read provider script {}: {e}",
                script_path.display()
            ))
        })?;
        Self::from_source(
            name,
            &src,
            init_config,
            caps,
            rate_limit,
            request_timeout_secs,
            Arc::new(ReqwestExecutor::new()),
        )
    }

    /// Build a provider from in-memory source with an injected [`HttpExecutor`]
    /// (used by tests to avoid real network I/O).
    #[allow(clippy::too_many_arguments)]
    pub fn from_source(
        name: String,
        src: &str,
        init_config: serde_json::Value,
        caps: HashMap<String, ModelCapabilities>,
        rate_limit: Option<RateLimitConfig>,
        request_timeout_secs: Option<u64>,
        executor: Arc<dyn HttpExecutor>,
    ) -> Result<Self> {
        let meta = parse_provider_annotations(src);
        let init_engine = build_init_engine();
        let ast = init_engine
            .compile(src)
            .map_err(|e| ProviderError::Other(format!("compile provider script {name}: {e}")))?;

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
        let mut join: JoinHandle<Result<Dynamic>> = tokio::task::spawn_blocking(move || {
            let engine = build_exec_engine(ExecConfig {
                jobs: job_tx,
                timeout_secs,
                chunk_tx: None,
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
    async fn infer(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        if let Some(rl) = &self.rate_limiter {
            rl.acquire().await?;
        }
        let timeout = self.effective_timeout(&request);
        let request_dyn = request_to_dynamic(&request);
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
        request: InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        if !self.has_stream {
            // No native `stream` fn: collapse a full inference into one chunk.
            let response = self.infer(request).await?;
            return Ok(Box::pin(tokio_stream::once(Ok(collapse_chunk(response)))));
        }

        if let Some(rl) = &self.rate_limiter {
            rl.acquire().await?;
        }
        let timeout = self.effective_timeout(&request);
        let request_dyn = request_to_dynamic(&request);
        let state_dyn = (*self.state).clone();
        let ast = self.ast.clone();

        let (job_tx, mut job_rx) = mpsc::unbounded_channel::<BrokerJob>();
        let (chunk_tx, chunk_rx) = mpsc::unbounded_channel::<Result<StreamChunk>>();
        let err_tx = chunk_tx.clone();

        let mut join: JoinHandle<Result<()>> = tokio::task::spawn_blocking(move || {
            let engine = build_exec_engine(ExecConfig {
                jobs: job_tx,
                timeout_secs: timeout,
                chunk_tx: Some(chunk_tx),
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

    fn max_context_tokens(&self, model: &str) -> usize {
        self.capability_overrides
            .get(model)
            .map(|c| c.max_context_tokens)
            .unwrap_or(self.meta.max_context_tokens)
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        if let Some(c) = self.capability_overrides.get(model) {
            return c.clone();
        }
        ModelCapabilities {
            max_context_tokens: self.meta.max_context_tokens,
            max_output_tokens: self.meta.max_output_tokens,
            supports_streaming: true,
            ..Default::default()
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
