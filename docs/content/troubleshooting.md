---
title: Troubleshooting
description: Common snags organised by symptom, starting with what `lev doctor` tells you.
group: Guides
group_order: 4
order: 2
---

# Troubleshooting

Common snags and how to clear them.

## Start with `lev doctor`

Before reading further, run [`lev doctor`](/docs/cli#lev-doctor). It checks the config file, model
resolution, one real inference, and the daemon handoff, in that order, and reports each one. The
check that fails tells you which section below you need. In particular it separates "my keys are
wrong" from "the daemon is stuck", which look identical from the outside.

## The Lair can't reach my server

[The Lair](https://leviath.dev/lair), the browser console, talks straight to your `lev serve`
endpoint, so three things must line up:

1. **The server is running** with a token: `lev serve --token <t>`.
2. **CORS allows the site's origin**: add `--cors https://leviath.dev` (or `--cors "*"`). Without
   it, the browser blocks the request before your server ever sees it.
3. **The token matches** the one you entered in the console.

```bash
lev serve --token 6618… --cors https://leviath.dev --allow-admin
```

> [!WARNING]
> A page served over **https** can't call an **http** endpoint (mixed content). `http://127.0.0.1`
> is exempt, so localhost works; for a remote box use TLS, an SSH tunnel, or the Docker image.

## I get `401 Unauthorized`

The token is missing or wrong. REST clients send `Authorization: Bearer <token>`; WebSocket clients
pass `?token=<token>` in the URL (browsers can't set WS headers). Confirm the value matches what you
passed to `--token`.

## An admin action returns `405` or `404`

Config-write and MCP add/remove live behind `--allow-admin`. Without that flag the mutating route is
not mounted, and you get whichever error fits the path: **405** for `PUT /api/config` and
`POST /api/mcp/servers`, because those paths still answer `GET`, and **404** for
`DELETE /api/mcp/servers/{name}`, because nothing is mounted there at all. The read routes keep
working either way. Restart with `lev serve … --allow-admin`.

## No provider configured

An agent needs at least one [provider](/docs/providers). Run `lev setup`, or point Leviath at a
local [Ollama](https://ollama.com) for a no-key start.

`lev doctor` says which provider your defaults actually resolve to, and which ones it tried to get
there. That matters because a stage naming no model of its own falls back to `anthropic`. So a
machine with only an OpenRouter key can resolve to a provider it has no credential for, spawn, and
sit at iteration 0. When your configured `default_provider` is the one being passed over, the
`resolve` line says that too.

## Every run dies immediately with a payment or auth error

The account behind that provider is out of credits, or its key was rejected. Every run starts,
fails on its first request, and ends at iteration 0 with no tool calls.

Run `lev ps`. A provider in this state is listed under the table with the reason and how long until
it is tried again, and `lev ps --json` carries the same under `health.providers_down`. After a few
failures in a row Leviath stops sending work there at all, so the flood of dead runs stops.

To keep working through it, give the host somewhere else to go:

```toml
[providers]
fallback_order = ["anthropic/claude-sonnet-5"]
```

Runs then move to that model instead of failing. This is read per run, so it takes effect on the
next `lev run` without a restart, and so does topping the original account back up. See
[Providers](/docs/providers#when-a-provider-keeps-failing) for the thresholds.

A run that has nowhere left to go is failed after `[limits] stall_timeout_secs` with a message
saying every provider it could use is out of service, rather than being left to sit there.

Not sure the credential is the problem at all? `lev doctor` checks the provider wiring layer by
layer and names the first one that breaks.

## `has no usable provider`, but I do have a key

The message names what it tried: `stage 'parallel_fix' has no usable provider (tried: anthropic)`.
That stage's `models` list simply never mentions the provider you configured. A blueprint only starts
on a provider it lists, and the [bundled agents](/docs/agent-catalog) list Anthropic, OpenAI, and
Ollama only, so a Google or OpenRouter key does not qualify.

Set `default_provider` and `default_model` together, which puts your provider ahead of the
blueprint's list for every stage that has not opted out. `--model <provider>/<model>` does it for
one run, and copying the blueprint does it per stage. See
[which entry a stage starts on](/docs/providers#which-entry-a-stage-starts-on).

## My run went to Ollama and I never asked for it

Ollama needs no key, so it is registered whether or not a server is running, and every bundled agent
lists it last. With nothing else configured that is the first entry that matches, and the run starts
against `http://localhost:11434`.

Set `default_model` alongside your `default_provider`. Without a model to send, `default_provider`
is never consulted and Ollama wins by default. `lev doctor` says so in its `resolve` line when your
configured provider is being passed over.

A run that does start on a dead Ollama no longer dies there: an unreachable provider is treated the
same as one out of credits, so the stage moves to its next candidate. You will see the swap in the
stage log:

```
[failover] ollama/qwen3.5:9b is unusable (Request failed: error sending request for url
(http://localhost:11434/api/chat)); retrying on openrouter/openai/gpt-4o-mini
```

## My OpenRouter agent finishes without saying anything

Reasoning models on OpenRouter answer with `content: null` and put their text under `reasoning`.
Leviath reads that field when the message carries nothing else, so a reasoning-only turn is no
longer an empty response. If you are on an older build, an agent that loops and finishes silently
on a `deepseek-r1`-style model is this.

The reasoning text is only used when the turn has no content and no tool calls of its own, so it
never displaces real output or a tool call.

## The provider says my model doesn't exist

Model strings are passed to the provider exactly as written and are never checked locally. So a
typo, a missing OpenRouter `vendor/` prefix, or an identifier the provider has retired all show up
the same way: an API error on the first call, not a `lev validate` failure.

Check the spelling against `lev models list --provider <name> --remote`, which asks the provider
rather than Leviath's built-in table. Note that a valid dated identifier such as
`deepseek/deepseek-v4-flash-0731` may be absent from the offline table while still working, so
absence there is not proof of a bad name. See
[model identifiers](/docs/providers#model-identifiers).

## Windows quoting and environment variables

`lev` itself is the same on every platform, and the commands in these docs work unchanged in
PowerShell. Two things around it differ.

Environment variables. The shell examples in these docs use the Unix `VAR=value command` prefix,
which PowerShell and `cmd` do not have:

```powershell
$env:ANTHROPIC_API_KEY = "sk-ant-..."     # PowerShell
lev run coder --task "Fix the failing test"
```

```bat
set ANTHROPIC_API_KEY=sk-ant-...
lev run coder --task "Fix the failing test"
```

Quoting. PowerShell strips the outer quotes before `lev` sees the argument, so a task containing a
literal quote needs escaping, and single quotes are safest when the text contains `$`:

```powershell
lev run coder --task 'Handle the $HOME case'
```

The agent's own shell is a separate matter: it runs through `cmd.exe`, not a POSIX shell, and
Leviath tells the model so. See [which shell you get](/docs/tools#which-shell-you-get).

## A Windows agent keeps trying `cat`, `ls`, and `grep`

The `shell` tool runs through `cmd.exe` on Windows, where those do not exist. Leviath names the
resolved shell in the tool's description and, on Windows, prepends a short system block giving the
PowerShell stand-ins, so an up-to-date install should not do this.

If you see it anyway, check that `shell_hint` has not been turned off in `config.toml` or in the
blueprint's `[agent]` / `[stages.<name>]` block. A blueprint that spells out POSIX commands in its
own prompt will still ask for them; that text has to change in the blueprint. See
[which shell you get](/docs/tools#which-shell-you-get).

## Console windows keep flashing on my Windows desktop

Every child process Leviath starts is asked for no console window, so this
should not happen on 0.2 or later. Before that, each `shell` call, each MCP
server, and each provider that shells out could pop a `cmd.exe` window on the
desktop, which a fleet of agents turned into a steady flicker.

If you still see it, check what is spawning. `lev` itself run from a terminal
shares that terminal and draws nothing extra. The one child that is still meant
to be visible is your editor: `lev run` with no `--task` opens `$EDITOR` in the
console on purpose, and that is not this bug.

## A run says `running` but never does anything

Check its provider first, with `lev doctor`. If a stage's model list names only providers you
haven't configured, `lev run` refuses the spawn outright and tells you which ones it tried.
Configure one with `lev setup`, or add it to `config.toml` and restart the daemon.

A run that gets past that and still can't dispatch (say you removed a provider key after it
started) is failed after `[limits] stall_timeout_secs`, 60 seconds by default. Its `meta.json`
records the reason. Set the limit to `0` to wait indefinitely instead.

Waiting for a busy model is *not* this. An agent queued behind other in-flight requests to the same
model is working as intended and is never failed, however long the queue takes. Raise
`[limits] max_concurrent_inferences` if you want more of them running at once.

## A run I spawned is not in `lev ps`

There are three of these, and they need different answers.

**It is still starting.** A spawn is not a start. Loading the blueprint, running any seed
commands, and getting through the first inference all happen before an agent can call its first
tool, and a cold model can take several seconds on its own. A run in this state is in `lev ps`
already, at `iteration 0`. It is there; give it a moment.

**It finished, and you looked within the retention window.** It is still listed, with the
status it ended on. An `error` row at `ITER 0` and `TOOLS 0` is a run that never got a turn,
and the message says why. `HTTP 402` there means the provider account is out of credit, not
that Leviath lost the run.

**It finished longer ago than the window.** Now it really is gone from the listing, and the
answer is on disk: `~/.leviath/runs/<run-id>/meta.json`, or `GET /api/agents`, which reads the
same records and does not expire them. Widen `[limits] finished_retention_secs` if you are
polling less often than the default five minutes.

If it is none of those, the spawn itself failed and no run was ever created. `lev run` reports
that on the spot, and the daemon logs it at `error` level, so check there rather than in the
listing.

This matters most to anything that schedules work by spawning agents and watching for them.
Poll the listing rather than timing how long a run "should" take: a wall-clock deadline that is
shorter than a cold start will keep giving up on runs that were about to work.

## Every run looks busy but nothing finishes

Run `lev ps` and read the line under the table. If there isn't one, the daemon thinks it is
getting somewhere and the problem is in a particular run rather than the daemon as a whole.

A `lanes:` line means the tool lane is full with batches queued behind it. On its own that is just
a busy factory. What matters is the second half:

```
lanes: tools 8/8 busy, 3 parked, 12 queued  ·  no progress for 14 cycles (7m)
```

A cycle is 30 seconds, so that reads as seven minutes in which work was waiting for the tool lane
and not one run moved. `parked` is not part of the problem: a batch waiting on a person or on
another run holds no capacity. `busy` with a `queued` figure that never falls is.

The usual cause is too little tool capacity for the shape of the workload. Raise
`[limits] max_concurrent_tools` and restart the daemon.

Left alone, the daemon widens the lane itself after `[limits] dead_cycles_before_relief` cycles, 10
by default, and logs it at `error` level. It never cancels anything, and it stops after granting one
extra lane's worth.

The same numbers are exported as `leviath.tool_lane.*` and
`leviath.scheduler.dead_cycles.total` if you have [observability](/docs/observability) on. A
healthy daemon sits at zero dead cycles.

## My work queue thinks runs are still running

A scheduler that hands work to Leviath and marks a slot busy has to learn when the run ends, and
two things get in the way of the obvious approach. `updated_at` in `meta.json` is a 30-second
heartbeat, so it stays fresh on a run that has stopped dead. `pid` is 0 for every run, live or
finished, so a sweeper that reverts on `pid == 0` reverts everything.

Poll `lev ps --all --json` instead, and read `last_progress_at` rather than `updated_at`. The
[reconciliation recipe](/docs/work-queues) covers the four cases and,
importantly, what to do when the daemon does not answer, which is nothing.

## A run says `running` but nothing in it is moving at all

Not the stall above, and not a slow one. A run can end up in a state no part of the engine can
reach: no inference in flight, no tool batch, nothing waiting on it. It has stopped for good and
still reports `running`.

Set `[limits] wedge_timeout_secs = 300` and the daemon fails such a run instead of leaving it. It
is off by default because it fails runs. A run it fails logs at `error` level and its `meta.json`
carries the reason, which begins `[wedged]` in the stage log. Nothing else in Leviath produces that
line, so it means the engine lost track of a run. Please report it.

It never fires on a run that is merely slow. An agent waiting on the model, on a tool, on its
sub-agents, or on a person is holding the marker that says so and is exempt however long it takes.

## An agent seems stuck in a loop

That's what [stuck detection](/docs/stages#stuck-detection) is for. Add a `condition = "stuck"` transition
with thresholds (`stuck_after_iterations`, `stuck_after_same_file_edits`, …) so the runtime escapes
the stage automatically instead of burning tokens.

## A run vanished when I closed my terminal

It didn't. `lev run` hands the agent to the [daemon](/docs/daemon), which keeps hosting it. Bring
it back with `lev ps` or `lev dash`. If the daemon itself was stopped, it reloads interrupted runs
on its next start.

## Install fails with an auth error

No token is needed, because the repos and release assets are public. A 401/403 during install usually
means leftovers from the private alpha: remove any
`url."https://…@github.com/GEMISIS/".insteadOf` rewrite from `~/.gitconfig`, unset stale
`GITHUB_TOKEN` / `HOMEBREW_GITHUB_API_TOKEN` exports (an expired token *fails* requests that
would succeed anonymously), and retry.

> [!NOTE]
> Still stuck? Open an issue or a private security advisory on
> [GitHub](https://github.com/GEMISIS/leviath).
