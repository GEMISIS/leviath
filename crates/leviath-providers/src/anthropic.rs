//! Anthropic Claude provider implementation.

use crate::provider::{
    FinishReason, InferenceRequest, InferenceResponse, LimitsSource, ModelCapabilities,
    ModelCapabilityOverride, ModelInfo, Provider, ProviderConfig, ProviderError, Result,
    StreamChunk, TokenUsage, ToolCall, ToolCallDelta,
};
use crate::rate_limit::RateLimiter;
use async_trait::async_trait;
use futures_core::Stream;
use std::collections::HashMap;
use std::pin::Pin;

/// Read the dump directory from the environment and delegate to
/// [`dump_request`]. See that function for the rationale.
fn maybe_dump_request(body: &serde_json::Value) {
    dump_request(
        body,
        std::env::var("LEVIATH_DUMP_REQUEST_DIR").ok().as_deref(),
    );
}

/// The shortest prefix Anthropic will cache, in tokens.
///
/// Below this the API declines to create an entry, so a marker there buys
/// nothing and spends one of the four. Measured in a dumped request: a marker
/// sat on a 269-byte block, a quarter of the budget on a prefix that could
/// never be read back.
///
/// One number for every Anthropic model rather than a per-model table.
/// 1024 is the documented Sonnet and Opus floor and Haiku's is higher, so this
/// is the permissive bound: on Haiku a marker that clears it may still be
/// declined, which costs nothing beyond the slot. A name-keyed table would be
/// the alternative and is the shape of thing that goes stale silently - the
/// Ollama context window was guessed from model names and was wrong by 4x
/// (#475), which is the failure this deliberately avoids.
const MIN_CACHEABLE_TOKENS: usize = 1024;

/// How far back Anthropic looks for a usable cache entry, in content blocks.
///
/// The lookup does not scan the whole request: from a marker it checks a bounded
/// number of preceding content blocks for an entry it can reuse. Anthropic
/// documents this as roughly twenty. So a marker further past the previous
/// request's entry than this never finds it, however byte-identical the prefix
/// is - the request reads nothing and rewrites the conversation at 1.25x.
///
/// This is the fault behind #474, and it is why every earlier attempt in that
/// thread moved the collapse around without removing it: the marker rolled
/// forward with the conversation, and a workload appending a dozen content
/// blocks a turn outran the lookback within a few turns and never got back
/// inside it. Measured by the reporter across a run: reads climbed while the
/// gap stayed at or below 21 blocks and died permanently at 25.
///
/// Kept a little under the documented figure. Being wrong low costs one extra
/// marker; being wrong high costs the entire conversation cache.
const CACHE_LOOKBACK_BLOCKS: usize = 16;

/// How many of the four markers the system blocks may claim.
///
/// One is held back for the messages. When the system prefix has not moved, a
/// marker in the messages covers everything - every system block *and* the
/// conversation ahead of it - so it is the single most valuable position in the
/// request. A fourth system marker can only ever cover less than that.
pub(crate) const MAX_SYSTEM_BREAKPOINTS: usize = 2;

/// Choose which messages carry a `cache_control` breakpoint.
///
/// The positions are anchored to a stride counted from the *start* of the
/// conversation rather than to an offset from its end. That is the whole point:
/// a marker at "20 blocks in" is at the same place next request, so the entry it
/// wrote is looked up exactly where it was left. A marker placed relative to the
/// end moves every turn by however much the turn appended, and once that step
/// exceeds [`CACHE_LOOKBACK_BLOCKS`] the lookup can no longer reach the previous
/// entry - which is an absorbing state, because the step does not shrink.
///
/// Anchoring also keeps the markers continuous as the conversation grows. Going
/// from 45 blocks to 57 leaves the stride multiples at 16 and 32 exactly where
/// they were; crossing 48 adds one at 48 and keeps the earlier two. There is
/// always a marker sitting on an entry that already exists.
///
/// `budget` is what the system blocks did not claim. The last positions are kept
/// when there are more than fit, because they cover the most conversation - and
/// the stride guarantees consecutive kept positions are within the lookback of
/// one another.
fn message_cache_breakpoints(block_counts: &[usize], budget: usize) -> Vec<usize> {
    if budget == 0 {
        return Vec::new();
    }
    let mut positions = Vec::new();
    let mut blocks = 0usize;
    let mut next_anchor = CACHE_LOOKBACK_BLOCKS;
    for (index, count) in block_counts.iter().enumerate() {
        blocks = blocks.saturating_add(*count);
        // The first message whose end passes the next stride multiple owns that
        // anchor. Several multiples can fall inside one large message, which
        // still yields one marker there - the message is indivisible.
        if blocks >= next_anchor {
            positions.push(index);
            while blocks >= next_anchor {
                next_anchor = next_anchor.saturating_add(CACHE_LOOKBACK_BLOCKS);
            }
        }
    }
    // A conversation too short to reach the first anchor still gets one marker,
    // at its end. Size in blocks is what the lookback counts, but it is not what
    // makes content worth caching: two messages carrying a large file read are
    // only two blocks and plenty of tokens. Without this such a conversation
    // goes uncached for its first several turns, which is a regression against
    // simply marking near the end.
    if positions.is_empty() && !block_counts.is_empty() {
        positions.push(block_counts.len() - 1);
    }
    if positions.len() > budget {
        positions.drain(..positions.len() - budget);
    }
    positions
}

/// Choose which system-block indices carry a `cache_control` breakpoint.
///
/// Anthropic caches by *prefix*: a marker stores everything from the start of
/// the request up to and including its block, and a later request reads it back
/// only if every one of those bytes is identical. Two consequences drive
/// everything here.
///
/// **A marker on content that changes is waste.** The entry it creates can never
/// match, and creating it is charged at the 1.25x write rate. So a block whose
/// region declared itself `rewritten`, or that the runtime clears every
/// iteration, is not a candidate - a prefix ending there is invalid by
/// construction. This is the fix for a measured case where the sole marker sat
/// on a six-token status line at the end of the system array, making the whole
/// prompt uncacheable (#474).
///
/// **Several markers cost nothing extra and find the longest match.** The API
/// takes up to four and reads back the longest stored prefix that still
/// matches, so spreading them is free insurance: if the newest block changed,
/// an earlier marker still hits. Spending one marker of four, which is what this
/// did before, threw that away. The *last* candidates are kept rather than an
/// even spread, because the longest prefix is the most valuable and the ones
/// just behind it are the fallbacks that matter - a region of twenty chunks
/// whose tail moves wants markers near the tail, not near the head.
///
/// Volatility comes from the blueprint and the hint from the region's kind, and
/// a candidate needs both to agree: the blueprint knows whether an author
/// rewrites a region, and the runtime knows that it clears a `Temporary` one
/// whatever the blueprint says.
pub(crate) fn system_cache_breakpoints(
    blocks: &[crate::provider::SystemBlock],
    budget: usize,
) -> Vec<usize> {
    if budget == 0 {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    let mut prefix_tokens = 0usize;
    // The deepest index whose *entire* prefix holds still, which is the only
    // kind of marker guaranteed to be readable next turn. Tracked separately
    // because the candidate test below asks only whether a block itself holds
    // still, and a prefix is all-or-nothing: one block that moves invalidates
    // every marker behind it.
    let mut stable_prefix_end: Option<usize> = None;
    let mut prefix_holds = true;
    for (index, block) in blocks.iter().enumerate() {
        prefix_tokens = prefix_tokens.saturating_add(leviath_core::estimate_tokens(&block.text));
        let holds_still = block.volatility != leviath_core::Volatility::Rewritten
            && block.cache_hint != leviath_core::CacheHint::Never;
        if holds_still && prefix_tokens >= MIN_CACHEABLE_TOKENS {
            candidates.push(index);
        }
        // `Grows` counts as movement here even though it only appends: the
        // bytes still differ from last request, so a prefix ending at or
        // behind it cannot match.
        if block.volatility != leviath_core::Volatility::Stable
            || block.cache_hint == leviath_core::CacheHint::Never
        {
            prefix_holds = false;
        }
        if prefix_holds && prefix_tokens >= MIN_CACHEABLE_TOKENS {
            stable_prefix_end = Some(index);
        }
    }

    // The reliable marker first. Without it every marker could sit on content
    // that changes: assembly sorts `stable` ahead of `grows`, the candidate
    // test admits `grows`, and keeping the *last* candidates then picked
    // exactly the regions that move. A measured research-agent block list put
    // both markers on `sources_index` and `raw_findings` - appended to on
    // almost every turn - and left the two genuinely stable blocks unmarked,
    // so each write was invalidated before it could be read.
    let mut chosen = Vec::new();
    if let Some(floor) = stable_prefix_end {
        chosen.push(floor);
    }
    // Then the deepest candidates, which pay on a turn where nothing behind
    // them moved. Anthropic reads back the longest stored prefix that still
    // matches, so these can only add to what the floor already guarantees.
    for &candidate in candidates.iter().rev() {
        if chosen.len() >= budget {
            break;
        }
        if !chosen.contains(&candidate) {
            chosen.push(candidate);
        }
    }
    chosen.sort_unstable();
    chosen
}

/// Always log the serialized request size at debug, and - when `dir` is
/// `Some` - write the full JSON body to `<dir>/anthropic-req-<unix_nanos>.json`.
///
/// Diagnostic for comparing request bodies: it lets us see exactly how a
/// request that stalls (e.g. one after a batch read, with file-tracked content
/// injected into the system prompt) differs from the small requests that
/// succeed - size, structure, and content - without a special build. Enable by
/// setting `LEVIATH_DUMP_REQUEST_DIR`.
fn dump_request(body: &serde_json::Value, dir: Option<&str>) {
    let bytes = serde_json::to_vec(body).map(|v| v.len()).unwrap_or(0);
    tracing::debug!(request_bytes = bytes, "anthropic request body");

    let Some(dir) = dir else { return };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::path::Path::new(dir).join(format!("anthropic-req-{nanos}.json"));
    // The dump is the whole system prompt, transcript and tool results - file
    // contents and `env_var` output included. It was written at the process
    // umask into a directory created the same way, so pointing this at `/tmp` on
    // a shared host handed every local account the conversation.
    let _ = std::fs::create_dir_all(dir);
    let _ = leviath_sys::secure_dir_perms(std::path::Path::new(dir));
    // `body` is an already-built `serde_json::Value`, which is infallibly
    // serializable (no NaN/Inf numbers, keys always strings) - `to_string_pretty`
    // only fails on a custom `Serialize` impl that errors, which a `Value` never
    // has. So there is no reachable error arm; `.expect` documents that.
    let pretty = serde_json::to_string_pretty(body)
        .expect("infallible: a serde_json::Value always serializes");
    if leviath_sys::write_private(&path, pretty.as_bytes()).is_ok() {
        tracing::info!(request_bytes = bytes, path = %path.display(), "dumped anthropic request body");
    }
}

/// Cache TTL for Anthropic prompt caching.
///
/// Settable as `[providers] anthropic_cache_ttl`. The 1-hour option was
/// implemented and unreachable: no config key, no blueprint field, no env var,
/// so every run took the 5-minute default however long its stages ran. Staged
/// agents routinely take longer than five minutes between reuses of the same
/// prefix - a compute stage running scripts is the normal case - so the cache
/// written at the start of a run was usually cold by the time a later stage
/// could have reused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CacheTtl {
    /// 5-minute ephemeral cache (default, no extra cost).
    #[default]
    #[serde(rename = "5m")]
    Ephemeral5m,
    /// 1-hour extended cache. Costs more to write and needs a beta header,
    /// which is sent automatically when this is selected.
    #[serde(rename = "1h")]
    Ephemeral1h,
}

/// Anthropic Claude provider.
pub struct AnthropicProvider {
    /// HTTP client
    client: reqwest::Client,

    /// API key
    api_key: String,

    /// API base URL
    base_url: String,

    /// Rate limiter
    rate_limiter: Option<RateLimiter>,

    /// Per-model capability overrides
    capability_overrides: HashMap<String, ModelCapabilityOverride>,

    /// Cache TTL for prompt caching breakpoints.
    cache_ttl: CacheTtl,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider.
    pub fn new(client: reqwest::Client, api_key: String) -> Self {
        Self {
            client,
            api_key,
            base_url: "https://api.anthropic.com/v1".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            cache_ttl: CacheTtl::default(),
        }
    }

