# Rhai script providers

Add support for any LLM API by dropping a `.rhai` script into
`~/.leviath/providers/` — no recompile, and no restart.

A `RhaiProvider` Rust wrapper implements the full `Provider` trait. Your script
only does **format mapping** (Leviath's request → the API's HTTP body, and the
API's response → Leviath's types). Leviath keeps ownership of the hard runtime
concerns: HTTP transport, rate limiting, per-stage timeouts, retry with
exponential backoff, error classification, and token counting.

## Just works, and hot-reloads

A script becomes a live provider the first time an agent references its name
(e.g. a stage with `provider = "groq"`), and it is **recompiled automatically
whenever the file changes** (the file's mtime is checked on each use). So:

- drop `groq.rhai` in and point an agent at `provider = "groq"` → it works;
- edit `groq.rhai` → the next agent run picks up the change, **no daemon
  restart**;
- a broken script is skipped with a warning (selection falls through to the next
  model), and starts working again once you fix the file.

Scripts are only compiled/run when an agent actually references the name — the
daemon never scans-and-executes every dropped file at startup.

## Discovery & config

The provider name is the script's filename stem (`groq.rhai` → `groq`) or the
key of a `[model_providers.<name>]` table. A config table is **optional** — it
only supplies overrides:

```toml
# ~/.leviath/config.toml
[model_providers.groq]
script  = "groq"                       # optional; defaults to <name>.rhai
api_key = "..."                        # optional; a script may read its own env var instead
base_url = "https://api.groq.com/openai/v1"  # optional

[model_providers.groq.rate_limit]      # optional; enforced by the Rust wrapper
requests_per_minute = 30
tokens_per_minute   = 100000

# Any other keys are forwarded verbatim to the script's initialize(config).
```

`base_url`, `api_key`, and any extra keys are passed to your script's
`initialize(config)` as the `config` map.

## Script contract

**Required**

```rhai
// Runs once when the provider loads. Runs OFFLINE — no HTTP host functions are
// available here. Return a state map persisted across calls.
fn initialize(config) -> Map

// Non-streaming inference.
// request = { system: [{text, cache_hint}], messages: [{role, content, cache_breakpoint}],
//             tools: [{name, description, parameters}], model, max_tokens, temperature,
//             request_timeout_secs, extra }
// return  = { content, tool_calls: [{id, name, arguments}],
//             tokens_used: {prompt_tokens, completion_tokens, total_tokens,
//                           cached_tokens, cache_write_tokens},
//             finish_reason }   // "Complete" | "ToolCall" | "TokenLimit" | "Stop"
fn inference(state, request) -> Map
```

**Optional**

```rhai
// Streaming. `on_chunk` is a function pointer — call it with `on_chunk.call(#{...})`.
// Each chunk: { delta, tool_calls: [{index, id, name, arguments_delta}],
//               tokens: {...}, finish_reason }
fn stream(state, request, on_chunk)

// Remote token counting (else Leviath uses a local heuristic).
fn count_tokens(state, text, model) -> int

// List available models.
fn list_models(state) -> Array   // [{ id, display_name, max_context_tokens, max_output_tokens }]
```

**Metadata** (leading `// @key value` comments, all optional):

```rhai
// @provider groq
// @description Groq inference (fast)
// @default_model llama-3.3-70b-versatile
// @max_context_tokens 131072
// @max_output_tokens 32768
// @supports_streaming true
```

## Host functions

- **HTTP**: `http_get(url [, headers])`, `http_post(url, body [, headers])`,
  and `stream_request(url, body, headers, callback)` for SSE streaming. The
  callback receives each SSE `data:` payload.
- **Data**: `parse_json(str)`, `to_json(value)`, `parse_sse(chunk)`.
- **Env/encoding**: `env_var(name)` (→ string or `()`), `encode_uri(str)`,
  `encode_base64(str)`.
- **Tokens**: `count_tokens_heuristic(text, hint)` where `hint` is `"openai"`,
  `"anthropic"`, `"gemini"`, or `"general"`.

Rate limiting, request timeouts, retry, and 429/5xx classification are applied
by the Rust wrapper around these calls — you don't implement them.

## Error handling

Signal errors by throwing a structured map:

```rhai
throw #{ message: "API key not set", transient: false };  // permanent
throw #{ message: "server 503",       transient: true  };  // retried with backoff
```

`transient: true` → retryable; a 429 returned by `http_post`/`stream_request` is
mapped to a rate-limit error automatically.

## Not in scope

Mid-run provider switching (a single `inference()` call always uses one
consistent snapshot of the script), a community registry, and provider
composition.

See [`examples/groq.rhai`](examples/groq.rhai) for a complete, working provider.
