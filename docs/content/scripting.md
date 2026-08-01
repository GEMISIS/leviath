---
title: Rhai scripting
group: Reference
group_order: 3
order: 6
---

# Rhai scripting

Leviath is extensible without a recompile. Drop a [Rhai](https://rhai.rs) script into the right
directory and it becomes a live part of the runtime. Four extension points share this model: custom
model [providers](/docs/providers), custom [context](/docs/context) regions, global
[tools](/docs/tools) offered to every agent, and scripted [taint-gate](/docs/security) policy rules.

Each script is small and focused. Leviath keeps ownership of the hard runtime concerns (transport,
budgets, sandboxing, retries) and hands the script only the format or decision it owns. Every script
runs in the same hardened sandbox: no `eval`, no `import`, no ambient filesystem or network (the host
functions listed below are the only way out), bounded operations, and a capped call depth.

This page walks each extension point end to end with complete, copy-pasteable examples. Function and
host-API names here are the exact ones the runtime looks for, so a typo is a hook that never fires.

## Custom providers

A `.rhai` script in `~/.leviath/providers/` teaches Leviath any OpenAI-compatible (or otherwise HTTP)
LLM API. The script does only **format mapping** (Leviath's request to the API's HTTP body, and the
response back). The Rust wrapper keeps HTTP transport, rate limiting, per-stage timeouts, retry with
backoff, error classification, and token counting.

The provider name is the filename stem, so `groq.rhai` is referenced as `provider = "groq"`. A script
becomes a live provider the first time an agent references its name, and it is recompiled
automatically whenever the file changes (the mtime is checked on each use). A broken script is skipped
with a warning and starts working again once you fix the file. The daemon never scans-and-runs every
dropped file at startup, only the ones an agent actually names.

### Where it goes and how it's referenced

Put the file at `~/.leviath/providers/groq.rhai`, then point a stage (or the top-level `[model]`) at
it from the blueprint:

```toml
# agent.leviath
[model]
provider = "groq"                       # the filename stem
model    = "llama-3.3-70b-versatile"    # passed through as request.model

# Per-stage override, if you want one stage on a different provider:
[stages.plan.model]
provider = "groq"
model    = "llama-3.3-70b-versatile"
```

A config table is optional. It only supplies overrides that reach the script's `initialize`:

```toml
# ~/.leviath/config.toml
[model_providers.groq]
script   = "groq"                            # optional; defaults to <name>.rhai
api_key  = "..."                             # optional; the script may read its own env var
base_url = "https://api.groq.com/openai/v1"  # optional

[model_providers.groq.rate_limit]            # optional; enforced by the Rust wrapper
requests_per_minute = 30
tokens_per_minute   = 100000

# Any other keys are forwarded verbatim to initialize(config).
```

### The script contract

A provider defines `initialize` and `inference` (required) and may add `stream`, `count_tokens`, and
`list_models`. Metadata comes from leading `// @key value` comments.

`initialize(config)` runs once when the provider loads. It runs **offline**, so no HTTP host
functions are available here. Return a state map that is persisted and passed to every later call.

`inference(state, request)` does one non-streaming call. `request` carries:

```jsonc
{
  "system":   [ { "text": "...", "cache_hint": "..." } ],
  "messages": [ { "role": "user", "content": "...", "cache_breakpoint": false } ],
  "tools":    [ { "name": "...", "description": "...", "parameters": { /* JSON schema */ } } ],
  "model": "...", "max_tokens": 1024, "temperature": 0.7,
  "request_timeout_secs": 120, "extra": { /* forwarded config keys */ }
}
```

and must return:

```jsonc
{
  "content": "...",
  "tool_calls":  [ { "id": "...", "name": "...", "arguments": { /* parsed */ } } ],
  "tokens_used": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0,
                   "cached_tokens": 0, "cache_write_tokens": 0 },
  "finish_reason": "Complete"   // "Complete" | "ToolCall" | "TokenLimit" | "Stop"
}
```

### Host functions

These are the only calls that reach outside the sandbox:

- HTTP: `http_get(url [, headers])`, `http_post(url, body [, headers])`, and
  `stream_request(url, body, headers, callback)` for SSE streaming. The callback is a Rhai closure
  invoked with each SSE `data:` payload.
- Data: `parse_json(str)`, `to_json(value)`, `parse_sse(chunk)`.
- Env and encoding: `env_var(name)` (returns a string or `()`), `encode_uri(str)`,
  `encode_base64(str)`.
- Tokens: `count_tokens_heuristic(text, hint)` where `hint` is `"openai"`, `"anthropic"`,
  `"gemini"`, or `"general"`.

Rate limiting, request timeouts, retry, and 429/5xx classification are applied by the Rust wrapper
around these calls, so you do not implement them.

### Error handling

Signal an error by throwing a structured map. `transient: true` is retried with backoff; a 429
returned by `http_post`/`stream_request` is mapped to a rate-limit error automatically.

```rhai
throw #{ message: "API key not set", transient: false };  // permanent, no retry
throw #{ message: "server 503",      transient: true  };  // retried with backoff
```

### A complete provider

This is a full OpenAI-compatible provider. Save it as `~/.leviath/providers/groq.rhai`, set
`GROQ_API_KEY`, and point a stage at `provider = "groq"`. It handles non-streaming and streaming
inference, tool calls, token usage, and model listing.

```rhai
// @provider groq
// @description Groq inference (OpenAI-compatible, fast)
// @supports_streaming true
// @default_model llama-3.3-70b-versatile
// @max_context_tokens 131072
// @max_output_tokens 32768

fn initialize(config) {
    let api_key = config.api_key ?? env_var("GROQ_API_KEY");
    if api_key == () { throw #{ message: "GROQ_API_KEY not set", transient: false }; }
    #{
        base_url: config.base_url ?? "https://api.groq.com/openai/v1",
        api_key: api_key,
        model: config.model ?? "llama-3.3-70b-versatile",
    }
}

fn auth_headers(state) {
    #{
        "Authorization": `Bearer ${state.api_key}`,
        "Content-Type": "application/json",
    }
}

fn build_messages(request) {
    let msgs = [];
    if request.system.len() > 0 {
        let sys_text = request.system.map(|b| b.text).reduce(|a, b| a + "\n\n" + b, "");
        msgs.push(#{ role: "system", content: sys_text });
    }
    for msg in request.messages {
        msgs.push(#{ role: msg.role, content: msg.content });
    }
    msgs
}

fn build_tools(request) {
    request.tools.map(|t| #{
        type: "function",
        function: #{ name: t.name, description: t.description, parameters: t.parameters },
    })
}

fn build_body(state, request, streaming) {
    let body = #{
        model: request.model ?? state.model,
        messages: build_messages(request),
        max_tokens: request.max_tokens,
        temperature: request.temperature,
    };
    let tools = build_tools(request);
    if tools.len() > 0 { body.tools = tools; }
    if streaming { body.stream = true; }
    to_json(body)
}

fn map_finish(reason) {
    switch reason {
        "stop" => "Complete",
        "tool_calls" => "ToolCall",
        "length" => "TokenLimit",
        _ => "Complete",
    }
}

fn usage_of(u) {
    if u == () { return #{ total_tokens: 0 }; }
    #{
        prompt_tokens: u.prompt_tokens ?? 0,
        completion_tokens: u.completion_tokens ?? 0,
        total_tokens: u.total_tokens ?? 0,
        cached_tokens: 0,
        cache_write_tokens: 0,
    }
}

fn inference(state, request) {
    let resp = parse_json(http_post(
        `${state.base_url}/chat/completions`,
        build_body(state, request, false),
        auth_headers(state),
    ));
    let choice = resp.choices[0];
    let msg = choice.message;
    let tool_calls = if msg.tool_calls != () {
        msg.tool_calls.map(|tc| #{
            id: tc.id,
            name: tc.function.name,
            arguments: parse_json(tc.function.arguments),
        })
    } else { [] };
    #{
        content: msg.content ?? "",
        tool_calls: tool_calls,
        tokens_used: usage_of(resp.usage),
        finish_reason: map_finish(choice.finish_reason),
    }
}

fn stream(state, request, on_chunk) {
    stream_request(
        `${state.base_url}/chat/completions`,
        build_body(state, request, true),
        auth_headers(state),
        |chunk| {
            let data = parse_sse(chunk);
            if data == () { return; }
            let choice = data.choices[0];
            let delta = choice.delta;
            let result = #{ delta: delta.content ?? "" };
            if choice.finish_reason != () {
                result.finish_reason = map_finish(choice.finish_reason);
            }
            if data.usage != () { result.tokens = usage_of(data.usage); }
            on_chunk.call(result);
        },
    );
}

fn count_tokens(state, text, model) {
    count_tokens_heuristic(text, "openai")
}

fn list_models(state) {
    let resp = parse_json(http_get(`${state.base_url}/models`, auth_headers(state)));
    resp.data.map(|m| #{
        id: m.id,
        display_name: m.id,
        max_context_tokens: m.context_window ?? 8192,
        max_output_tokens: 4096,
    })
}
```

The three optional functions each carry their own shape. `stream(state, request, on_chunk)` calls
`on_chunk.call(#{...})` per delta, where each chunk map is
`{ delta, tool_calls: [{index, id, name, arguments_delta}], tokens: {...}, finish_reason }`.
`count_tokens(state, text, model)` returns an int (Leviath falls back to a local heuristic without
it). `list_models(state)` returns an array of
`{ id, display_name, max_context_tokens, max_output_tokens }`.

### Testing it

To adapt this to another OpenAI-compatible API, change the metadata block, the default `base_url` and
env var in `initialize`, and the default `model`. The request/response mapping is usually the same.
For an API that is not OpenAI-shaped, rewrite `build_body` and the response parsing in `inference` to
match its wire format.

```bash
lev models --provider groq     # calls list_models, so it exercises auth + parse_json end to end
lev run <agent> --stage plan   # a live run through the stage that references the provider
```

> [!TIP]
> A broken provider script is skipped and model selection falls through to the next configured model,
> so a syntax error looks like "my agent quietly used the wrong model". Run `lev models --provider
> <name>` first; a compile or auth error surfaces there instead of hiding behind a fallback.

See [Providers](/docs/providers) for how a stage selects a model and orders fallbacks.

## Custom regions

When the built-in region kinds (`pinned`, `sliding_window`, `temporary`, `compacting`, `clearable`,
`compact_history`, `hashmap`) do not fit, a `kind = "custom"` [context](/docs/context) region hands
one region's behavior to a Rhai script: how it renders into the model's context, what happens to each
incoming entry, and what gets dropped under budget pressure.

### Declaring one

```toml
[context.regions.brain]
kind       = "custom"
script     = "context_hooks/brain.rhai"   # required, relative to the agent dir
persistent = false                        # optional, default false
budget     = "40%"                        # budgets work exactly as on built-ins
min_tokens = 10000
```

`script` resolves relative to the directory holding `agent.leviath`, so the script travels with the
agent through `lev add` and bundles. `persistent = true` makes the region pinned-like (never evicted,
fixed budget, `Always` cache hint). `persistent = false` behaves like temporary (evicted under
pressure, but the script gets first say through `on_overflow`). Percentage budgets resolve at spawn
against the stage model's window, and the script sees the resolved absolute number in
`ctx.region.budget`. Per-stage layouts (`[stages.<name>.context.regions.<region>]`) may declare
custom regions too.

The script is read and compile-checked **once, at spawn**. A missing or broken file, or one without
`fn render(ctx)`, is a hard spawn error that `lev validate` also reports. Editing the script takes
effect on the next run.

### The three hooks

All three receive one `ctx` argument. Rhai passes arguments by value, so mutating `ctx` does nothing.
Each hook **returns** its result.

`render(ctx)` is **required**. It runs on every inference (even when the region is empty, so a script
can emit static scaffolding). `ctx`:

```jsonc
{
  "region":  { "name": "brain", "budget": 80000, "current_tokens": 1234, "entry_count": 7 },
  "entries": [ {
      "content": "...", "tokens": 12, "timestamp": 1710000000, "key": null,
      "kind": "text" | "user_message" | "assistant_turn" | "tool_result",
      // assistant_turn only:
      "tool_calls": [ { "id": "...", "name": "...", "arguments": { } } ],
      // tool_result only:
      "tool_call_id": "...", "tool_name": "...", "is_error": false
  } ],
  "stage_name": "plan", "stage_iterations": 2, "model": "claude-...",
  "window": { "total_tokens": 9000, "max_tokens": 200000 }
}
```

`render` returns either a string (one system block, empty string means nothing) or a map with
`system` (a string or an array of strings) and `messages` (typed `role`/`content` entries, optionally
carrying `tool_calls` or `tool_results`). The assembler's orphan sanitizer strips any unpaired tool
blocks, so a buggy script cannot produce a provider-invalid request.

`on_write(ctx)` is **optional**. It sees each entry headed into the region, with
`ctx = { region, entry: { content, kind, tokens } }`. Return a string to replace the content (tokens
re-estimated, kind preserved), `true` or `()` to accept unchanged, or `false` to drop the entry.

`on_overflow(ctx)` is **optional**. It runs when the region must shrink, with
`ctx = { region, entries, needed_tokens }`. Return an array of entry indices to drop. If the hook is
absent, or your drops do not free enough, oldest-first eviction makes up the difference.

### A complete region

This region keeps a running "brain", drops noisy successful tool results on write, and under budget
pressure evicts successes before errors. It renders everything as one XML user message (the
[12-Factor Agents](https://github.com/humanlayer/12-factor-agents) "own your context window"
pattern).

```rhai
// context_hooks/brain.rhai

fn render(ctx) {
    let xml = "<context>";
    for entry in ctx.entries {
        xml += `<event kind="${entry.kind}">${entry.content}</event>`;
    }
    xml += "</context>";
    #{ messages: [ #{ role: "user", content: xml } ] }
}

fn on_write(ctx) {
    // Keep tool errors verbatim; trim chatty successes to their first line.
    let entry = ctx.entry;
    if entry.kind == "tool_result" && !entry.content.contains("ERROR") {
        let head = entry.content.split("\n")[0];
        return head;                 // replace content (kind preserved)
    }
    ()                               // accept everything else unchanged
}

fn on_overflow(ctx) {
    // Drop successes first; oldest-first eviction covers any shortfall.
    let drops = [];
    for (entry, i) in ctx.entries {
        if !entry.content.contains("ERROR") { drops.push(i); }
    }
    drops
}
```

Runtime problems never break an inference: a `render` error falls back to a plain `[name]:` block, an
`on_write` error accepts the entry unchanged, and an `on_overflow` error falls back to oldest-first.
Only load-time problems (missing file, compile error, no `render`) fail the spawn.

> [!NOTE]
> Region hooks run in the pure-data sandbox: no filesystem, no network, no host I/O functions. They
> transform the `ctx` they are given and return a value. `lev test` assembles the context with your
> hooks active so you can preview exactly what the provider would receive.

See [Context](/docs/context) for how regions, budgets, and cache hints fit together.

## Global tools

Every `.rhai` file in `~/.leviath/tools/` is compiled at spawn and offered as an executable tool to
**every** agent. This is how you give all agents a shared capability without editing each blueprint.
Per-agent tools live in that agent's own `tools/` directory and are validated by
`lev validate <agent>`; a per-agent tool of the same name shadows the global one.

> [!WARNING]
> The directory is `~/.leviath/tools/`, inside Leviath's data root next to `providers/` and
> `agents/`. It is not `$HOME/tools/`. Every `.rhai` file here becomes a tool for every agent, so
> treat it as a trusted location.

### Declaring a tool

A tool declares itself with leading `// @` directives and reads its arguments from the `params`
object. The recognized directives:

- `// @tool <name>` is required and names the tool (must match a stage's `available_tools` entry).
- `// @description <text>` is an optional one-liner shown to the model.
- `// @param <name> <type> <required|optional> "<description>"` is repeatable. `<type>` is a JSON
  schema type: `string`, `integer`, `number`, `boolean`, `array`, `object`. A typo here produces a
  schema that does not compile, which switches off [argument validation](/docs/tools#argument-validation)
  for the tool (the daemon logs a warning); calls still run, just unchecked.
- `// @requires <cap> [<cap>...]` lists platform capabilities the tool needs (`network`, `shell`,
  `filesystem`), comma or space separated and repeatable. Leviath drops the tool where the platform
  cannot provide one.

The script's return value becomes the tool result: a string is returned verbatim, anything else is
JSON-encoded, and a bare `()` is an empty string. A missing optional param reads as `()`.

### Host functions inside a tool

Tools get a different host surface than providers, because they act on behalf of a running agent:
`http_get(url [, headers])`, `http_post(url, body [, headers])`, `shell(cmd)`, `read_file(path)`,
`write_file(path, content)`, and `env_var(name)` do the I/O (each gated by the tool's `@requires` and
the agent's permissions). The pure helpers are `parse_json`, `to_json`, `encode_uri`, and
`html_to_text`. Shared string/content helpers are also available: `contains`, `starts_with`,
`ends_with`, `trim`, `join`, `split`, `count_tokens`, `is_json`, `is_markdown`, `is_mermaid`,
`is_empty`.

### A complete tool

A minimal transform tool, `~/.leviath/tools/upper.rhai`:

```rhai
// @tool upper
// @description Upper-case text
// @param text string required "input to transform"
params.text.to_upper()
```

A tool that does real I/O, `~/.leviath/tools/web_fetch.rhai`. It declares the `network` capability,
fetches a URL, and hands the model readable prose instead of raw HTML:

```rhai
// @tool web_fetch
// @description Fetch a URL and return its readable text
// @param url string required "the URL to fetch"
// @requires network
let body = http_get(params.url);
html_to_text(body)
```

For parameter shapes that directives cannot express (enums, array `items`, numeric bounds), drop a
sibling `tool.toml` next to the script. When present it overrides the annotations entirely:

```toml
# ~/.leviath/tools/export.toml   (beside export.rhai)
[tool]
name        = "export"
description = "Export in a chosen format"
requires    = ["filesystem"]

[[tool.params]]
name     = "format"
required = true
schema   = { type = "string", enum = ["json", "yaml"], description = "output format" }
```

### Inspecting the inventory

`lev tools` lists the global inventory without starting the daemon. Compiled tools are marked,
files that failed to compile are shown with their reason (they are simply not advertised), and a tool
whose `@requires` capability the platform cannot satisfy is flagged unavailable:

```bash
lev tools           # human-readable inventory, params, requires, and skipped files
lev tools --json    # machine-readable, including param schemas and required capabilities
```

See [Tools](/docs/tools) for how a stage's `available_tools` and `tool_permissions` gate which tools
an agent may actually call.

## Policy rules

The [taint-gate](/docs/security) blocks any exfiltration-capable tool whose data taint exceeds its
clearance. Beyond the static allowlist in `policy.toml`, you can relax the gate with scripted rules:
`.rhai` files in the `leviath/rules/` directory under your OS config dir - `~/.config/leviath/rules/`
on Linux, `~/Library/Application Support/leviath/rules/` on macOS. They are consulted **after** the
static allowlist, and the first script that allows a call wins. The filename stem becomes the rule
name in decisions.

Each rule receives a `context` map with `tool`, `target`, and `taint_level` (a string: `"public"`,
`"internal"`, or `"private"`), and evaluates to a boolean. `true` allows the call.

```rhai
// <config dir>/leviath/rules/company.rhai
context.tool == "send_email"
    && context.target == "ops@corp"
    && context.taint_level == "internal"
```

A script that errors or does not evaluate to a boolean is treated as no match, so a broken rule can
never accidentally open the gate. Inspect and dry-run rules with the CLI:

```bash
lev policy list                                              # static + scripted rules
lev policy test send_email --target ops@corp --taint internal
```

> [!IMPORTANT]
> Scripted rules only ever **allow** calls the gate would otherwise block. They cannot tighten the
> gate or override a deny. See [Security](/docs/security) for the taint model and the full gate
> decision flow.
