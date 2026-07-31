---
title: Rhai scripting
group: Reference
group_order: 3
order: 6
---

# Rhai scripting

Leviath is extensible without a recompile: drop a [Rhai](https://rhai.rs) script into the right
directory and it becomes a live part of the runtime. Four extension points share this model — custom
model providers, custom [context](/docs/context) regions, global [tools](/docs/tools) offered to
every agent, and scripted [taint-gate](/docs/security) policy rules. Each is a small, focused script;
Leviath keeps ownership of the hard runtime concerns (transport, budgets, sandboxing, retries) and
hands the script only the format or decision it owns.

## Custom providers

A `.rhai` script in `~/.leviath/providers/` teaches Leviath any OpenAI-compatible (or otherwise
HTTP) LLM API — no recompile, no daemon restart. The script does only **format mapping** (Leviath's
request → the API's HTTP body, and the response back); the Rust wrapper keeps HTTP transport, rate
limiting, per-stage timeouts, retry with backoff, error classification, and token counting.

The provider name is the filename stem, so `groq.rhai` is referenced as `provider = "groq"`:

```toml
# ~/.leviath/config.toml — a config table is optional; it only supplies overrides
[model_providers.groq]
api_key  = "..."                              # or the script reads its own env var
base_url = "https://api.groq.com/openai/v1"

[model_providers.groq.rate_limit]             # enforced by the Rust wrapper
requests_per_minute = 30
tokens_per_minute   = 100000
```

A script defines `initialize(config)` plus `inference(state, request)` (required), and may add
`stream`, `count_tokens`, and `list_models`. It becomes a live provider the first time an agent
references its name, and is recompiled automatically whenever the file changes.

```rhai
fn initialize(config) {
    let api_key = config.api_key ?? env_var("GROQ_API_KEY");
    if api_key == () { throw #{ message: "GROQ_API_KEY not set", transient: false }; }
    #{ base_url: config.base_url ?? "https://api.groq.com/openai/v1", api_key: api_key }
}
```

> [!TIP]
> `docs/examples/groq.rhai` in the repo is a complete, working provider you can copy. The full script
> contract, host functions, and error-handling conventions are documented in `docs/rhai-providers.md`.
> See also [Providers](/docs/providers).

## Custom regions

When the built-in region kinds (`pinned`, `sliding_window`, `temporary`, `compacting`, `clearable`,
`compact_history`, `hashmap`) don't fit, a `kind = "custom"` [context](/docs/context) region hands
one region's behavior to a Rhai script: how it renders into the model's context, what happens to each
incoming entry, and what gets dropped under budget pressure.

```toml
[context.regions.brain]
kind        = "custom"
script      = "context_hooks/brain.rhai"   # required, relative to the agent dir
persistent  = false                        # optional, default false
budget      = "40%"                        # budgets work exactly as on built-ins
min_tokens  = 10000
```

The script implements `render(ctx)` (required) and optionally `on_write(ctx)` and `on_overflow(ctx)`.
Rhai passes arguments by value, so each hook **returns** its result rather than mutating `ctx`:

```rhai
fn render(ctx) {
    let xml = "<context>";
    for entry in ctx.entries {
        xml += `<event kind="${entry.kind}">${entry.content}</event>`;
    }
    xml += "</context>";
    #{ messages: [ #{ role: "user", content: xml } ] }
}
```

> [!NOTE]
> The script is compile-checked once at spawn — a missing or broken file is a hard spawn error that
> `lev validate` also reports, while runtime errors degrade gracefully and never break an inference.
> Full `ctx` shapes, return conventions, and failure semantics are in `docs/rhai-regions.md`.

## Global tools

Every `.rhai` file in `~/.leviath/tools/` is compiled at spawn and offered as an executable tool to
**every** agent — a way to give all agents a shared capability without editing each blueprint.
Agent-specific tools are validated separately by `lev validate <agent>`.

A tool script declares itself with leading directives and reads its arguments from `params`:

```rhai
// @tool upper
// @description Upper-case text
// @param text string required "input to transform"
// @requires network
params.text.to_upper()
```

Use `lev tools` to see the global inventory — compiled tools are marked, files that failed to compile
are shown with their reason (they are simply not advertised, exactly as the daemon treats them), and a
tool whose `@requires` capability the platform can't satisfy is flagged unavailable:

```bash
lev tools           # human-readable inventory + skipped files
lev tools --json    # machine-readable, including params and required capabilities
```

> [!NOTE]
> The directory is `~/.leviath/tools/` — inside Leviath's data root, alongside `providers/` and
> `agents/`. See [Built-in tools](/docs/tools) for how a stage's `available_tools` and
> `tool_permissions` gate which tools an agent may actually call.

## Policy rules

The [taint-gate](/docs/security) blocks any exfiltration-capable tool whose data taint exceeds its
clearance. Beyond the static TOML allowlist in `policy.toml`, you can relax the gate with scripted
rules: `.rhai` files in `~/.config/leviath/rules/`. They are consulted **after** the static allowlist,
and the first script that allows a call wins — its filename stem becomes the rule name in decisions.

Each rule receives a `context` map with `tool`, `target`, and `taint_level`, and evaluates to a
boolean — `true` allows the call:

```rhai
// ~/.config/leviath/rules/company.rhai
context.tool == "send_email"
    && context.target == "ops@corp"
    && context.taint_level == "internal"
```

A script that errors or doesn't evaluate to a boolean is treated as no match, so a broken rule can
never accidentally open the gate. Inspect and dry-run rules with the CLI:

```bash
lev policy list                                   # static + scripted rules
lev policy test --tool send_email --target ops@corp
```

> [!IMPORTANT]
> Scripted rules only ever **allow** calls the gate would otherwise block; they cannot tighten the
> gate or override a deny. See [Security & sandboxing](/docs/security) for the taint model and the
> full gate decision flow.