    /// Create a new Anthropic provider with full configuration.
    pub fn with_config(client: reqwest::Client, config: ProviderConfig) -> Self {
        let rate_limiter = config.rate_limit.as_ref().map(RateLimiter::new);
        Self {
            client,
            api_key: config.api_key,
            base_url: config
                .base_url
                .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string()),
            rate_limiter,
            capability_overrides: HashMap::new(),
            cache_ttl: CacheTtl::default(),
        }
    }

    /// Create a new Anthropic provider with per-model capability overrides.
    pub fn with_overrides(
        client: reqwest::Client,
        api_key: String,
        overrides: HashMap<String, ModelCapabilityOverride>,
        rate_limit: Option<&crate::provider::RateLimitConfig>,
    ) -> Self {
        Self {
            client,
            api_key,
            base_url: "https://api.anthropic.com/v1".to_string(),
            rate_limiter: rate_limit.map(crate::rate_limit::RateLimiter::new),
            capability_overrides: overrides,
            cache_ttl: CacheTtl::default(),
        }
    }

    /// Return built-in capabilities for a model based on its name pattern.
    fn builtin_capabilities(&self, model: &str) -> ModelCapabilities {
        // Opus 5 - top-tier, 1M context, 128K output, no temperature
        if model.contains("claude-opus-5") {
            return ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_000_000,
                max_output_tokens: 128_000,
                limits_source: LimitsSource::Builtin,
            };
        }
        // Sonnet 5 - 1M context, 128K output, no temperature
        if model.contains("claude-sonnet-5") {
            return ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_000_000,
                max_output_tokens: 128_000,
                limits_source: LimitsSource::Builtin,
            };
        }
        // Fable 5 / Mythos 5 - top-tier, 1M context, 128K output, no temperature
        if model.contains("claude-fable-5") || model.contains("claude-mythos-5") {
            return ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_000_000,
                max_output_tokens: 128_000,
                limits_source: LimitsSource::Builtin,
            };
        }
        // Opus 4.8 / 4.7 - 1M context, 128K output, no temperature
        if model.contains("claude-opus-4-8") || model.contains("claude-opus-4-7") {
            return ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_000_000,
                max_output_tokens: 128_000,
                limits_source: LimitsSource::Builtin,
            };
        }
        // Opus 4.6 - 1M context, 128K output, temperature supported
        if model.contains("claude-opus-4-6") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_000_000,
                max_output_tokens: 128_000,
                limits_source: LimitsSource::Builtin,
            };
        }
        // Sonnet 4.6 - 1M context, 128K output, temperature supported
        if model.contains("claude-sonnet-4-6") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_000_000,
                max_output_tokens: 128_000,
                limits_source: LimitsSource::Builtin,
            };
        }
        // Haiku 4.5 - 200K context, 64K output, temperature supported
        if model.contains("claude-haiku-4-5") {
            return ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 200_000,
                max_output_tokens: 64_000,
                limits_source: LimitsSource::Builtin,
            };
        }
        // Generic Claude 4.x fallback (e.g. older 4.5 snapshots)
        if model.contains("claude-opus-4")
            || model.contains("claude-sonnet-4")
            || model.contains("claude-haiku-4")
        {
            return ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_000_000,
                max_output_tokens: 32_768,
                limits_source: LimitsSource::Builtin,
            };
        }
        ModelCapabilities::default()
    }

    /// Point this provider at a different host.
    ///
    /// An enterprise gateway or self-hosted proxy speaks the same API on a
    /// different origin, and every part of that was already here - the struct
    /// holds a `base_url`, and `with_config` honours one - except a way for
    /// configuration to reach the constructor the registry actually calls.
    /// `with_config` sets the URL and drops the capability overrides;
    /// `with_overrides` does the reverse, and the registry needs the overrides,
    /// so the URL was the half that got lost.
    ///
    /// A builder rather than a fifth constructor parameter, following
    /// `with_cache_ttl`: one field that four providers each gained does not
    /// need to widen three constructors apiece.
    ///
    /// `None` keeps the built-in default, so a config that says nothing is
    /// byte-for-byte the request it was before.
    pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
        if let Some(url) = base_url {
            self.base_url = url;
        }
        self
    }

    /// Return the `cache_control` JSON value for the configured TTL.
    /// Select the prompt-cache TTL.
    ///
    /// A builder rather than another constructor parameter: this is the only
    /// provider with the setting, and the alternative grows the signature of
    /// all three of its constructors for one field.
    pub fn with_cache_ttl(mut self, ttl: CacheTtl) -> Self {
        self.cache_ttl = ttl;
        self
    }

    fn cache_control_value(&self) -> serde_json::Value {
        match self.cache_ttl {
            CacheTtl::Ephemeral5m => serde_json::json!({ "type": "ephemeral" }),
            CacheTtl::Ephemeral1h => serde_json::json!({ "type": "ephemeral", "ttl": "1h" }),
        }
    }

    /// Apply common Anthropic headers to a request builder.
    /// The headers every Anthropic request carries.
    ///
    /// The one source of truth for the set: [`Self::apply_headers`] folds it
    /// onto a builder, and the shared `send_chat_request` takes it as pairs.
    /// Keeping them separate is how the debug-http log drifted - it hardcoded
    /// three headers and silently omitted `anthropic-beta`, so under
    /// `--features debug-http` a 1h-cache request logged something the wire
    /// never carried.
    fn header_pairs(&self) -> Vec<(&'static str, String)> {
        let mut headers = vec![
            ("x-api-key", self.api_key.clone()),
            ("anthropic-version", "2023-06-01".to_string()),
            ("content-type", "application/json".to_string()),
        ];
        if self.cache_ttl == CacheTtl::Ephemeral1h {
            headers.push((
                "anthropic-beta",
                "extended-cache-ttl-2025-04-11".to_string(),
            ));
        }
        headers
    }

    fn apply_headers(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        self.header_pairs()
            .into_iter()
            .fold(builder, |b, (name, value)| b.header(name, value))
    }

    /// Call Anthropic's exact `/messages/count_tokens` endpoint for `text`.
    ///
    /// Wraps the text as a single user message (the endpoint counts structured
    /// message input). Returns the reported `input_tokens`, or an error the
    /// caller turns into a heuristic fallback. Does not consume the inference
    /// rate limiter - counting is a cheap, best-effort side call.
    async fn count_tokens_remote(&self, text: &str, model: &str) -> Result<usize> {
        let url = format!("{}/messages/count_tokens", self.base_url);
        let body = serde_json::json!({
            "model": model,
            "messages": [{ "role": "user", "content": text }],
        });
        let response = self
            .apply_headers(self.client.post(&url))
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::transport("sending the request", &e))?;
        let response = crate::provider::check_http_response(response, None).await?;
        let value: serde_json::Value = crate::provider::decode_json(response).await?;
        value
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .ok_or_else(|| {
                ProviderError::InvalidResponse("count_tokens missing input_tokens".to_string())
            })
    }

    /// Build the request body for the Anthropic API.
    fn build_request_body(&self, request: &InferenceRequest) -> serde_json::Value {
        // Anthropic allows at most 4 `cache_control` blocks per request, counted
        // across BOTH system blocks and message content. System blocks get first
        // claim on that budget (they're the most stable, most valuable prefix to
        // cache); messages get whatever remains.
        const MAX_BREAKPOINTS: usize = 4;

        // Consolidate system-block cache breakpoints so a blueprint with many
        // pinned/cached regions stays within the limit (issue #12): one
        // `cache_control` per contiguous run of same-hint cacheable blocks,
        // capped at the total budget.
        let system_breakpoints: std::collections::HashSet<usize> =
            system_cache_breakpoints(&request.system, MAX_SYSTEM_BREAKPOINTS)
                .into_iter()
                .collect();

        // Build system blocks from request.system, annotating only the chosen
        // breakpoint indices with cache_control.
        let system_parts: Vec<serde_json::Value> = request
            .system
            .iter()
            .enumerate()
            .map(|(i, block)| {
                let mut obj = serde_json::json!({
                    "type": "text",
                    "text": block.text,
                });
                if system_breakpoints.contains(&i) {
                    obj["cache_control"] = self.cache_control_value();
                }
                obj
            })
            .collect();

        // Build conversation messages
        let mut messages: Vec<serde_json::Value> = Vec::new();

        // Messages get whatever the system blocks did not claim, and this layer
        // decides where they go.
        //
        // The assembler decides *whether* the conversation may be cached at all,
        // because it is the half that knows what changed - it withholds its flag
        // when the system prefix moved, since a marker in the messages sits
        // behind every system block and could not be read back. This layer then
        // decides *where*, because the placement rule is Anthropic's: a stride
        // in content blocks, bounded by how far its lookup will search.
        let messages_may_cache = request.messages.iter().any(|m| m.cache_breakpoint);
        let message_breakpoint_budget = MAX_BREAKPOINTS - system_breakpoints.len();
        let message_breakpoints: std::collections::HashSet<usize> = match messages_may_cache {
            true => {
                let block_counts: Vec<usize> = request
                    .messages
                    .iter()
                    .map(|m| match &m.content {
                        crate::MessageContent::Blocks(blocks) => blocks.len().max(1),
                        crate::MessageContent::Text(_) => 1,
                    })
                    .collect();
                message_cache_breakpoints(&block_counts, message_breakpoint_budget)
                    .into_iter()
                    .collect()
            }
            false => std::collections::HashSet::new(),
        };

        for (index, msg) in request.messages.iter().enumerate() {
            if message_breakpoints.contains(&index) {
                // Wrap content with cache_control
                match &msg.content {
                    crate::MessageContent::Text(text) => {
                        let mut block = serde_json::json!({
                            "type": "text",
                            "text": text,
                        });
                        block["cache_control"] = self.cache_control_value();
                        messages.push(serde_json::json!({
                            "role": msg.role,
                            "content": [block],
                        }));
                    }
                    crate::MessageContent::Blocks(_) => {
                        // The marker goes on the last content block. The API
                        // takes `cache_control` there, and in an agent run
                        // nearly every message is a tool turn - so serializing
                        // these "normally" spent a breakpoint from a budget of
                        // four and wrote nothing, leaving the tail uncached.
                        // The breakpoint is chosen by index
                        // (`assemble_with_meta`), so it lands here most of the
                        // time.
                        let mut message = serde_json::json!({
                            "role": msg.role,
                            "content": msg.content,
                        });
                        // Nothing to mark leaves the message unannotated; it
                        // is still sent.
                        if let Some(last) = message["content"]
                            .as_array_mut()
                            .and_then(|blocks| blocks.last_mut())
                        {
                            last["cache_control"] = self.cache_control_value();
                        }
                        messages.push(message);
                    }
                }
            } else {
                messages.push(serde_json::json!({
                    "role": msg.role,
                    "content": msg.content,
                }));
            }
        }

        let caps = self.capabilities(&request.model);

        let mut body = if caps.supports_temperature {
            serde_json::json!({
                "model": request.model,
                "max_tokens": request.max_tokens,
                "temperature": crate::provider::json_number(request.temperature),
                "messages": messages,
            })
        } else {
            serde_json::json!({
                "model": request.model,
                "max_tokens": request.max_tokens,
                "messages": messages,
            })
        };

        // Add system blocks as top-level field. Use the plain-string form only
        // for a single *uncached* block; anything carrying cache_control must
        // stay in the array form or the annotation would be dropped.
        if system_parts.len() == 1 && system_breakpoints.is_empty() {
            body["system"] = system_parts[0]["text"].clone();
        } else if !system_parts.is_empty() {
            body["system"] = serde_json::Value::Array(system_parts);
        }

        // Add tools if present
        if !request.tools.is_empty() {
            let tools: Vec<serde_json::Value> = request
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect();
            body["tools"] = serde_json::Value::Array(tools);
        }

        // Pass through extra model parameters (top_p, top_k, stop_sequences, …).
        crate::openai_compat::merge_extra_params(
            body.as_object_mut()
                .expect("an Anthropic request body is always a JSON object"),
            &request.extra,
        );
        body
    }

    /// Parse a stop reason string into a FinishReason.
    fn parse_stop_reason(reason: &str) -> FinishReason {
        match reason {
            "end_turn" => FinishReason::Complete,
            "tool_use" => FinishReason::ToolCall,
            "max_tokens" => FinishReason::TokenLimit,
            "stop_sequence" => FinishReason::Stop,
            _ => FinishReason::Complete,
        }
    }

    /// Parse the API response body.
    fn parse_response(&self, body: &serde_json::Value) -> Result<InferenceResponse> {
        let mut content = String::new();
        let mut tool_calls = Vec::new();

        if let Some(content_blocks) = body.get("content").and_then(|c| c.as_array()) {
            for block in content_blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            content.push_str(text);
                        }
                    }
                    Some("tool_use") => {
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let arguments = block
                            .get("input")
                            .cloned()
                            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                        tool_calls.push(ToolCall {
                            id,
                            name,
                            arguments,
                            thought_signature: None,
                        });
                    }
                    _ => {}
                }
            }
        }

        let usage = body.get("usage");
        let prompt_tokens = usage
            .and_then(|u| u.get("input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let completion_tokens = usage
            .and_then(|u| u.get("output_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        let stop_reason = body
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("end_turn");

        let cached_tokens = usage
            .and_then(|u| u.get("cache_read_input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let cache_write_tokens = usage
            .and_then(|u| u.get("cache_creation_input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        Ok(InferenceResponse {
            content,
            tool_calls,
            // `input_tokens` is already exclusive of both cache counts here, so
            // it is the fresh figure `TokenUsage` wants. The old total omitted
            // the cache counts entirely and under-reported every cached call.
            tokens_used: TokenUsage::new(
                prompt_tokens,
                cached_tokens,
                cache_write_tokens,
                completion_tokens,
            ),
            finish_reason: Self::parse_stop_reason(stop_reason),
        })
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling Anthropic API");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let body = self.build_request_body(request);
        maybe_dump_request(&body);
        let url = format!("{}/messages", self.base_url);

        let response = crate::openai_compat::send_chat_request(
            &self.client,
            "anthropic",
            &url,
            &self.header_pairs(),
            &body,
            self.rate_limiter.as_ref(),
            request.request_timeout_secs,
        )
        .await?;

        let response_body: serde_json::Value = crate::provider::decode_json(response).await?;

        let result = self.parse_response(&response_body)?;

        if let Some(limiter) = &self.rate_limiter {
            limiter.record_tokens(result.tokens_used.total_tokens).await;
        }

        Ok(result)
    }

    async fn infer_stream(
        &self,
        request: &InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        tracing::debug!(model = %request.model, "Calling Anthropic API (streaming)");

        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await?;
        }

        let mut body = self.build_request_body(request);
        body["stream"] = serde_json::Value::Bool(true);
        maybe_dump_request(&body);
        let url = format!("{}/messages", self.base_url);

        let response = crate::openai_compat::send_chat_request(
            &self.client,
            "anthropic",
            &url,
            &self.header_pairs(),
            &body,
            self.rate_limiter.as_ref(),
            request.request_timeout_secs,
        )
        .await?;

        let byte_stream = response.bytes_stream();
        let stream = AnthropicSseStream::new(byte_stream);

        Ok(Box::pin(stream))
    }

    async fn count_tokens(&self, text: &str, model: &str) -> usize {
        // Prefer the exact `/messages/count_tokens` endpoint; fall back to the
        // ~3.5 chars/token heuristic on any error (network, non-2xx, parse).
        match self.count_tokens_remote(text, model).await {
            Ok(n) => n,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "Anthropic count_tokens endpoint failed; using heuristic"
                );
                crate::tokenizer::count_tokens(text, model)
            }
        }
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        self.capabilities(model).max_context_tokens
    }

    fn name(&self) -> &str {
        "anthropic"
    }

    fn serves_model(&self, model_key: &str) -> Option<String> {
        // Anthropic's models are named `claude-*`. See the note on the Gemini
        // provider for why the capability table is the wrong thing to ask.
        (model_key.starts_with("claude") || self.capability_overrides.contains_key(model_key))
            .then(|| model_key.to_string())
    }

    fn pricing(&self, model: &str) -> Option<crate::ModelPricing> {
        // Config first: it is the only source that can know a negotiated rate,
        // and the shipped table is a transcription of a public page that may
        // have moved since this build.
        self.capability_overrides
            .get(model)
            .and_then(|o| o.pricing())
            .or_else(|| crate::pricing::published_rates("anthropic", model))
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        // Merged, not swapped: an entry names only what it corrects.
        match self.capability_overrides.get(model) {
            Some(o) => o.apply_to(self.builtin_capabilities(model)),
            None => self.builtin_capabilities(model),
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .client
            .get(format!("{}/models", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(|e| ProviderError::transport("listing models", &e))?;

        // Shared classification here too. `is_transient` treats
        // `RequestFailed` as retryable, so classifying by status is what keeps
        // a revoked key from looking like a flaky network - the one is only
        // fixable by the operator, the other by waiting.
        let response = crate::provider::check_http_response(response, None).await?;

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ProviderError::transport("reaching the provider", &e))?;

        let data = body.get("data").and_then(|d| d.as_array()).ok_or_else(|| {
            ProviderError::RequestFailed("missing 'data' field in /models response".to_string())
        })?;

        let models = data
            .iter()
            .filter_map(|entry| {
                let id = entry.get("id").and_then(|v| v.as_str())?.to_string();
                let display_name = entry
                    .get("display_name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let capabilities = self.capabilities(&id);
                Some(ModelInfo {
                    id,
                    display_name,
                    provider: "anthropic".to_string(),
                    capabilities,
                })
            })
            .collect();

        Ok(models)
    }
}

// SSE stream parser for Anthropic's streaming API.
//
// The inner byte stream is boxed as a trait object rather than kept generic.
// In production this is always `reqwest`'s `bytes_stream()`; tests inject
// dozens of distinct mock stream types via `new`'s generic parameter, and a
// generic `impl<S> Stream` causes `cargo llvm-cov` to instrument each
// monomorphized `poll_next` separately, leaving some artificially "uncovered"
// even though the shared logic is fully exercised. Boxing collapses all of
// that into a single concrete `poll_next` implementation.
struct AnthropicSseStream {
    inner: Pin<Box<dyn Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send>>,
    buffer: String,
    current_tool_index: usize,
}

impl AnthropicSseStream {
    fn new<S>(inner: S) -> Self
    where
        S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    {
        Self {
            inner: Box::pin(inner),
            buffer: String::new(),
            current_tool_index: 0,
        }
    }
}

impl Stream for AnthropicSseStream {
    type Item = Result<StreamChunk>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            // Check if we have complete SSE events in the buffer
            if let Some(chunk) = parse_sse_event(&mut this.buffer, &mut this.current_tool_index) {
                return std::task::Poll::Ready(Some(Ok(chunk)));
            }

            // Try to get more data
            match this.inner.as_mut().poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(bytes))) => {
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        this.buffer.push_str(text);
                    }
                }
                std::task::Poll::Ready(Some(Err(e))) => {
                    return std::task::Poll::Ready(Some(Err(ProviderError::RequestFailed(
                        e.to_string(),
                    ))));
                }
                std::task::Poll::Ready(None) => {
                    // Stream ended - try to parse any remaining data
                    if let Some(chunk) =
                        parse_sse_event(&mut this.buffer, &mut this.current_tool_index)
                    {
                        return std::task::Poll::Ready(Some(Ok(chunk)));
                    }
                    return std::task::Poll::Ready(None);
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

/// Parse a single SSE event from the buffer, consuming it if found.
fn parse_sse_event(buffer: &mut String, tool_index: &mut usize) -> Option<StreamChunk> {
    // `None` until the double newline that terminates an event has arrived;
    // the caller polls again with more bytes.
    let (event_text, rest) = buffer.split_once("\n\n")?;
    let event_text = event_text.to_string();
    *buffer = rest.to_string();

    // Parse event type and data
    let mut event_type = String::new();
    let mut data = String::new();

    for line in event_text.lines() {
        if let Some(et) = line.strip_prefix("event: ") {
            event_type = et.to_string();
        } else if let Some(d) = line.strip_prefix("data: ") {
            data = d.to_string();
        }
    }

    if data.is_empty() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_str(&data).ok()?;

    match event_type.as_str() {
        "content_block_delta" => {
            let delta = json.get("delta")?;
            match delta.get("type").and_then(|t| t.as_str()) {
                Some("text_delta") => {
                    let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    Some(StreamChunk {
                        delta: text.to_string(),
                        tool_calls: Vec::new(),
                        tokens: None,
                        finish_reason: None,
                    })
                }
                Some("input_json_delta") => {
                    let partial = delta
                        .get("partial_json")
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    Some(StreamChunk {
                        delta: String::new(),
                        tool_calls: vec![ToolCallDelta {
                            index: *tool_index,
                            id: None,
                            name: None,
                            arguments_delta: partial.to_string(),
                        }],
                        tokens: None,
                        finish_reason: None,
                    })
                }
                _ => None,
            }
        }
        "content_block_start" => {
            let content_block = json.get("content_block")?;
            if content_block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                let id = content_block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = content_block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let idx = *tool_index;
                *tool_index += 1;
                Some(StreamChunk {
                    delta: String::new(),
                    tool_calls: vec![ToolCallDelta {
                        index: idx,
                        id: Some(id),
                        name: Some(name),
                        arguments_delta: String::new(),
                    }],
                    tokens: None,
                    finish_reason: None,
                })
            } else {
                None
            }
        }
        "message_delta" => {
            let stop_reason = json
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|s| s.as_str())
                .unwrap_or("end_turn");

            let usage = json.get("usage");
            let output_tokens = usage
                .and_then(|u| u.get("output_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;

            Some(StreamChunk {
                delta: String::new(),
                tool_calls: Vec::new(),
                tokens: Some(TokenUsage::new(0, 0, 0, output_tokens)),
                finish_reason: Some(AnthropicProvider::parse_stop_reason(stop_reason)),
            })
        }
        "message_start" => {
            // Extract input token count from message_start
            let usage = json.get("message").and_then(|m| m.get("usage"));
            let input_tokens = usage
                .and_then(|u| u.get("input_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let cached = usage
                .and_then(|u| u.get("cache_read_input_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let cache_write = usage
                .and_then(|u| u.get("cache_creation_input_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;

            if input_tokens > 0 || cached > 0 || cache_write > 0 {
                Some(StreamChunk {
                    delta: String::new(),
                    tool_calls: Vec::new(),
                    tokens: Some(TokenUsage::new(input_tokens, cached, cache_write, 0)),
                    finish_reason: None,
                })
            } else {
                None
            }
        }
        "message_stop" | "ping" => None,
        _ => None,
    }
}

#[cfg(test)]
mod tests {

    /// Config beats the shipped table, and an unconfigured model still gets the
    /// published rate rather than falling to unpriced.
    #[test]
    fn pricing_prefers_config_then_the_published_table() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "claude-opus-5".to_string(),
            crate::ModelCapabilityOverride {
                input_per_mtok: Some(1.0),
                output_per_mtok: Some(2.0),
                ..Default::default()
            },
        );
        let provider = AnthropicProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            overrides,
            None,
        );

        // Configured: the operator's number, not the table's.
        let configured = provider.pricing("claude-opus-5").expect("configured");
        assert_eq!(configured.input_per_mtok, 1.0);
        assert_eq!(configured.output_per_mtok, 2.0);

        // Not configured: the published rate.
        let listed = provider.pricing("claude-sonnet-5").expect("in the table");
        assert_eq!(listed.input_per_mtok, 2.0);

        // Neither: unpriced, so the run reports its cost unavailable.
        assert_eq!(provider.pricing("no-such-model-9"), None);
    }
    use super::*;

    // ─── A breakpoint on a tool turn is not thrown away ─────────────────────

    /// The body this provider would send, with a breakpoint on `flagged`.
    fn body_with_breakpoint_on(content: crate::MessageContent) -> serde_json::Value {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "assistant".to_string(),
                content,
                cache_breakpoint: true,
            }],
            model: "claude-sonnet-5".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        provider.build_request_body(&request)
    }

    /// A marker has to have a prefix that holds still, not merely a block that
    /// does.
    ///
    /// Assembly sorts `stable` ahead of `grows`, the candidate test admitted
    /// `grows`, and keeping the last candidates then chose exactly the regions
    /// that move. On a real research-agent block list both markers landed on
    /// `sources_index` and `raw_findings` - appended to on nearly every turn -
    /// while `query` and `format` went unmarked, so every write was invalid
    /// before it could be read.
    #[test]
    fn a_marker_lands_on_the_prefix_that_holds_still() {
        use leviath_core::{CacheHint, Volatility};
        let block = |name: &str, v: Volatility, h: CacheHint| crate::provider::SystemBlock {
            // ~1250 tokens each, so a single block clears MIN_CACHEABLE_TOKENS
            // and the token floor is never what decides this test.
            text: "x ".repeat(2500),
            cache_hint: h,
            region: name.to_string(),
            volatility: v,
        };
        // The order assembly produces: stable, then grows, then rewritten.
        let blocks = vec![
            block("query", Volatility::Stable, CacheHint::Always),
            block("format", Volatility::Stable, CacheHint::Always),
            block("sources_index", Volatility::Grows, CacheHint::Always),
            block("raw_findings", Volatility::Grows, CacheHint::UntilChanged),
            block("conversation", Volatility::Rewritten, CacheHint::Never),
        ];

        let picked = system_cache_breakpoints(&blocks, MAX_SYSTEM_BREAKPOINTS);
        let names: Vec<&str> = picked.iter().map(|i| blocks[*i].region.as_str()).collect();

        assert!(
            names.contains(&"format"),
            "the deepest all-stable prefix ends at `format`, and something has \
             to mark it or nothing is reliably cacheable: {names:?}"
        );
        assert!(
            !picked.is_empty() && picked.len() <= MAX_SYSTEM_BREAKPOINTS,
            "within budget: {names:?}"
        );
        // The remaining marker may go deeper - a turn that appended nothing
        // still reads it back - but never at the cost of the reliable one.
        assert!(
            !names.contains(&"conversation"),
            "a block cleared every iteration is never a marker: {names:?}"
        );
    }

    #[test]
    fn a_breakpoint_on_a_tool_turn_reaches_the_wire() {
        // In an agent run nearly every message is a tool turn, and the
        // breakpoint is chosen by index, so this is where it usually lands.
        // It used to decrement a budget of four and write nothing.
        let content = crate::MessageContent::Blocks(vec![
            crate::provider::ContentBlock::Text {
                text: "thinking".to_string(),
            },
            crate::provider::ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": "x"}),
                thought_signature: None,
            },
        ]);
        let body = body_with_breakpoint_on(content);
        let blocks = body["messages"][0]["content"]
            .as_array()
            .expect("blocks serialize as an array");
        assert!(
            blocks
                .last()
                .is_some_and(|b| b.get("cache_control").is_some()),
            "the marker belongs on the last block: {blocks:?}"
        );
        // And on exactly one block, or the budget is spent several times over.
        let marked = blocks
            .iter()
            .filter(|b| b.get("cache_control").is_some())
            .count();
        assert_eq!(marked, 1, "{blocks:?}");
    }

    #[test]
    fn a_breakpoint_on_a_text_turn_still_reaches_the_wire() {
        let body = body_with_breakpoint_on(crate::MessageContent::Text("hi".to_string()));
        assert!(
            body["messages"][0]["content"][0]
                .get("cache_control")
                .is_some(),
            "{body}"
        );
    }

    /// A flagged message with no blocks at all hands its budget back rather
    /// than spending it on nothing.
    #[test]
    fn an_empty_block_list_does_not_spend_the_budget() {
        let body = body_with_breakpoint_on(crate::MessageContent::Blocks(vec![]));
        let blocks = body["messages"][0]["content"].as_array().expect("array");
        assert!(blocks.is_empty());
    }

    // ─── The 1-hour TTL is reachable ────────────────────────────────────────

    #[test]
    fn the_default_ttl_is_five_minutes_and_sends_no_beta_header() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
        );
        assert_eq!(
            provider.cache_control_value(),
            serde_json::json!({"type": "ephemeral"})
        );
        assert!(
            !provider
                .header_pairs()
                .iter()
                .any(|(_, v)| v.contains("extended-cache-ttl")),
            "the beta header is for the extended TTL only"
        );
    }

    #[test]
    fn selecting_the_hour_ttl_marks_it_and_sends_the_beta_header() {
        // Implemented but unreachable before this: no config key, no blueprint
        // field, no env var, so every run took the five-minute default.
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
        )
        .with_cache_ttl(CacheTtl::Ephemeral1h);
        assert_eq!(
            provider.cache_control_value(),
            serde_json::json!({"type": "ephemeral", "ttl": "1h"})
        );
        assert!(
            provider
                .header_pairs()
                .iter()
                .any(|(_, v)| v.contains("extended-cache-ttl")),
            "the extended TTL needs its beta header or the API ignores it"
        );
    }

    #[test]
    fn the_ttl_spellings_are_the_ones_the_config_accepts() {
        assert_eq!(
            serde_json::from_str::<CacheTtl>("\"1h\"").unwrap(),
            CacheTtl::Ephemeral1h
        );
        assert_eq!(
            serde_json::from_str::<CacheTtl>("\"5m\"").unwrap(),
            CacheTtl::Ephemeral5m
        );
        assert!(serde_json::from_str::<CacheTtl>("\"2h\"").is_err());
    }
    use crate::test_support::always_on_tracing_guard;
    use leviath_testkit::{
        spawn_mock_server,
        spawn_mock_server_truncated_body as spawn_mock_server_truncated_error_body,
        spawn_mock_server_with_headers,
    };

    #[test]
    fn test_provider_creation() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        assert_eq!(provider.name(), "anthropic");
    }

    #[test]
    fn dump_request_none_is_noop() {
        // No dir → only the debug size log runs; must not panic or write.
        dump_request(&serde_json::json!({"model": "x", "messages": []}), None);
    }

    #[test]
    fn dump_request_writes_body_when_dir_set() {
        // Active subscriber so the `info!` on the successful-write path fully
        // evaluates its argument expressions (otherwise the disabled macro
        // short-circuits and llvm-cov reads that line's arg region as uncovered).
        let _guard = always_on_tracing_guard();
        let dir = std::env::temp_dir().join(format!(
            "leviath-dumptest-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let body = serde_json::json!({"model": "claude-sonnet-5", "marker": "unique-body-42"});
        dump_request(&body, Some(dir.to_str().unwrap()));

        let files: Vec<_> = std::fs::read_dir(&dir)
            .expect("dump dir should exist")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(files.len(), 1, "exactly one dump file expected");
        let contents = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(
            contents.contains("unique-body-42"),
            "dump must contain the body"
        );
        assert!(contents.contains("claude-sonnet-5"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn maybe_dump_request_reads_env_without_panicking() {
        // Exercises the env-reading wrapper (dir unset in the test env → noop).
        maybe_dump_request(&serde_json::json!({"model": "x", "messages": []}));
    }

    #[test]
    fn test_context_limits() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        assert_eq!(provider.max_context_tokens("claude-sonnet-4-6"), 1_000_000);
    }

    #[test]
    fn test_build_request_body() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![crate::SystemBlock {
                text: "You are helpful. ".repeat(400),
                cache_hint: leviath_core::CacheHint::Always,
                volatility: leviath_core::Volatility::Stable,
                region: String::new(),
            }],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        assert_eq!(body["model"], "claude-sonnet-4-6");
        // A single *cacheable* system block is emitted in array form so its
        // cache_control annotation survives (issue #12 fix); the plain-string
        // form is reserved for a single uncached block.
        let system = body["system"]
            .as_array()
            .expect("cached system → array form");
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["text"], "You are helpful. ".repeat(400));
        assert!(system[0].get("cache_control").is_some());
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_build_request_body_passes_through_extra_params() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::json!({ "top_p": 0.9, "top_k": 40 }),
            request_timeout_secs: None,
        };
        let body = provider.build_request_body(&request);
        assert_eq!(body["top_p"], serde_json::json!(0.9));
        assert_eq!(body["top_k"], serde_json::json!(40));
    }

    #[test]
    fn test_parse_response() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let body = serde_json::json!({
            "content": [
                { "type": "text", "text": "Hello!" }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5
            }
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.content, "Hello!");
        assert_eq!(response.tokens_used.prompt_tokens, 10);
        assert_eq!(response.tokens_used.completion_tokens, 5);
        assert_eq!(response.finish_reason, FinishReason::Complete);
    }

    /// The same call, billed the same way, whichever provider reported it.
    ///
    /// This is the property that was broken and that no test held: Anthropic
    /// reports its three input counts separately while the OpenAI shape folds
    /// two of them into `prompt_tokens`, so identical usage produced different
    /// `TokenUsage` and any cost arithmetic was wrong for one of them. 100
    /// fresh + 80 cached + 4 written must read the same from both.
    #[test]
    fn the_two_provider_shapes_agree_on_identical_usage() {
        let anthropic = serde_json::json!({
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 25,
                "cache_read_input_tokens": 80,
                "cache_creation_input_tokens": 4
            }
        });
        let openai = serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }],
            "usage": {
                // Same call: the OpenAI shape reports the 184 total inclusive
                // of the 80 cached and 4 written.
                "prompt_tokens": 184,
                "completion_tokens": 25,
                "prompt_tokens_details": {"cached_tokens": 80, "cache_write_tokens": 4}
            }
        });

        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let a = provider
            .parse_response(&anthropic)
            .expect("anthropic parses")
            .tokens_used;
        let o = crate::openai_compat::parse_openai_response(&openai)
            .expect("openai parses")
            .tokens_used;

        assert_eq!(a.prompt_tokens, o.prompt_tokens, "fresh input");
        assert_eq!(a.cached_tokens, o.cached_tokens, "cache reads");
        assert_eq!(a.cache_write_tokens, o.cache_write_tokens, "cache writes");
        assert_eq!(a.total_tokens, o.total_tokens, "total");
        assert_eq!(a.prompt_tokens, 100);
        assert_eq!(a.input_tokens(), 184);
        assert_eq!(a.total_tokens, 209);
    }

    /// One provider's private field must not ride shared history into another
    /// provider's request.
    ///
    /// Gemini attaches a `thought_signature` to its tool calls. That call goes
    /// into the conversation, and with per-stage models the next stage may run
    /// on Anthropic - which rejects the unknown key outright:
    ///
    ///     tool_use.thought_signature: Extra inputs are not permitted
    ///
    /// Observed as a Gemini stage followed by an Anthropic one dying on its
    /// first request, with the failing model not the one under test (#575).
    #[test]
    fn a_foreign_thought_signature_never_reaches_anthropic() {
        // As a Gemini turn left it in history.
        let body = body_with_breakpoint_on(crate::MessageContent::Blocks(vec![
            crate::provider::ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": "a.md"}),
                thought_signature: Some("gemini-sig".to_string()),
            },
        ]));
        let json = serde_json::to_string(&body).expect("serializes");
        assert!(
            !json.contains("thought_signature"),
            "Anthropic rejects the key outright; it must not be in the body: {json}"
        );
        assert!(json.contains("call_1"), "the tool call itself still goes");
    }

    #[test]
    fn test_parse_response_with_tool_calls() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let body = serde_json::json!({
            "content": [
                { "type": "text", "text": "Let me search." },
                {
                    "type": "tool_use",
                    "id": "toolu_123",
                    "name": "search",
                    "input": { "query": "rust" }
                }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 20, "output_tokens": 15 }
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.content, "Let me search.");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "search");
        assert_eq!(response.finish_reason, FinishReason::ToolCall);
    }

    #[test]
    fn test_builtin_capabilities_opus48() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("claude-opus-4-8");
        assert!(!caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_opus5() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("claude-opus-5");
        assert!(!caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_sonnet46() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("claude-sonnet-4-6");
        assert!(caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_sonnet5() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("claude-sonnet-5");
        assert!(!caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_fable_and_opus46() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        for m in ["claude-fable-5", "claude-mythos-5", "claude-opus-4-6"] {
            let caps = provider.builtin_capabilities(m);
            assert!(caps.supports_streaming);
            assert!(caps.max_context_tokens >= 200_000);
        }
    }

    #[test]
    fn test_builtin_capabilities_haiku45() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("claude-haiku-4-5-20251001");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 200_000);
        assert_eq!(caps.max_output_tokens, 64_000);
    }

    #[test]
    fn test_builtin_capabilities_fable5() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("claude-fable-5");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_sonnet_46() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("claude-sonnet-4-6");
        assert!(caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_capability_overrides() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "claude-sonnet-4-6".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 1_000_000,
                max_output_tokens: 32_768,
                limits_source: LimitsSource::Builtin,
            }
            .into(),
        );
        let provider = AnthropicProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
            overrides,
            None,
        );
        let caps = provider.capabilities("claude-sonnet-4-6");
        // Override should take precedence over built-in
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_output_tokens, 32_768);
    }

    #[test]
    fn test_build_request_body_with_cache_breakpoint() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![crate::SystemBlock {
                text: "You are helpful. ".repeat(400),
                cache_hint: leviath_core::CacheHint::Always,
                volatility: leviath_core::Volatility::Stable,
                region: String::new(),
            }],
            messages: vec![
                crate::provider::Message {
                    role: "user".to_string(),
                    content: "Hello".into(),
                    cache_breakpoint: true,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: "World".into(),
                    cache_breakpoint: false,
                },
            ],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let messages = body["messages"].as_array().unwrap();

        // Exactly one message carries the marker, and it serializes in array
        // form so the annotation survives. Which message is the provider's
        // choice - the assembler's flag is permission, not a position - so this
        // asserts the serialization rather than the placement, which
        // `message_cache_breakpoints` covers on its own.
        let marked: Vec<&serde_json::Value> = messages
            .iter()
            .filter(|m| m["content"].is_array())
            .collect();
        assert_eq!(marked.len(), 1);
        assert_eq!(
            marked[0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        // Every other message stays in the plain string form.
        assert_eq!(
            messages.iter().filter(|m| m["content"].is_string()).count(),
            messages.len() - 1
        );
    }

    #[test]
    fn test_build_request_body_max_4_breakpoints() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let mut messages: Vec<crate::provider::Message> = (0..6)
            .map(|i| crate::provider::Message {
                role: "user".to_string(),
                content: format!("Message {}", i).into(),
                cache_breakpoint: true,
            })
            .collect();
        // Add a system message
        messages.insert(
            0,
            crate::provider::Message {
                role: "system".to_string(),
                content: "System".into(),
                cache_breakpoint: false,
            },
        );

        let request = InferenceRequest {
            system: vec![],
            messages,
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let msgs = body["messages"].as_array().unwrap();

        // Count messages that have content blocks with cache_control
        let bp_count = msgs
            .iter()
            .filter(|m| {
                m.get("content")
                    .and_then(|c| c.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|block| block.get("cache_control"))
                    .is_some()
            })
            .count();
        // Placement belongs to the provider now, so the exact count is whatever
        // its rule yields. What must hold is the hard API limit, and that a
        // permitted conversation is served at all.
        assert!(bp_count >= 1, "a permitted conversation carries a marker");
        assert!(bp_count <= 4, "never more than the API accepts: {bp_count}");
    }

    // ── issue #12: system-block cache_control budget ──────────────────────────

    /// Blocks carrying only the hints under test, all eligible - these tests
    /// are about how runs are grouped, not about what held still.
    /// Blocks that hold still, carrying only the hints under test. Volatility
    /// defaults to `Rewritten`, which is never a marker candidate, so a fixture
    /// about *placement* has to say otherwise.
    fn blocks_of(hints: &[leviath_core::CacheHint]) -> Vec<crate::provider::SystemBlock> {
        hints
            .iter()
            .map(|h| crate::provider::SystemBlock {
                // Each block clears `MIN_CACHEABLE_TOKENS` on its own, so these
                // tests exercise how runs are grouped rather than the separate
                // rule that refuses a breakpoint on an uncacheably short prefix.
                text: "word ".repeat(1200),
                cache_hint: *h,
                volatility: leviath_core::Volatility::Stable,
                region: String::new(),
            })
            .collect()
    }

    /// Every block that holds still is a candidate, and the budget is spent
    /// rather than hoarded: the API reads back the longest stored prefix that
    /// still matches, so extra markers are free insurance for the case where the
    /// newest block moved.

    #[test]
    fn markers_spread_across_the_blocks_that_hold_still() {
        use leviath_core::CacheHint;
        assert_eq!(
            system_cache_breakpoints(&blocks_of(&[CacheHint::Always; 5]), 3),
            vec![2, 3, 4],
            "the last three, so the longest prefix is covered and two fall back"
        );
    }

    /// The last candidates, not an even spread. A region of many chunks whose
    /// tail moves wants its markers near the tail: an early marker matches a
    /// prefix so short it saves almost nothing.
    #[test]
    fn the_budget_keeps_the_longest_prefixes() {
        use leviath_core::CacheHint;
        assert_eq!(
            system_cache_breakpoints(&blocks_of(&[CacheHint::Always; 8]), 2),
            vec![6, 7]
        );
    }

    /// A block that changes every turn can never be the end of a readable
    /// prefix, so a marker there is a 1.25x write for an entry nothing reads.
    /// This is the measured case from #474: the sole marker sat past the churn.
    #[test]
    fn a_rewritten_block_is_never_a_candidate() {
        let mut blocks = blocks_of(&[leviath_core::CacheHint::Always; 3]);
        blocks[2].volatility = leviath_core::Volatility::Rewritten;
        assert_eq!(
            system_cache_breakpoints(&blocks, 3),
            vec![0, 1],
            "the markers stop in front of the churn"
        );
    }

    /// The runtime's knowledge wins where it has some: it clears a `Temporary`
    /// region every iteration whatever the blueprint declared.
    #[test]
    fn a_never_hinted_block_is_never_a_candidate() {
        use leviath_core::CacheHint;
        let hints = [CacheHint::Always, CacheHint::Never, CacheHint::Always];
        assert_eq!(
            system_cache_breakpoints(&blocks_of(&hints), 3),
            vec![0, 2],
            "the uncacheable block is skipped, the ones around it are not"
        );
    }

    /// Nothing holds still, so there is nothing worth marking.
    #[test]
    fn system_cache_breakpoints_all_never_is_empty() {
        use leviath_core::CacheHint;
        assert!(system_cache_breakpoints(&blocks_of(&[CacheHint::Never; 3]), 3).is_empty());
    }

    /// A prefix below the provider's floor cannot be stored, so a marker on it
    /// would spend a slot on nothing.
    #[test]
    fn a_prefix_below_the_cacheable_floor_gets_no_marker() {
        let mut blocks = blocks_of(&[leviath_core::CacheHint::Always; 3]);
        for block in &mut blocks {
            block.text = "tiny".to_string();
        }
        assert!(system_cache_breakpoints(&blocks, 3).is_empty());
    }

    #[test]
    fn system_cache_breakpoints_zero_budget_is_empty() {
        use leviath_core::CacheHint;
        assert!(system_cache_breakpoints(&blocks_of(&[CacheHint::Always]), 0).is_empty());
    }

    /// Total `cache_control` annotations across system blocks + message content.
    fn count_cache_control(body: &serde_json::Value) -> usize {
        let mut n = 0;
        if let Some(arr) = body.get("system").and_then(|s| s.as_array()) {
            n += arr
                .iter()
                .filter(|b| b.get("cache_control").is_some())
                .count();
        }
        if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
            for m in msgs {
                if let Some(blocks) = m.get("content").and_then(|c| c.as_array()) {
                    n += blocks
                        .iter()
                        .filter(|b| b.get("cache_control").is_some())
                        .count();
                }
            }
        }
        n
    }

    #[test]
    fn build_request_body_caps_total_cache_control_at_4_with_many_system_regions() {
        use leviath_core::CacheHint;
        // issue #12 regression: 5 cacheable system regions (architecture/
        // program_flows/plan/task pinned + files hashmap) must consolidate
        // within the 4-block total `cache_control` budget; emitting 5
        // `cache_control` blocks is a hard Anthropic 400.
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let system = vec![
            crate::SystemBlock {
                text: "architecture ".repeat(400),
                cache_hint: CacheHint::Always,
                volatility: leviath_core::Volatility::Stable,
                region: String::new(),
            },
            crate::SystemBlock {
                text: "program_flows ".repeat(400),
                cache_hint: CacheHint::Always,
                volatility: leviath_core::Volatility::Stable,
                region: String::new(),
            },
            crate::SystemBlock {
                text: "plan ".repeat(400),
                cache_hint: CacheHint::Always,
                volatility: leviath_core::Volatility::Stable,
                region: String::new(),
            },
            crate::SystemBlock {
                text: "task ".repeat(400),
                cache_hint: CacheHint::Always,
                volatility: leviath_core::Volatility::Stable,
                region: String::new(),
            },
            crate::SystemBlock {
                text: "files ".repeat(400),
                cache_hint: CacheHint::UntilChanged,
                volatility: leviath_core::Volatility::Stable,
                region: String::new(),
            },
        ];
        let request = InferenceRequest {
            system,
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "go".into(),
                cache_breakpoint: true,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let total = count_cache_control(&body);
        assert!(
            total <= 4,
            "must stay within Anthropic's 4-block cache_control limit; got {total}"
        );
        // The system claims its share and no more, leaving one of the four for
        // the messages - where, when the system prefix has not moved, a marker
        // covers everything ahead of it and is worth more than a fourth system
        // marker could be.
        let sys_bp = body["system"]
            .as_array()
            .expect("system must be array form")
            .iter()
            .filter(|b| b.get("cache_control").is_some())
            .count();
        assert_eq!(sys_bp, MAX_SYSTEM_BREAKPOINTS);
    }

    #[test]
    fn build_request_body_system_blocks_take_priority_over_message_breakpoints() {
        use leviath_core::CacheHint;
        // 3 distinct system tiers claim 3 of the 4 breakpoints; the lone message
        // that requests one gets the last slot - and never more than 4 total.
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let system = vec![
            crate::SystemBlock {
                text: "alpha ".repeat(900),
                cache_hint: CacheHint::Always,
                volatility: leviath_core::Volatility::Stable,
                region: String::new(),
            },
            crate::SystemBlock {
                text: "bravo ".repeat(900),
                cache_hint: CacheHint::UntilChanged,
                volatility: leviath_core::Volatility::Stable,
                region: String::new(),
            },
            crate::SystemBlock {
                text: "charlie ".repeat(900),
                cache_hint: CacheHint::Always,
                volatility: leviath_core::Volatility::Stable,
                region: String::new(),
            },
        ];
        let messages: Vec<crate::provider::Message> = (0..3)
            .map(|i| crate::provider::Message {
                role: "user".to_string(),
                content: format!("m{i}").into(),
                cache_breakpoint: true,
            })
            .collect();
        let request = InferenceRequest {
            system,
            messages,
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        assert!(count_cache_control(&body) <= 4);
        let sys_bp = body["system"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b.get("cache_control").is_some())
            .count();
        // Two rather than three: the messages need a pair to anchor a marker
        // where the previous request left one, which is what keeps a growing
        // conversation readable at all.
        assert_eq!(
            sys_bp, MAX_SYSTEM_BREAKPOINTS,
            "the system takes its share and leaves the rest for the messages"
        );
        let msg_bp = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| {
                m.get("content")
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                    .and_then(|b| b.get("cache_control"))
                    .is_some()
            })
            .count();
        assert_eq!(msg_bp, 1, "messages get the single remaining breakpoint");
    }

    #[test]
    fn build_request_body_single_uncached_system_block_uses_string_form() {
        use leviath_core::CacheHint;
        // A lone Never block carries no cache_control, so the compact plain-string
        // system form is still used (back-compat).
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![crate::SystemBlock {
                text: "ephemeral".into(),
                cache_hint: CacheHint::Never,
                volatility: leviath_core::Volatility::Stable,
                region: String::new(),
            }],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "hi".into(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        let body = provider.build_request_body(&request);
        assert_eq!(body["system"], "ephemeral");
    }

    #[test]
    fn test_parse_response_with_cache_metrics() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let body = serde_json::json!({
            "content": [
                { "type": "text", "text": "Hello!" }
            ],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_read_input_tokens": 80,
                "cache_creation_input_tokens": 10
            }
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.tokens_used.prompt_tokens, 100);
        assert_eq!(response.tokens_used.cached_tokens, 80);
        assert_eq!(response.tokens_used.cache_write_tokens, 10);
    }

    #[test]
    fn test_token_usage_defaults_cache_fields_to_zero() {
        let usage = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_tokens: 0,
            cache_write_tokens: 0,
            reported_cost_usd: None,
        };
        assert_eq!(usage.cached_tokens, 0);
        assert_eq!(usage.cache_write_tokens, 0);
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    /// Build a provider whose count-token endpoint is unreachable, so
    /// `count_tokens` deterministically falls back to the local heuristic
    /// without any real network call.
    fn heuristic_only_provider() -> AnthropicProvider {
        AnthropicProvider {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            base_url: "http://127.0.0.1:19997".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            cache_ttl: CacheTtl::default(),
        }
    }

    #[tokio::test]
    async fn test_count_tokens_basic() {
        let provider = heuristic_only_provider();
        let tokens = provider
            .count_tokens("Hello, world!", "claude-sonnet-4-6")
            .await;
        assert!(tokens > 0);
        // ~3.5 chars per token → 13 chars ≈ 3-4 tokens
        assert!(tokens < 10);
    }

    #[tokio::test]
    async fn test_count_tokens_empty() {
        let provider = heuristic_only_provider();
        let tokens = provider.count_tokens("", "claude-sonnet-4-6").await;
        assert_eq!(tokens, 0);
    }

    #[tokio::test]
    async fn test_count_tokens_long_string() {
        let provider = heuristic_only_provider();
        let text = "a".repeat(3500);
        let tokens = provider.count_tokens(&text, "claude-sonnet-4-6").await;
        assert_eq!(tokens, 1000); // heuristic fallback: 3500 / 3.5 = 1000
    }

    #[tokio::test]
    async fn test_count_tokens_uses_exact_endpoint() {
        // The endpoint's `input_tokens` wins over the heuristic when reachable.
        let url = spawn_mock_server(200, "OK", br#"{"input_tokens": 42}"#).await;
        let provider = AnthropicProvider {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            base_url: url,
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            cache_ttl: CacheTtl::default(),
        };
        let tokens = provider.count_tokens("anything", "claude-sonnet-4-6").await;
        assert_eq!(tokens, 42);
    }

    #[tokio::test]
    async fn test_count_tokens_falls_back_on_error_status() {
        // A 500 from the endpoint → heuristic fallback (7 chars / 3.5 = 2).
        let url = spawn_mock_server(500, "Internal Server Error", b"boom").await;
        let provider = AnthropicProvider {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            base_url: url,
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            cache_ttl: CacheTtl::default(),
        };
        let tokens = provider.count_tokens("1234567", "claude-sonnet-4-6").await;
        assert_eq!(tokens, 2);
    }

    #[tokio::test]
    async fn test_count_tokens_falls_back_on_malformed_json() {
        // 200 but a non-JSON body → InvalidResponse on parse → heuristic (7/3.5 = 2).
        let url = spawn_mock_server(200, "OK", b"not json").await;
        let provider = AnthropicProvider {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            base_url: url,
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            cache_ttl: CacheTtl::default(),
        };
        let tokens = provider.count_tokens("1234567", "claude-sonnet-4-6").await;
        assert_eq!(tokens, 2);
    }

    #[tokio::test]
    async fn test_count_tokens_falls_back_on_missing_field() {
        // 200 but no `input_tokens` → heuristic fallback (7 chars / 3.5 = 2).
        let url = spawn_mock_server(200, "OK", br#"{"unexpected": true}"#).await;
        let provider = AnthropicProvider {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            base_url: url,
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            cache_ttl: CacheTtl::default(),
        };
        let tokens = provider.count_tokens("1234567", "claude-sonnet-4-6").await;
        assert_eq!(tokens, 2);
    }

    #[test]
    fn test_name() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        assert_eq!(provider.name(), "anthropic");
    }

    #[test]
    fn test_with_config_default_base_url() {
        let config = ProviderConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            rate_limit: None,
            request_timeout_secs: None,
        };
        let provider = AnthropicProvider::with_config(
            crate::provider::build_http_client(None).expect("a test client builds"),
            config,
        );
        assert_eq!(provider.base_url, "https://api.anthropic.com/v1");
    }

    #[test]
    fn test_with_config_custom_base_url() {
        let config = ProviderConfig {
            api_key: "test-key".to_string(),
            base_url: Some("https://custom.api.com".to_string()),
            rate_limit: None,
            request_timeout_secs: None,
        };
        let provider = AnthropicProvider::with_config(
            crate::provider::build_http_client(None).expect("a test client builds"),
            config,
        );
        assert_eq!(provider.base_url, "https://custom.api.com");
    }

    #[test]
    fn test_with_config_with_rate_limit() {
        let config = ProviderConfig {
            api_key: "test-key".to_string(),
            base_url: None,
            rate_limit: Some(crate::provider::RateLimitConfig {
                requests_per_minute: 10,
                tokens_per_minute: 50000,
            }),
            request_timeout_secs: None,
        };
        let provider = AnthropicProvider::with_config(
            crate::provider::build_http_client(None).expect("a test client builds"),
            config,
        );
        assert!(provider.rate_limiter.is_some());
    }

    #[test]
    fn test_builtin_capabilities_opus46() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("claude-opus-4-6");
        assert!(caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert!(caps.supports_system_prompt);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_opus47() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("claude-opus-4-7");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_mythos5() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("claude-mythos-5");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 128_000);
    }

    #[test]
    fn test_builtin_capabilities_generic_claude4_fallback() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        // Uses generic claude-4.x fallback (not matching specific model patterns above)
        let caps = provider.builtin_capabilities("claude-haiku-4");
        assert!(!caps.supports_temperature);
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 32_768);
    }

    #[test]
    fn test_builtin_capabilities_unknown_model() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "test-key".to_string(),
        );
        let caps = provider.builtin_capabilities("some-unknown-model");
        // Should return ModelCapabilities::default()
        let default = ModelCapabilities::default();
        assert_eq!(caps.max_context_tokens, default.max_context_tokens);
    }

    #[test]
    fn test_capabilities_uses_override_when_present() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "custom-model".to_string(),
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 42,
                max_output_tokens: 10,
                limits_source: LimitsSource::Builtin,
            }
            .into(),
        );
        let provider = AnthropicProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
            overrides,
            None,
        );
        let caps = provider.capabilities("custom-model");
        assert_eq!(caps.max_context_tokens, 42);
        assert!(!caps.supports_streaming);
    }

    #[test]
    fn test_capabilities_falls_through_to_builtin() {
        let provider = AnthropicProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
            HashMap::new(),
            None,
        );
        let caps = provider.capabilities("claude-sonnet-4-6");
        assert_eq!(caps.max_context_tokens, 1_000_000);
    }

    #[test]
    fn test_max_context_tokens_delegates_to_capabilities() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        assert_eq!(provider.max_context_tokens("claude-haiku-4-5"), 200_000);
        assert_eq!(provider.max_context_tokens("claude-opus-4-8"), 1_000_000);
    }

    #[test]
    fn test_parse_stop_reason_all_variants() {
        assert_eq!(
            AnthropicProvider::parse_stop_reason("end_turn"),
            FinishReason::Complete
        );
        assert_eq!(
            AnthropicProvider::parse_stop_reason("tool_use"),
            FinishReason::ToolCall
        );
        assert_eq!(
            AnthropicProvider::parse_stop_reason("max_tokens"),
            FinishReason::TokenLimit
        );
        assert_eq!(
            AnthropicProvider::parse_stop_reason("stop_sequence"),
            FinishReason::Stop
        );
        assert_eq!(
            AnthropicProvider::parse_stop_reason("unknown_reason"),
            FinishReason::Complete
        );
    }

    #[test]
    fn test_parse_response_empty_content_blocks() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let body = serde_json::json!({
            "content": [],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 5, "output_tokens": 0 }
        });
        let resp = provider.parse_response(&body).unwrap();
        assert_eq!(resp.content, "");
        assert!(resp.tool_calls.is_empty());
    }

    #[test]
    fn test_parse_response_no_content_field() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let body = serde_json::json!({
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 5, "output_tokens": 0 }
        });
        let resp = provider.parse_response(&body).unwrap();
        assert_eq!(resp.content, "");
    }

    #[test]
    fn test_parse_response_no_usage_field() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let body = serde_json::json!({
            "content": [{ "type": "text", "text": "Hello" }],
            "stop_reason": "end_turn"
        });
        let resp = provider.parse_response(&body).unwrap();
        assert_eq!(resp.tokens_used.prompt_tokens, 0);
        assert_eq!(resp.tokens_used.completion_tokens, 0);
    }

    #[test]
    fn test_parse_response_unknown_content_type_ignored() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let body = serde_json::json!({
            "content": [
                { "type": "image", "data": "abc" },
                { "type": "text", "text": "Hello" }
            ],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 10, "output_tokens": 5 }
        });
        let resp = provider.parse_response(&body).unwrap();
        assert_eq!(resp.content, "Hello");
    }

    #[test]
    fn test_parse_response_tool_call_missing_fields() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let body = serde_json::json!({
            "content": [
                { "type": "tool_use" }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 10, "output_tokens": 5 }
        });
        let resp = provider.parse_response(&body).unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "");
        assert_eq!(resp.tool_calls[0].name, "");
    }

    #[test]
    fn test_parse_response_total_tokens_computed() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let body = serde_json::json!({
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 100, "output_tokens": 50 }
        });
        let resp = provider.parse_response(&body).unwrap();
        assert_eq!(resp.tokens_used.total_tokens, 150);
    }

    #[test]
    fn test_build_request_body_with_tools() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Use the tool".into(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![crate::provider::Tool {
                name: "search".to_string(),
                description: "Search the web".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "search");
        assert_eq!(tools[0]["input_schema"]["type"], "object");
    }

    #[test]
    fn test_build_request_body_no_temperature_for_opus48() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hi".into(),
                cache_breakpoint: false,
            }],
            model: "claude-opus-4-8".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        // Opus 4.8 doesn't support temperature, so it should NOT be in the body
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn test_build_request_body_temperature_for_sonnet46() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hi".into(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        assert_eq!(body["temperature"], 0.5);
    }

    #[test]
    fn test_build_request_body_multiple_system_messages() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![
                crate::SystemBlock {
                    text: "System part 1".to_string(),
                    cache_hint: leviath_core::CacheHint::Always,
                    volatility: leviath_core::Volatility::Stable,
                    region: String::new(),
                },
                crate::SystemBlock {
                    text: "System part 2".to_string(),
                    cache_hint: leviath_core::CacheHint::Always,
                    volatility: leviath_core::Volatility::Stable,
                    region: String::new(),
                },
            ],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        // Multiple system messages → should be an array
        assert!(body["system"].is_array());
        assert_eq!(body["system"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_build_request_body_no_system_messages() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        assert!(body.get("system").is_none());
    }

    #[test]
    fn test_build_request_body_system_block_never_hint_omits_cache_control() {
        // A system block with CacheHint::Never exercises the false branch of
        // the `cache_hint != Never` guard, so no cache_control is attached.
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![crate::SystemBlock {
                text: "No caching here.".to_string(),
                cache_hint: leviath_core::CacheHint::Never,
                volatility: leviath_core::Volatility::Stable,
                region: String::new(),
            }],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        // Single system block → serialized as its plain text, never as a
        // block carrying cache_control.
        assert_eq!(body["system"], "No caching here.");
    }

    #[test]
    fn test_build_request_body_cache_breakpoint_with_block_content() {
        // A message with cache_breakpoint = true AND block (non-Text) content
        // exercises the MessageContent::Blocks arm of the breakpoint handling,
        // which serializes the content normally rather than wrapping it.
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "assistant".to_string(),
                content: crate::MessageContent::Blocks(vec![crate::ContentBlock::Text {
                    text: "block text".to_string(),
                }]),
                cache_breakpoint: true,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        // Block content is serialized normally (an array of content blocks),
        // not wrapped in a synthetic cache_control text block.
        assert_eq!(messages[0]["role"], "assistant");
        assert!(messages[0]["content"].is_array());
        assert_eq!(messages[0]["content"][0]["type"], "text");
        assert_eq!(messages[0]["content"][0]["text"], "block text");
    }

    // ── SSE parsing tests ──────────────────────────────────────────────────

    #[test]
    fn test_parse_sse_event_text_delta() {
        let mut buffer = "event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n".to_string();
        let mut tool_index = 0usize;
        let chunk = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        assert_eq!(chunk.delta, "Hello");
        assert!(chunk.tool_calls.is_empty());
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_parse_sse_event_input_json_delta() {
        let mut buffer = "event: content_block_delta\ndata: {\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"key\\\"\"}}\n\n".to_string();
        let mut tool_index = 1usize;
        let chunk = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        assert_eq!(chunk.delta, "");
        assert_eq!(chunk.tool_calls.len(), 1);
        assert_eq!(chunk.tool_calls[0].index, 1);
        assert_eq!(chunk.tool_calls[0].arguments_delta, "{\"key\"");
    }

    #[test]
    fn test_parse_sse_event_content_block_start_tool_use() {
        let mut buffer = "event: content_block_start\ndata: {\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"search\"}}\n\n".to_string();
        let mut tool_index = 0usize;
        let chunk = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        assert_eq!(chunk.tool_calls.len(), 1);
        assert_eq!(chunk.tool_calls[0].id, Some("toolu_1".to_string()));
        assert_eq!(chunk.tool_calls[0].name, Some("search".to_string()));
        assert_eq!(chunk.tool_calls[0].index, 0);
        assert_eq!(tool_index, 1);
    }

    #[test]
    fn test_parse_sse_event_content_block_start_text_returns_none() {
        let mut buffer = "event: content_block_start\ndata: {\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_sse_event_message_delta() {
        let mut buffer = "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":42}}\n\n".to_string();
        let mut tool_index = 0usize;
        let chunk = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        assert_eq!(chunk.finish_reason, Some(FinishReason::ToolCall));
        let tokens = chunk.tokens.unwrap();
        assert_eq!(tokens.completion_tokens, 42);
    }

    #[test]
    fn test_parse_sse_event_message_start_with_usage() {
        let mut buffer = "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":50,\"cache_creation_input_tokens\":10}}}\n\n".to_string();
        let mut tool_index = 0usize;
        let chunk = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        let tokens = chunk.tokens.unwrap();
        assert_eq!(tokens.prompt_tokens, 100);
        assert_eq!(tokens.cached_tokens, 50);
        assert_eq!(tokens.cache_write_tokens, 10);
    }

    #[test]
    fn test_parse_sse_event_message_start_zero_usage_returns_none() {
        let mut buffer =
            "event: message_start\ndata: {\"message\":{\"usage\":{\"input_tokens\":0}}}\n\n"
                .to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_sse_event_message_stop_returns_none() {
        let mut buffer = "event: message_stop\ndata: {}\n\n".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_sse_event_ping_returns_none() {
        let mut buffer = "event: ping\ndata: {}\n\n".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_sse_event_unknown_event_returns_none() {
        let mut buffer = "event: some_future_event\ndata: {\"foo\":\"bar\"}\n\n".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_sse_event_incomplete_buffer() {
        let mut buffer = "event: content_block_delta\ndata: {\"delta\":".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
        // Buffer should be unchanged
        assert!(buffer.contains("content_block_delta"));
    }

    #[test]
    fn test_parse_sse_event_empty_data_returns_none() {
        let mut buffer = "event: content_block_delta\n\n\n".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_sse_event_comment_line_does_not_set_event_or_data() {
        // A line that doesn't start with "event: " or "data: " (e.g. SSE comment)
        // exercises the else-if's None branch in the for-loop.
        let mut buffer =
            ": this is a comment\nevent: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n"
                .to_string();
        let mut tool_index = 0usize;
        let chunk = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        assert_eq!(chunk.delta, "hi");
    }

    #[test]
    fn test_parse_sse_event_content_block_delta_without_delta_field_returns_none() {
        // content_block_delta event where the JSON has no "delta" key → the ?
        // at json.get("delta")? returns None.
        let mut buffer = "event: content_block_delta\ndata: {\"no_delta\": true}\n\n".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_response_text_block_missing_text_field_is_skipped() {
        // A text block with no "text" key - exercises the if-let None branch
        // in parse_response's content iteration.
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let body = serde_json::json!({
            "content": [
                { "type": "text" },
                { "type": "text", "text": "hello" }
            ],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 5, "output_tokens": 2 }
        });
        let resp = provider.parse_response(&body).unwrap();
        assert_eq!(resp.content, "hello");
    }

    // ─── HTTP error paths (connection refused) ───────────────────────────

    #[tokio::test]
    async fn test_infer_connection_refused_returns_error() {
        let provider = AnthropicProvider {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            base_url: "http://127.0.0.1:19997".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            cache_ttl: CacheTtl::default(),
        };
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 100,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        let result = provider.infer(&request).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with("Request failed:")
        );
    }

    #[tokio::test]
    async fn test_infer_stream_connection_refused_returns_error() {
        let provider = AnthropicProvider {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            base_url: "http://127.0.0.1:19997".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            cache_ttl: CacheTtl::default(),
        };
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 100,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        assert!(provider.infer_stream(&request).await.is_err());
    }

    #[tokio::test]
    async fn test_list_models_connection_refused_returns_error() {
        let provider = AnthropicProvider {
            client: reqwest::Client::new(),
            api_key: "test-key".to_string(),
            base_url: "http://127.0.0.1:19997".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            cache_ttl: CacheTtl::default(),
        };
        let result = provider.list_models().await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with("Request failed:")
        );
    }

    // ─── parse_sse_event: message_delta without usage ─────────────────────

    #[test]
    fn test_parse_sse_event_message_delta_no_usage() {
        let mut buffer =
            "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n"
                .to_string();
        let mut tool_index = 0usize;
        let chunk = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        assert_eq!(chunk.finish_reason, Some(FinishReason::Complete));
        // No usage → tokens default to 0
        let tokens = chunk.tokens.unwrap();
        assert_eq!(tokens.completion_tokens, 0);
    }

    // ─── parse_sse_event: multiple events in buffer ───────────────────────

    #[test]
    fn test_parse_sse_event_multiple_events_consumed_one_at_a_time() {
        let mut buffer = concat!(
            "event: content_block_delta\n",
            "data: {\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n"
        )
        .to_string();
        let mut tool_index = 0usize;

        // First event
        let chunk1 = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        assert_eq!(chunk1.delta, "Hello");

        // Second event
        let chunk2 = parse_sse_event(&mut buffer, &mut tool_index).unwrap();
        assert_eq!(chunk2.delta, " world");

        // Buffer now empty
        assert!(parse_sse_event(&mut buffer, &mut tool_index).is_none());
    }

    // ─── parse_sse_event: content_block_start non-tool type ──────────────

    #[test]
    fn test_parse_sse_event_content_block_start_no_content_block() {
        // content_block_start with no "content_block" key
        let mut buffer = "event: content_block_start\ndata: {\"index\":0}\n\n".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    // ─── parse_sse_event: invalid JSON returns None ───────────────────────

    #[test]
    fn test_parse_sse_event_invalid_json_data_returns_none() {
        let mut buffer = "event: content_block_delta\ndata: not-valid-json\n\n".to_string();
        let mut tool_index = 0usize;
        let result = parse_sse_event(&mut buffer, &mut tool_index);
        assert!(result.is_none());
    }

    // ─── parse_response: stop_reason default end_turn ─────────────────────

    #[test]
    fn test_parse_response_no_stop_reason_defaults_to_complete() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        let body = serde_json::json!({
            "content": [{ "type": "text", "text": "hi" }],
            "usage": { "input_tokens": 5, "output_tokens": 2 }
        });
        let resp = provider.parse_response(&body).unwrap();
        assert_eq!(resp.finish_reason, FinishReason::Complete);
    }

    // ─── build_request_body: cache breakpoints at max limit ───────────────

    #[test]
    fn test_build_request_body_exactly_4_cache_breakpoints() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "key".to_string(),
        );
        // Exactly 4 messages with cache_breakpoint = true
        let messages: Vec<crate::provider::Message> = (0..4)
            .map(|i| crate::provider::Message {
                role: "user".to_string(),
                content: format!("Message {}", i).into(),
                cache_breakpoint: true,
            })
            .collect();

        let request = InferenceRequest {
            system: vec![],
            messages,
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 512,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let msgs = body["messages"].as_array().unwrap();

        // All 4 should have cache_control
        let bp_count = msgs
            .iter()
            .filter(|m| {
                m.get("content")
                    .and_then(|c| c.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|block| block.get("cache_control"))
                    .is_some()
            })
            .count();
        // Placement belongs to the provider now, so the exact count is whatever
        // its rule yields. What must hold is the hard API limit, and that a
        // permitted conversation is served at all.
        assert!(bp_count >= 1, "a permitted conversation carries a marker");
        assert!(bp_count <= 4, "never more than the API accepts: {bp_count}");
    }

    // ─── HTTP-call-level tests via a raw-TCP mock server ───────────────────
    //
    // No mocking crate - bind to an OS-assigned localhost port, accept one
    // connection, write back a fixed HTTP/1.1 response. Enough to exercise
    // infer()/infer_stream()/list_models()'s response-handling branches
    // without a real network call.

    fn provider_with_url(url: String) -> AnthropicProvider {
        AnthropicProvider::with_config(
            crate::provider::build_http_client(None).expect("a test client builds"),
            ProviderConfig {
                api_key: "test-key".to_string(),
                base_url: Some(url),
                rate_limit: None,
                request_timeout_secs: None,
            },
        )
    }

    fn simple_request() -> InferenceRequest {
        InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "hi".into(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn infer_success_parses_response() {
        // Registers a real Subscriber so the tracing::debug! call's field
        // arguments at the top of infer() are actually exercised.
        let _guard = always_on_tracing_guard();
        let body = br#"{
            "content": [{"type": "text", "text": "hello there"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let resp = provider.infer(&simple_request()).await.unwrap();
        assert_eq!(resp.content, "hello there");
        assert_eq!(resp.tokens_used.prompt_tokens, 10);
        assert_eq!(resp.tokens_used.completion_tokens, 5);
    }

    #[tokio::test]
    async fn infer_rate_limited_returns_rate_limit_error() {
        let url = spawn_mock_server(429, "Too Many Requests", b"{}").await;
        let provider = provider_with_url(url);
        let err = provider.infer(&simple_request()).await.unwrap_err();
        assert_eq!(
            std::mem::discriminant(&err),
            std::mem::discriminant(&ProviderError::RateLimitExceeded {
                retry_after_secs: None,
            })
        );
    }

    #[tokio::test]
    async fn infer_rate_limited_with_retry_after_header() {
        let url =
            spawn_mock_server_with_headers(429, "Too Many Requests", "retry-after: 5\r\n", b"{}")
                .await;
        let provider = provider_with_url(url);
        let err = provider.infer(&simple_request()).await.unwrap_err();
        assert_eq!(
            std::mem::discriminant(&err),
            std::mem::discriminant(&ProviderError::RateLimitExceeded {
                retry_after_secs: None,
            })
        );
    }

    #[tokio::test]
    async fn infer_non_success_status_returns_api_error() {
        let url = spawn_mock_server(500, "Internal Server Error", b"boom").await;
        let provider = provider_with_url(url);
        let msg = provider
            .infer(&simple_request())
            .await
            .unwrap_err()
            .to_string();
        assert!(msg.contains("500"));
        assert!(msg.contains("boom"));
    }

    fn assert_contains_500(msg: &str) {
        assert!(msg.contains("500"), "expected 500 in: {msg}");
    }

    #[test]
    #[should_panic(expected = "expected 500 in: not the status you're looking for")]
    fn assert_contains_500_panics_when_missing() {
        assert_contains_500("not the status you're looking for");
    }

    #[tokio::test]
    async fn infer_non_success_status_body_read_error_falls_back_to_unknown_error() {
        let url = spawn_mock_server_truncated_error_body(500, "Internal Server Error").await;
        let provider = provider_with_url(url);
        let msg = provider
            .infer(&simple_request())
            .await
            .unwrap_err()
            .to_string();
        assert_contains_500(&msg);
    }

    #[tokio::test]
    async fn infer_malformed_json_returns_invalid_response() {
        let url = spawn_mock_server(200, "OK", b"not json").await;
        let provider = provider_with_url(url);
        let err = provider.infer(&simple_request()).await.unwrap_err();
        assert!(err.to_string().starts_with("Invalid response:"));
    }

    #[tokio::test]
    async fn infer_stream_rate_limited_returns_error() {
        let url = spawn_mock_server(429, "Too Many Requests", b"{}").await;
        let provider = provider_with_url(url);
        assert!(provider.infer_stream(&simple_request()).await.is_err());
    }

    #[tokio::test]
    async fn infer_stream_rate_limited_with_retry_after_header() {
        let url =
            spawn_mock_server_with_headers(429, "Too Many Requests", "retry-after: 5\r\n", b"{}")
                .await;
        let provider = provider_with_url(url);
        assert!(provider.infer_stream(&simple_request()).await.is_err());
    }

    #[tokio::test]
    async fn infer_stream_non_success_status_returns_api_error() {
        let url = spawn_mock_server(503, "Service Unavailable", b"down").await;
        let provider = provider_with_url(url);
        let result = provider.infer_stream(&simple_request()).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("503"));
    }

    #[tokio::test]
    async fn infer_stream_non_success_status_body_read_error_falls_back_to_unknown_error() {
        let url = spawn_mock_server_truncated_error_body(503, "Service Unavailable").await;
        let provider = provider_with_url(url);
        let result = provider.infer_stream(&simple_request()).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("503"));
    }

    #[tokio::test]
    async fn infer_stream_success_yields_chunks() {
        // Registers a real Subscriber so the tracing::debug! call's field
        // arguments at the top of infer_stream() are actually exercised.
        let _guard = always_on_tracing_guard();
        let sse_body = b"event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";
        let url = spawn_mock_server(200, "OK", sse_body).await;
        let provider = provider_with_url(url);
        let mut stream = provider.infer_stream(&simple_request()).await.unwrap();
        use tokio_stream::StreamExt;
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "hi");
    }

    #[tokio::test]
    async fn list_models_success_returns_models() {
        let body = br#"{"data": [{"id": "claude-sonnet-4-6", "display_name": "Sonnet"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "claude-sonnet-4-6");
        assert_eq!(models[0].display_name.as_deref(), Some("Sonnet"));
        assert_eq!(models[0].provider, "anthropic");
    }

    #[tokio::test]
    async fn list_models_non_success_status_returns_error() {
        let url = spawn_mock_server(401, "Unauthorized", b"bad key").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        // A rejected key is the provider's problem, not a flaky connection:
        // it must not classify as transient (issue #201).
        assert_eq!(
            err.unavailable_reason(),
            Some(crate::provider::UnavailableReason::AuthFailed)
        );
        assert!(!err.is_transient());
        let msg = err.to_string();
        assert!(msg.contains("401"), "{msg}");
        assert!(msg.contains("bad key"), "{msg}");
    }

    #[tokio::test]
    async fn list_models_non_success_status_body_read_error_still_reports_the_status() {
        let url = spawn_mock_server_truncated_error_body(401, "Unauthorized").await;
        let provider = provider_with_url(url);
        let msg = provider.list_models().await.unwrap_err().to_string();
        assert!(msg.contains("401"), "{msg}");
    }

    #[tokio::test]
    async fn list_models_malformed_json_returns_error() {
        let url = spawn_mock_server(200, "OK", b"not json").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().starts_with("Request failed:"));
    }

    #[tokio::test]
    async fn list_models_missing_data_field_returns_error() {
        let url = spawn_mock_server(200, "OK", b"{}").await;
        let provider = provider_with_url(url);
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("data"));
    }

    #[tokio::test]
    async fn list_models_skips_entries_without_id() {
        let body = br#"{"data": [{"display_name": "No ID"}, {"id": "valid-model"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = provider_with_url(url);
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "valid-model");
    }

    // ─── AnthropicSseStream parser (no HTTP needed) ────────────────────────

    struct StaticByteStream {
        data: Vec<Vec<u8>>,
        idx: usize,
    }

    impl futures_core::Stream for StaticByteStream {
        type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
        fn poll_next(
            mut self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            if self.idx < self.data.len() {
                let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                self.idx += 1;
                std::task::Poll::Ready(Some(Ok(chunk)))
            } else {
                std::task::Poll::Ready(None)
            }
        }
    }

    #[tokio::test]
    async fn sse_stream_parses_input_json_delta() {
        use tokio_stream::StreamExt;
        let data = b"event: content_block_delta\ndata: {\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\":1}\"}}\n\n".to_vec();
        let stream = StaticByteStream {
            data: vec![data],
            idx: 0,
        };
        let mut sse = AnthropicSseStream::new(stream);
        let chunk = sse.next().await.unwrap().unwrap();
        assert_eq!(chunk.tool_calls.len(), 1);
        assert_eq!(chunk.tool_calls[0].arguments_delta, "{\"a\":1}");
    }

    #[tokio::test]
    async fn sse_stream_unknown_delta_type_is_skipped() {
        use tokio_stream::StreamExt;
        // An unknown delta type produces None from parse_sse_event, so the
        // stream keeps polling the inner stream until it ends.
        let data =
            b"event: content_block_delta\ndata: {\"delta\":{\"type\":\"unknown_delta\"}}\n\n"
                .to_vec();
        let stream = StaticByteStream {
            data: vec![data],
            idx: 0,
        };
        let mut sse = AnthropicSseStream::new(stream);
        assert!(sse.next().await.is_none());
    }

    #[tokio::test]
    async fn sse_stream_ends_with_incomplete_buffer_returns_none() {
        use tokio_stream::StreamExt;
        // No trailing "\n\n", so the event never completes.
        let data = b"event: content_block_delta\ndata: {\"delta\":{}}".to_vec();
        let stream = StaticByteStream {
            data: vec![data],
            idx: 0,
        };
        let mut sse = AnthropicSseStream::new(stream);
        assert!(sse.next().await.is_none());
    }

    #[tokio::test]
    async fn infer_stream_body_error_propagates_as_stream_item_error() {
        // Send a Content-Length larger than the actual body and close the
        // connection early - reqwest's body stream then yields a real
        // Err(reqwest::Error) mid-stream, exercising AnthropicSseStream's
        // Poll::Ready(Some(Err(e))) branch with a genuine error (not a
        // hand-built one - reqwest::Error has no public constructor).
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let response =
                b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\nshort";
            let _ = socket.write_all(response).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });

        let provider = provider_with_url(format!("http://{}", addr));
        let mut stream = provider.infer_stream(&simple_request()).await.unwrap();
        use tokio_stream::StreamExt;
        let result = stream.next().await;
        assert!(result.is_some());
        assert!(result.unwrap().is_err());
    }

    #[tokio::test]
    async fn sse_stream_parses_trailing_event_left_in_buffer_after_stream_end() {
        use tokio_stream::StreamExt;
        // Two complete "\n\n"-terminated events arrive in a single byte
        // chunk: the first has no "data:" line (parse_sse_event consumes it
        // but returns None), the second is a real content_block_delta. The
        // top-of-loop parse_sse_event check consumes+discards the first
        // event, then polls the inner stream again for more data - which
        // immediately reports the stream as ended (this is the only chunk).
        // That exercises the "stream ended, try to parse any remaining
        // data" fallback, which finds the still-buffered second event.
        let data = b"event: ping\n\nevent: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n".to_vec();
        let stream = StaticByteStream {
            data: vec![data],
            idx: 0,
        };
        let mut sse = AnthropicSseStream::new(stream);
        let chunk = sse.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "hi");
        assert!(sse.next().await.is_none());
    }

    #[tokio::test]
    async fn sse_stream_skips_invalid_utf8_bytes_and_continues() {
        use tokio_stream::StreamExt;
        // First chunk is invalid UTF-8 → skipped without adding to buffer.
        // Second chunk is a valid SSE event.
        let invalid = vec![0xFF, 0xFE, 0x00]; // invalid UTF-8
        let valid = b"event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n".to_vec();
        let stream = StaticByteStream {
            data: vec![invalid, valid],
            idx: 0,
        };
        let mut sse = AnthropicSseStream::new(stream);
        let chunk = sse.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "ok");
    }

    #[test]
    fn cache_ttl_ephemeral_1h_value_and_beta_header() {
        // A provider configured for a 1-hour cache TTL emits the extended-TTL
        // cache_control value and the extended-cache beta header.
        let provider = AnthropicProvider {
            client: reqwest::Client::new(),
            api_key: "k".to_string(),
            base_url: "http://localhost".to_string(),
            rate_limiter: None,
            capability_overrides: HashMap::new(),
            cache_ttl: CacheTtl::Ephemeral1h,
        };
        assert_eq!(
            provider.cache_control_value(),
            serde_json::json!({ "type": "ephemeral", "ttl": "1h" })
        );
        let req = provider
            .apply_headers(provider.client.post("http://localhost/"))
            .build()
            .unwrap();
        assert_eq!(
            req.headers().get("anthropic-beta").unwrap(),
            "extended-cache-ttl-2025-04-11"
        );
    }

    #[test]
    fn count_cache_control_handles_body_without_system_or_messages() {
        // Exercises both `if let Some(..)` false paths (no "system"/"messages"
        // arrays present).
        assert_eq!(count_cache_control(&serde_json::json!({})), 0);
    }

    #[test]
    fn dump_request_ignores_write_failure_when_dir_is_a_file() {
        // Point `dir` at an existing regular file so `create_dir_all` and the
        // subsequent `fs::write` both fail, exercising the write-failure
        // (skip) branch of dump_request.
        let file =
            std::env::temp_dir().join(format!("lev-dump-not-a-dir-{}.tmp", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        dump_request(&serde_json::json!({ "a": 1 }), Some(file.to_str().unwrap()));
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn with_overrides_wires_the_rate_limiter() {
        // The daemon path constructs providers exclusively through
        // with_overrides, so a rate limit that stops here is a rate limit
        // nobody gets.
        let cfg = crate::provider::RateLimitConfig {
            requests_per_minute: 5,
            tokens_per_minute: 1_000,
        };
        let limited = AnthropicProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            HashMap::new(),
            Some(&cfg),
        );
        assert!(limited.rate_limiter.is_some());
        let unlimited = AnthropicProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
            HashMap::new(),
            None,
        );
        assert!(unlimited.rate_limiter.is_none());
    }

    /// Message roles are passed through untouched, including ones this API
    /// rejects. That is the contract callers have to build against: Anthropic
    /// accepts only `user` and `assistant` in `messages`, and nothing here
    /// rescues a `system` role by lifting it into the top-level field.
    ///
    /// Worth pinning because a caller did exactly that. Run titling put its
    /// instruction in a `role: "system"` message, which every OpenAI-shaped
    /// provider accepts and this one 400s, so no run on the default provider
    /// was ever titled - and the failure was swallowed as a best-effort miss.
    #[test]
    fn build_request_body_passes_message_roles_through_untouched() {
        let provider = AnthropicProvider::new(reqwest::Client::new(), "k".to_string());
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::Message {
                role: "system".to_string(),
                content: "be brief".to_string().into(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 16,
            temperature: 0.0,
            tools: Vec::new(),
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);

        assert_eq!(body["messages"][0]["role"], "system");
        assert!(
            body.get("system").is_none(),
            "a system *message* is not promoted to the system field: {body}"
        );
    }

    // ─── message markers ────────────────────────────────────────────────────

    /// The property the whole thing rests on: a marker sits at the same place
    /// next request, so the entry it wrote is looked up exactly where it was
    /// left rather than relying on how far back the provider will search.
    ///
    /// A marker placed relative to the *end* fails this - it moves every turn by
    /// however much the turn appended, and once that step exceeds the lookback
    /// the lookup can never reach the previous entry again. That is absorbing,
    /// because the step does not shrink, and it is the fault behind #474.
    #[test]
    fn a_growing_conversation_keeps_a_marker_where_the_last_one_was() {
        // Each turn appends a dozen-plus content blocks, as a parallel read
        // does: one assistant turn plus six two-block tool exchanges.
        let mut counts: Vec<usize> = vec![1, 1];
        let mut previous: Vec<usize> = Vec::new();
        let mut misses = 0usize;
        for turn in 0..10 {
            counts.push(1);
            counts.extend(std::iter::repeat_n(2, 6));
            let at: Vec<usize> = message_cache_breakpoints(&counts, 2)
                .iter()
                .map(|i| counts[..=*i].iter().sum::<usize>())
                .collect();
            if turn > 0 && !previous.is_empty() && !at.iter().any(|p| previous.contains(p)) {
                misses += 1;
            }
            previous = at;
        }
        // The failure this guards against is *absorbing*: once the marker
        // outruns the lookback it never gets back inside it, so every later call
        // misses. One miss is the handover from the short-conversation fallback
        // to the first anchor and costs a single write.
        assert!(misses <= 1, "{misses} turns found no previous marker");
    }

    /// Consecutive markers stay within the lookback of one another, so even the
    /// newest one is reachable from the entry behind it.
    #[test]
    fn markers_are_never_further_apart_than_the_lookback() {
        let counts: Vec<usize> = (0..60).map(|_| 2).collect();
        let at: Vec<usize> = message_cache_breakpoints(&counts, 4)
            .iter()
            .map(|i| counts[..=*i].iter().sum::<usize>())
            .collect();
        for pair in at.windows(2) {
            assert!(
                pair[1] - pair[0] <= CACHE_LOOKBACK_BLOCKS + 1,
                "{pair:?} is further apart than the lookback"
            );
        }
    }

    /// A single message can be larger than the stride. It is indivisible, so it
    /// takes one marker and the anchor advances past every multiple it covers -
    /// rather than trying to place several markers on one message.
    #[test]
    fn one_message_larger_than_the_stride_takes_one_marker() {
        let counts = vec![1, 200, 1];
        assert_eq!(message_cache_breakpoints(&counts, 4), vec![1]);
    }

    /// A conversation too short to reach the first anchor still gets a marker at
    /// its end. Blocks are what the lookback counts, but tokens are what make
    /// content worth caching, and two messages holding a large file read are
    /// only two blocks.
    #[test]
    fn a_short_conversation_still_gets_one_marker() {
        assert_eq!(message_cache_breakpoints(&[1, 1, 1], 4), vec![2]);
    }

    #[test]
    fn message_markers_respect_a_zero_budget() {
        let counts: Vec<usize> = (0..60).map(|_| 2).collect();
        assert!(message_cache_breakpoints(&counts, 0).is_empty());
    }

    /// This vendor claims the models it serves, and declines the rest.
    ///
    /// `serves_model` is what decides where a bare model name resolves, so a
    /// provider that over-claims wins a model it cannot run. Deciding it from
    /// the capability table did exactly that: the table answers how big a
    /// context window to assume, its fallback for an unknown model is a guess,
    /// and a guess is indistinguishable from a real entry. Measured, `google`
    /// claimed `claude-opus-5`.
    #[test]
    fn it_claims_its_own_models_and_no_one_elses() {
        let provider = AnthropicProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "k".to_string(),
        );
        assert_eq!(
            provider.serves_model("claude-opus-5"),
            Some("claude-opus-5".to_string()),
            "its own model"
        );
        assert!(
            provider.serves_model("gemini-3.1-pro-preview").is_none(),
            "gemini-3.1-pro-preview belongs to another vendor"
        );
        assert!(
            provider.serves_model("gpt-5.5").is_none(),
            "gpt-5.5 belongs to another vendor"
        );
        assert!(
            provider.serves_model("grok-4.6").is_none(),
            "grok-4.6 belongs to another vendor"
        );
        assert!(
            provider.serves_model("not-a-real-model-xyz").is_none(),
            "a model nobody has"
        );
    }
}
