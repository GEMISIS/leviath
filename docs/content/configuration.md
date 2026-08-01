---
title: Configuration
group: Reference
group_order: 3
order: 1
---

# Configuration (`config.toml`)

Machine-wide settings live in `~/.leviath/config.toml`. [`lev setup`](/docs/cli) writes it for you,
and everything below is optional: an install with one provider key works with no other key set.

This page is the exhaustive list. The concept pages explain *why* each knob exists; this one is
where you look up the exact name, type, and default.

> [!NOTE]
> The daemon watches this file and reloads it when it changes, so an edit takes effect on the
> **next** `lev run` with no restart. Boot-time wiring (providers, MCP connections, telemetry
> exporters) still needs `lev daemon restart`. See [the daemon docs](/docs/daemon#config-changes-take-effect-on-the-next-run).

## Top level

```toml
default_provider     = "anthropic"   # provider used when a blueprint names none
default_model        = "claude-sonnet-4-5"   # optional model override
agent_paths          = ["~/projects/my-agents"]   # extra directories scanned for blueprints
openrouter_api_key   = "sk-or-..."   # env fallback: OPENROUTER_API_KEY
ollama_base_url      = "http://localhost:11434"   # env fallback: OLLAMA_HOST
request_timeout_secs = 900           # per-request HTTP timeout to a provider
taint_tracking       = false         # global master switch, see below
batch_tool_hint      = true          # global master switch, see below
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `default_provider` | string | `"anthropic"` | |
| `default_model` | string | unset | |
| `agent_paths` | array of paths | `[]` | Searched in addition to `~/.leviath/agents` |
| `openrouter_api_key` | string | unset | Falls back to `OPENROUTER_API_KEY` |
| `ollama_base_url` | string | unset | Falls back to `OLLAMA_HOST`, then `http://localhost:11434` |
| `request_timeout_secs` | integer | unset | Unset means the 15 minute ceiling. A stage's `[stages.<name>.model] request_timeout_secs` wins for that stage |
| `taint_tracking` | bool | `false` | Turns on [taint tracking](/docs/security) for every agent. Off, an agent still opts in through its own `[security]` block |
| `batch_tool_hint` | bool | `true` | Adds a short system hint telling the model it may batch independent tool calls. An agent or stage can set it either way at the narrower scope |

## `[providers]`

Provider credentials. Every key falls back to the matching environment variable, so you can leave
the file empty in CI.

```toml
[providers]
anthropic_api_key   = "sk-ant-..."   # env fallback: ANTHROPIC_API_KEY
openai_api_key      = "sk-..."       # env fallback: OPENAI_API_KEY
google_api_key      = "..."          # env fallback: GOOGLE_API_KEY
claude_code_enabled = false          # opt in to the Claude Code CLI transport
claude_code_binary  = "/usr/local/bin/claude"   # unset resolves `claude` on PATH
claude_code_effort  = "medium"       # low | medium | high | xhigh | max
```

`claude_code_enabled` is off unless you turn it on. See
[Providers](/docs/providers#claude-code-transport) for the terms note that goes with it.

## `[limits]`

```toml
[limits]
max_concurrent_inferences = 8    # in-flight requests per model without its own pool entry
max_concurrent_tools      = 8    # agents whose tool batches may run at once, daemon-wide
default_max_iterations    = 50   # fallback cap for a stage that sets none
exact_token_counting      = false
script_shell_timeout_secs = 60
```

| Key | Default | Notes |
|---|---|---|
| `max_concurrent_inferences` | `8` | The [inference pool](/docs/engine#inference-pools) cap. Omit or set a large number to effectively unbound it |
| `max_concurrent_tools` | `8` | Size of the shared tool worker pool. Clamped to at least 1 |
| `default_max_iterations` | `50` | A stage's explicit `max_iterations` always wins |
| `exact_token_counting` | `false` | Counts each assembled request exactly before sending and rejects one that would overflow the window. Costs a network round trip per inference on providers with a remote count endpoint |
| `script_shell_timeout_secs` | `60` | Cap on a Rhai script tool's `shell()` host call |

<a id="security"></a>

## `[security]`

Machine-wide switches that are not part of the per-tool permission cascade.

```toml
[security]
allow_seed_commands        = true
allow_local_network        = false
allow_env_vars             = ["MY_PROVIDER_KEY"]
allow_blueprint_read_paths = false
read_paths                 = ["~/.leviath/runs", "glob:~/design-docs/**"]
credential_store           = "file"   # file | keychain
```

| Key | Default | Notes |
|---|---|---|
| `allow_seed_commands` | `true` | Whether a blueprint's `seed = { command = "..." }` regions may run. They execute at spawn, before the first approval prompt. `--no-seed-commands` refuses them for one run |
| `allow_local_network` | `false` | Whether agent fetches may reach loopback, private, and link-local addresses. Off, this blocks cloud metadata, your own `lev serve`, and the LAN |
| `allow_env_vars` | `[]` | Credential-shaped variable names a Rhai script may read through `env_var()`. Matching is exact and case-insensitive, and there is no wildcard |
| `allow_blueprint_read_paths` | `false` | Honors every blueprint's `[read_paths]` as written. Prefer a per-agent grant for anything you did not author |
| `read_paths` | `[]` | Machine-wide read grants. A grant only applies to a path the blueprint also declares, so listing one here opens nothing by itself |
| `credential_store` | `"file"` | `keychain` moves secrets to the OS credential store. Run `lev auth migrate` after changing it |

Grant entries (here and in `[agent_read_paths]`) take three forms: an exact path, which grants its
subtree; `glob:` patterns; and `regex:` patterns, auto-anchored. Both patterns are matched against
the symlink-resolved real path and are written with `/` on every OS. `~/` expands to your home, and
a relative entry resolves against the run's workdir. Full walkthrough in
[Security](/docs/security#reading-outside-the-workdir).

## `[agent_read_paths.<agent>]`

Per-agent read grants, the itemized counterpart of `allow_blueprint_read_paths`.

```toml
[agent_read_paths.cto]
allow = ["~/.leviath/runs", "glob:~/design-docs/**"]
```

## Tool permissions

`[tool_permissions]` sets a machine-wide ceiling. A blueprint's own `[tool_permissions]` may
tighten it but never loosen it.

```toml
[tool_permissions]
shell      = "ask"     # allow | ask | deny
write_file = "ask"
read_file  = "allow"
```

`[agent_tool_permissions.<agent>]` is the escape hatch. Naming an agent replaces the global value
for it, and that becomes the ceiling its blueprint is clamped against.

```toml
[agent_tool_permissions.coder]
shell = "allow"
```

Resolution order, narrowest first: launch flag, stage, agent, this file, built-in default. A launch
flag (`--allow`, `--yolo`) can turn `ask` into `allow` but can never lift a `deny`. The built-in
defaults are in [Built-in tools](/docs/tools).

<a id="tool_script_permissions"></a>

## `[tool_script_permissions]`

Layer 3 of the permission model: what a Rhai script tool may *do*, independent of whether the tool
is visible or approved. Each key is `allow`, `deny`, or `inherit`.

```toml
[tool_script_permissions]
http_get   = "inherit"
http_post  = "inherit"
shell      = "inherit"
read_file  = "inherit"
write_file = "inherit"
env_var    = "inherit"
```

Every field defaults to `inherit`. For `shell`, `read_file`, and `write_file`, that defers to the
agent's own permission for the equivalent built-in and permits the call only when it resolves to
`allow`. For `http_get`, `http_post`, and `env_var`, which have no built-in equivalent, `inherit`
permits the call; the tool itself is still gated by the other three layers. See
[Rhai tools](/docs/rhai-tools).

## `[sandbox]`

The machine-wide default sandbox for tool execution. An agent's or stage's own `[sandbox]`
overrides it, and the two resolve to the **stronger** of the pair, so an installed agent can tighten
its sandbox but never turn one off.

```toml
[sandbox]
kind           = "container"   # none | namespace | container
image          = "debian:bookworm-slim"
engine         = "docker"      # docker | podman | nerdctl | finch; auto-detected when unset
network        = true
mounts         = ["/opt/toolchain:ro"]
persist        = false
on_unavailable = "error"       # error | warn
```

Unset entirely, agents run tools on the host. Details in
[Security and sandboxing](/docs/security#sandboxes).

## `[rate_limits.<provider>]`

Client-side limits enforced before every call, for the built-in providers (`anthropic`, `openai`,
`google`, `openrouter`).

```toml
[rate_limits.anthropic]
requests_per_minute = 50
tokens_per_minute   = 40000
```

This shapes request *rate*. `[limits] max_concurrent_inferences` bounds *concurrency*. Both apply.
Script providers configure theirs under `[model_providers.<name>.rate_limit]` instead.

## `[model_capabilities.<model_id>]`

Per-model overrides that take precedence over the provider's built-in capability table. Useful for
a local or self-hosted model Leviath does not know.

```toml
[model_capabilities.my-local-llama]
supports_temperature = true
supports_streaming   = true
supports_tools       = true
supports_system_prompt = true
max_context_tokens   = 32768
max_output_tokens    = 4096
```

<a id="model_providersname"></a>

## `[model_providers.<name>]`

Optional overrides for a [Rhai script provider](/docs/rhai-providers). A script activates by being
referenced and existing in `~/.leviath/providers/`; this table only supplies extras.

```toml
[model_providers.groq]
script   = "groq"        # defaults to <name>.rhai
api_key  = "..."
base_url = "https://api.groq.com/openai/v1"

[model_providers.groq.rate_limit]
requests_per_minute = 30
tokens_per_minute   = 100000
```

Any other key you add is forwarded verbatim to the script's `initialize(config)`.

## `[[mcp_servers]]`

[MCP](/docs/mcp) tool servers. `lev mcp add` writes these for you.

```toml
[[mcp_servers]]
name      = "github"
transport = "http"        # stdio | http; inferred from command/url when omitted
url       = "https://api.example.com/mcp"
headers   = { Authorization = "Bearer ${GITHUB_TOKEN}" }

[[mcp_servers]]
name      = "local-fs"
transport = "stdio"
command   = "npx"
args      = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
env       = { LOG_LEVEL = "debug" }
```

Values in `headers` and `env` may use `${VAR}` to pull from the environment.

## `[nudge]`

Machine-wide defaults for the empty-response nudge: the `[System]` message injected when a stage's
model replies with text before making any tool call.

```toml
[nudge]
enabled = true
max     = 3
text    = "You have tools available. Please use them to complete the task."
```

All three keys are optional and each is overridden independently by an agent's `[agent.nudge]` or a
stage's `[stages.<name>.nudge]`. `text` supports `{stage}` and `{regions}` placeholders. Defaults
are on, `max = 3`, and a built-in message. See [Nudging](/docs/stages#nudging).

## `[title]`

Auto-generated short run titles.

```toml
[title]
enabled  = true
provider = "anthropic"
model    = "claude-haiku-4-5-20251001"
```

`enabled` defaults to `true`. `provider` and `model` fall back to the run's own first-stage
provider and model.

## `[webhook]`

Delivery tuning for completion webhooks. Every field has a default, so the whole section can be
omitted. The webhook URL itself is per-spawn, not configured here; see [the API docs](/docs/api).

```toml
[webhook]
max_retries   = 3       # retries after the first attempt; 0 disables retries
base_delay_ms = 500     # doubles per retry, capped at max_delay_ms
max_delay_ms  = 30000
timeout_secs  = 10      # per attempt
```

## `[observability]`

OpenTelemetry export, off by default. Full walkthrough in [Observability](/docs/observability).

```toml
[observability]
enabled      = true
exporter     = "otlp"    # otlp | stdout | none
endpoint     = "http://localhost:4318"
service_name = "leviath"
```

`endpoint` falls back to `OTEL_EXPORTER_OTLP_ENDPOINT`, then `http://localhost:4318`. Leviath
exports OTLP over **HTTP/protobuf**, so a collector's gRPC port (4317) will not work.
`service_name` falls back to `OTEL_SERVICE_NAME`, then `"leviath"`.

## Environment variables

Leviath reads a `.env` file from the working directory unless `LEVIATH_SKIP_DOTENV` is set.

| Variable | Effect |
|---|---|
| `LEVIATH_HOME` | Redirects the whole data root. Every home-relative path honors it, which is what makes an isolated test or a second install possible |
| `LEVIATH_CONFIG_PATH` | Path to an exact config file, bypassing the default location |
| `LEVIATH_SKIP_DOTENV` | Set to skip `.env` loading |
| `LEVIATH_RUNS_DIR` | Overrides where run directories are written |
| `LEVIATH_API_TOKEN` | Bearer token for `lev serve`. The server refuses to start without one |
| `LEVIATH_CONTROL_TIMEOUT_SECS` | Deadline for one control-socket request |
| `LEVIATH_DASHBOARD_LOG_PATH` | Overrides the dashboard log file |
| `LEVIATH_DUMP_REQUEST_DIR` | Writes each outgoing provider request to this directory, for debugging |
| `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GOOGLE_API_KEY`, `OPENROUTER_API_KEY` | Provider key fallbacks for `[providers]` |
| `OLLAMA_HOST` | Fallback for `ollama_base_url` |
| `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME` | Fallbacks for `[observability]` |
| `EDITOR`, `VISUAL` | Editor used when a prompt opens one |
| `XDG_CONFIG_HOME` | Where `policy.toml` and scripted rules are looked up |

> [!WARNING]
> A variable whose name looks like a credential is not readable by Rhai scripts through `env_var()`
> unless you list it in `[security] allow_env_vars`. That closed an exfiltration path where a
> two-line script tool could read a provider key and POST it elsewhere with no prompt.

## Where things live on disk

Everything persistent sits under the data root, `<home>/.leviath`, which `LEVIATH_HOME` redirects.

| Path | Holds |
|---|---|
| `config.toml` | This file, created `0600` |
| `mcp-auth.json` | MCP OAuth tokens, created `0600` |
| `runs/` | One directory per run: `meta.json`, `context.json`, `stages.json`, the `run.lvr` journal, per-stage logs |
| `agents/` | Blueprints installed by `lev add` |
| `providers/` | Drop-in [Rhai provider](/docs/rhai-providers) scripts |
| `tools/` | Drop-in [Rhai tool](/docs/rhai-tools) scripts, offered to every agent |
| `dashboard.log` | `lev dash` diagnostics |

The daemon's control socket, its token, its pid file, and a build marker live here too.

## `policy.toml`

Taint-gate policy is a separate file at `~/.config/leviath/policy.toml` (under `XDG_CONFIG_HOME`
when set), managed with [`lev policy`](/docs/cli).

```toml
[[allowlist]]
tool             = "http_post"
to               = ["https://hooks.internal/*"]
max_sensitivity  = "internal"   # public | internal | private

[mcp_overrides."github.create_issue"]
sensitivity = "internal"
direction   = "outbound"
clearance   = "internal"
```

Scripted rules live in `~/.config/leviath/rules/` as `.rhai` files. See
[Rhai tools](/docs/rhai-tools#policy-rules) and [Security](/docs/security#taint-tracking-experimental).
