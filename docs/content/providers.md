---
title: Providers
group: Reference
group_order: 3
order: 6
---

# Providers

Leviath needs at least one model provider. Configure them with `lev setup` or by writing keys via
the [API](/docs/api).

| Provider | Env var | Get a key |
|---|---|---|
| Anthropic | `ANTHROPIC_API_KEY` | [console.anthropic.com](https://console.anthropic.com/settings/keys) |
| OpenAI | `OPENAI_API_KEY` | [platform.openai.com](https://platform.openai.com/api-keys) |
| Google (Gemini) | `GOOGLE_API_KEY` | [aistudio.google.com](https://aistudio.google.com/apikey) |
| OpenRouter | `OPENROUTER_API_KEY` | [openrouter.ai/keys](https://openrouter.ai/keys) |
| Ollama | none (local) | [ollama.com/download](https://ollama.com/download) |
| Claude Code | none (subscription) | see below |

## Model identifiers

A model is always named by a `provider` and a `model` together. The `model` string is passed to that
provider verbatim, so it has to be spelled the way the provider spells it:

| Provider | Shape | Example |
|---|---|---|
| Anthropic | the bare model name | `claude-sonnet-5` |
| OpenAI | the bare model name | `gpt-5.4-mini` |
| Google | the bare model name | `gemini-2.5-pro` |
| OpenRouter | `vendor/model` | `deepseek/deepseek-v4-flash` |
| Ollama | `model:tag` | `qwen3.5:9b` |

OpenRouter is the one that trips people up: its identifiers carry a vendor prefix, and the prefix is
part of the name. `deepseek-v4-flash` is not a valid OpenRouter model; `deepseek/deepseek-v4-flash`
is. Browse the full catalog at [openrouter.ai/models](https://openrouter.ai/models), or ask your
install:

```bash
lev models list --provider openrouter        # what Leviath knows offline
lev models list --provider openrouter --remote   # live from the provider's API
lev models list --all                        # every provider, even unconfigured ones
```

> [!NOTE]
> Without `--remote`, `lev models list` prints a built-in table of well-known models. It is a
> convenience, not the catalog. A model absent from it is not necessarily invalid, and Leviath does
> not check model strings against it: `lev validate` never looks at them, and an unrecognized string
> gets a conservative 128K context / 8192 output assumption locally and is then sent to the provider
> as written.

### Dated and rotating identifiers

Providers publish dated aliases (`deepseek/deepseek-v4-flash-0731`,
`deepseek/deepseek-r1-0528`) alongside undated ones. A dated alias can be perfectly valid upstream
while being absent from `lev models list`, and it can also stop resolving later when the provider
retires it. Nothing local will warn you: the first sign is a model-not-found error from the API.

If you pin a dated identifier, treat it as something to revisit. If you would rather not, pin the
undated one and accept that the provider may move it under you.

## Per-stage model selection with fallback

Each [stage](/docs/stages) names an ordered list of provider/model pairs. The runtime picks the
first one you have configured, so a blueprint can prefer a strong model and fall back gracefully:

```mermaid
flowchart LR
  ST["stage: analyze"] --> M1{"anthropic<br/>configured?"}
  M1 -->|yes| U1["use claude-sonnet"]
  M1 -->|no| M2{"openai<br/>configured?"}
  M2 -->|yes| U2["use gpt-mini"]
  M2 -->|no| M3["… next fallback"]
```

This is why a blueprint written against Anthropic still runs for someone who only has OpenAI keys.

The rest of the list is kept, not discarded. If the provider in use stops being usable partway
through a run, the stage moves to the next entry and carries on rather than failing:

```toml
[[stages.analyze.model.models]]
provider = "openrouter"
model    = "deepseek/deepseek-v4-flash"

[[stages.analyze.model.models]]
provider = "anthropic"
model    = "claude-sonnet-5"
```

"Stops being usable" means the account is out of credits, or the key was rejected or is not allowed
to use that model. An ordinary bad request is not that, and never spends a fallback.

## A host-wide fallback chain

A stage that names a single model has nowhere to go on its own. Give the whole host somewhere to
fall back to:

```toml
[providers]
fallback_order = ["anthropic/claude-sonnet-5", "openai/gpt-5.6-mini"]
```

Entries are `provider/model` pairs, best first, and are tried after the stage's own list and your
default model. A failover target needs a model to send, so a bare provider name is not enough. An
entry naming a provider you have not configured is skipped, and a malformed one is ignored with a
warning rather than stopping the daemon.

This is read per run, so editing it takes effect on the next `lev run` with no restart.

## When a provider keeps failing

Failing over saves the run in front of you. It does nothing for the next one, which would start on
the same dead provider and fail the same way. So Leviath counts consecutive failures per provider,
and after a few takes it out of service for every run:

```toml
[limits]
provider_failures_before_open  = 3    # consecutive failures before it is pulled
provider_circuit_cooldown_secs = 300  # how long before it is tried again
```

Three rather than one because a single "payment required" can be one request asking for more output
tokens than the balance covers, which a smaller request would survive. Three in a row is the
account.

While a provider is out, runs move to their next candidate. A run with none left is failed with an
error saying so, rather than left sitting there looking healthy. Once the cooldown passes, the next
request goes through as a probe: if it works the provider is back immediately, and if it fails the
wait restarts. Topping up an account needs no restart.

`lev ps` lists anything currently out of service under the table, with why and how long until it is
retried. `lev ps --json` carries the same under `health.providers_down`, and the
`leviath.provider.circuit.open` metric reports it per provider. Set `provider_failures_before_open`
to `0` to switch the whole thing off and keep only per-run failover.

## Which entry a stage starts on

Failover decides where a run *goes*; this decides where it *begins*. The starting choice is made
once, at spawn, and keys on whether the **provider** is configured, never on whether the model
exists. In order:

1. `lev run --model <provider>/<model>`, which overrides everything and skips the check entirely. A
   bare `--model <model>` replaces only the model name and keeps the provider resolved below.
2. The first entry in `models` whose provider is configured.
3. Your `default_provider` / `default_model` from `config.toml`, if `default_model` is set, its
   provider is configured, and the stage did not set `allow_user_default = false`.
4. The first entry in the list, whether or not its provider exists. If it does not, the run fails at
   spawn with `stage '<name>' has no usable provider`.

> [!IMPORTANT]
> Step 3 is only reached when **no** listed provider is configured. Ollama needs no key and is
> therefore registered unconditionally, so any stage listing Ollama matches at step 2 whether or not
> Ollama is actually installed, and your `default_model` is never consulted for that stage.

### Running the bundled agents on another provider

The [bundled agents](/docs/agent-catalog) list Anthropic, OpenAI, and Ollama, in that order. None of
them lists OpenRouter, Google, or a custom Rhai provider, so a key for one of those does not make
them use it. On an install whose only key is an OpenRouter one:

- Almost every stage matches Ollama at step 2 and runs against `http://localhost:11434`. If Ollama
  is not installed, the run spawns fine and then fails at its first call.
- `parallel-fixer`'s `parallel_fix` and `fix_worker` stages list only Anthropic and OpenAI, so they
  fail at spawn: `stage 'parallel_fix' has no usable provider (tried: anthropic)`.

Setting `default_provider` and `default_model` does **not** fix this, for the reason in the note
above. Three things do. A full override, per run:

```bash
lev run coder -t "fix the failing test" --model openrouter/deepseek/deepseek-v4-flash
```

A host-wide `fallback_order`, which catches a stage once its own entries are exhausted:

```toml
[providers]
fallback_order = ["openrouter/deepseek/deepseek-v4-flash"]
```

Or, for something permanent, copying the blueprint and naming your provider on the stages you care
about:

```toml
model = { models = [
  { provider = "openrouter", model = "deepseek/deepseek-v4-flash" },
  { provider = "anthropic",  model = "claude-sonnet-5" },
] }
```

## Where credentials live

Keys live in `~/.leviath/config.toml` by default, or (to keep them out of a plaintext file) in
your OS keychain. `lev auth` manages the backend:

```bash
lev auth status                 # which backend holds your secrets
lev auth migrate                # move keys into the OS keychain
lev auth migrate --to-file      # move them back to config.toml
lev auth migrate --dry-run      # preview without moving anything
```

## Rate limits

Optional per-provider client-side rate limits, enforced before each call:

```toml
[rate_limits.anthropic]
requests_per_minute = 50
tokens_per_minute   = 100000
```

## Custom OpenAI-compatible providers

Point Leviath at any OpenAI-compatible endpoint with a small Rhai script.
[Rhai providers](/docs/rhai-providers) walks through a complete Groq provider.

## Claude Code transport

If you have Claude Code installed and signed in, you can run Leviath on your Claude subscription
with no API key. Leviath's structured regions still work; the CLI is driven as a plain inference
relay.

> [!CAUTION]
> **Terms of service.** Anthropic's terms state that third-party developers may not offer claude.ai
> login or subscription rate limits for their products without prior approval. Using this transport
> routes inference through your Claude subscription via the CLI's OAuth session. By enabling it, you
> accept responsibility for compliance with Anthropic's terms. For unambiguous compliance, use a
> direct Anthropic API key instead.

Measured caveats: the CLI adds ~130 tokens of its own context to **every** call (including your
account email address and the current date) with no flag to disable it; no prompt caching; a
subprocess per call; Anthropic models only.

Enable it through `lev setup`, or directly:

```toml
[providers]
claude_code_enabled = true
claude_code_binary  = "/usr/local/bin/claude"   # unset resolves `claude` on PATH
claude_code_effort  = "medium"                  # low | medium | high | xhigh | max
```

It is off unless you turn it on, and `lev setup` defaults to declining, so pressing Enter through
the wizard leaves it off. `claude_code_effort` is always sent explicitly: left to itself the CLI
picks `high` with adaptive thinking, spending output tokens and latency Leviath never asked for.
