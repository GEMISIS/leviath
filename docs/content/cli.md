---
title: CLI reference
group: Reference
group_order: 3
order: 3
---

# CLI reference (`lev`)

Everything Leviath does is one binary, `lev`. This page lists every command and its flags.
`lev <command> --help` prints the same thing at the terminal.

`-v` / `--verbose` is global and works on every subcommand.

Most commands talk to the [shared-world daemon](/docs/daemon). `lev run`, `lev dash`, `lev serve`,
and `lev agent-client` start one automatically if none is running, and restart it if it is running
an older build.

## Running agents

### `lev run [PATH]`

Spawn an agent into the daemon. `PATH` is an installed agent name, a blueprint directory, or an
`agent.leviath` file. Omitted, the current directory is used.

| Flag | Purpose |
|---|---|
| `-t`, `--task <TEXT>` | The task prompt |
| `-m`, `--model <MODEL>` | Model override, as `provider/model` or a bare model name |
| `--workdir <DIR>` | Working directory for the run. Defaults to where you ran the command. File tools are confined to it, and relative `[read_paths]` entries resolve against it |
| `--yolo` | Run unattended: approve every tool call and auto-answer the agent's own prompts (`ask_user_*`, plan approvals). Cannot lift a `deny` |
| `--allow <TOOL>` | Allow one tool outright. Repeatable |
| `--max-depth <N>` | Override the blueprint's maximum sub-agent tree depth |
| `--no-seed-commands` | Refuse the blueprint's `seed = { command = "..." }` regions for this run |
| `--<region> <TEXT\|@FILE>` | Seed a named context region. See below |

Region seed flags are dynamic, because region names come from the blueprint. Any `--<name>` that is
not one of the flags above is read as a seed for the region called `<name>`, and a value starting
with `@` is read from that file:

```bash
lev run reviewer --task "Review the auth module" --standards @./team-standards.md
```

A region only accepts a seed if the blueprint declares it as caller input. `lev validate` lists
which ones do.

> [!NOTE]
> `--task` fills the caller-input key `task`. A blueprint receives it only if some region asks for
> that key, either with `seed = "task_input"` or by being named `task` (which gets the seed
> implicitly). A blueprint with neither has nowhere to put the prompt, and it is dropped.

### `lev create <NAME>`

Scaffold a new [blueprint](/docs/agents) directory.

| Flag | Default | Purpose |
|---|---|---|
| `-t`, `--template <NAME>` | `software-engineer` | Starting template: `software-engineer`, `coder`, or `researcher` |

### `lev validate [PATH]`

Check a blueprint before running it: graph well-formedness and reachability, seed declarations,
`[read_paths]` entries, and the tools each stage asks for. `PATH` defaults to `.`.

### `lev test [PATH]`

Run a blueprint's tests.

| Flag | Purpose |
|---|---|
| `-f`, `--filter <PATTERN>` | Only run matching tests |
| `--dry-run` | Validate the test structure without running agents, so no API calls happen |

### `lev models`

| Command | Flags |
|---|---|
| `lev models list` | `-p/--provider <NAME>`, `-r/--remote` (fetch live from the provider APIs, slower but complete), `-a/--all` (include providers this install has no credential for) |
| `lev models show <MODEL>` | `-p/--provider <NAME>` (required for a remote lookup), `-r/--remote` |

### `lev agent-client`

Serve an agent over the [Agent Client Protocol](/docs/agent-client-protocol) as JSON-RPC on stdio.

| Flag | Purpose |
|---|---|
| `--agent <NAME\|PATH>` | Blueprint to serve. Omitted, each session's working directory is searched for an `agent.leviath` |
| `--yolo` | Approve every tool call without prompting. Recommended for hosts that do not implement `session/request_permission` |
| `--allow <TOOL>` | Allow one tool outright. Repeatable |
| `--max-depth <N>` | Override the maximum sub-agent tree depth |
| `--no-seed-commands` | Refuse the blueprint's command seeds |

## Blueprints and packaging

| Command | Flags | Purpose |
|---|---|---|
| `lev list` | `-f`, `--filter <agents\|blueprints\|all>` (default `all`) | List installed and bundled blueprints |
| `lev add <PACKAGE>` | | Install a blueprint directory or `.leviath-bundle`. Prints what the package grants itself before installing |
| `lev remove <NAME>` | | Uninstall a blueprint |
| `lev pack [PATH]` | `-o`, `--output <FILE>` (default `{name}-{version}.leviath-bundle`) | Bundle a blueprint for [sharing](/docs/packaging) |

## Watching and steering

| Command | Flags | Purpose |
|---|---|---|
| `lev ps` | | List runs in the daemon with their status |
| `lev dash` | | Full-screen TUI [dashboard](/docs/dashboard) |
| `lev msg <AGENT_ID> <CONTENT>` | | Deliver a message into a running agent's context |
| `lev pause <RUN_ID>` | | Pause a run. It finishes its in-flight step, then holds |
| `lev resume <RUN_ID>` | | Un-pause a run |
| `lev cancel <RUN_ID>` | `--force` | Cancel a run. Also aliased as `lev kill` |
| `lev context <RUN_ID>` | `--json`, `--full` | Show a run's context-window history from its `run.lvr` archive |

`lev cancel --force` writes the run's on-disk state terminal without asking the daemon, for when
the daemon is gone or unresponsive. Without it, the daemon is asked first, since it can stop the
work rather than only record the outcome, and the on-disk write is the fallback.

`lev context --full` includes each region's entry contents instead of per-region summaries.

### `lev respond [REQUEST_ID] [VALUE]`

Answer an interaction the daemon is holding. With no `REQUEST_ID`, lists the open ones.

| Flag | Purpose |
|---|---|
| `--choice <INDEX>` | Answer a multiple-choice interaction by zero-based option index |
| `--approve` | Approve a tool-approval or confirm interaction. Conflicts with `--deny` |
| `--deny` | Deny it |
| `--session` | With `--approve`, allow that tool for the rest of the session |

See [Human-in-the-loop](/docs/interaction) for what raises these.

## The daemon and API

### `lev daemon [ACTION]`

With no action, runs the [daemon](/docs/daemon) in the foreground.

| Action | Purpose |
|---|---|
| `start` | Start it in the background. A no-op if one is already running |
| `stop` | Shut it down |
| `status` | Report whether it is running and how many agents it hosts |
| `restart` | Stop, then start, reloading persisted agents |
| `install` | Register with the OS supervisor (launchd, or `systemd --user`) so it starts at login and restarts if it dies |
| `uninstall` | Deregister it |

`--socket <ID>` overrides the control socket path and works on every action.

### `lev serve`

Start the [REST and WebSocket API](/docs/api).

| Flag | Default | Purpose |
|---|---|---|
| `-p`, `--port <PORT>` | `3000` | |
| `-H`, `--host <HOST>` | `127.0.0.1` | |
| `--token <TOKEN>` | unset | Bearer token clients must present. Overrides `LEVIATH_API_TOKEN`. The server refuses to start if neither is set |
| `--cors <ORIGIN>` | none | Allow browser requests from an origin. `*` is accepted and means any origin |
| `--allow-admin` | off | Mount the MCP administration and config-write routes |
| `--workdir-root <PATH>` | unset | Restrict agent working directories to this root |
| `--no-remote-yolo` | off | Refuse `"yolo": true` on spawn requests |

> [!WARNING]
> Prefer `LEVIATH_API_TOKEN` over `--token`. A command-line argument is visible in `ps` to every
> local user for the life of the process.
>
> `--allow-admin` is off by default because the MCP write routes are remote code execution by
> construction: adding a server writes a `command` into your config, which Leviath then spawns.
> `--workdir-root` matters for the same reason: without it a token holder can point a
> tool-executing agent at any directory, including `/`.

## Configuration and tools

### `lev setup`

The interactive [provider](/docs/providers) wizard. Every value it asks for has a flag, so the
whole thing is scriptable.

| Flag | Purpose |
|---|---|
| `--non-interactive` | Use only flag values, ask nothing |
| `--no-verify` | Skip checking credentials against the provider APIs |
| `--anthropic-key`, `--openai-key`, `--google-key`, `--openrouter-key <KEY>` | Provider API keys |
| `--ollama-url <URL>` | Ollama base URL |
| `--default-model <MODEL>` | Default model override |
| `--claude-code <true\|false>` | Enable the Claude Code CLI transport. Off unless set |
| `--claude-code-effort <LEVEL>` | `low`, `medium`, `high`, `xhigh`, or `max` |
| `--install-agents` | Install the bundled blueprints without asking |

```bash
lev setup --non-interactive --anthropic-key sk-ant-... --install-agents
```

> [!NOTE]
> The bundled agents are **not** installed unless `--install-agents` is passed in non-interactive
> mode. That is deliberate, so a scripted setup does not write blueprints you did not ask for.

### `lev mcp`

Manage [MCP tool servers](/docs/mcp).

| Command | Flags | Purpose |
|---|---|---|
| `lev mcp add <NAME>` | `--url`, `--command`, `--arg` (repeatable), `--env KEY=VALUE` (repeatable), `--header KEY=VALUE` (repeatable), `--no-login` | Add a server. Detects OAuth and starts a login unless `--no-login` |
| `lev mcp list` | `--json` | List servers and their auth status |
| `lev mcp remove <NAME>` | | Remove a server |
| `lev mcp login <NAME>` | | Authenticate or re-authenticate |
| `lev mcp logout <NAME>` | | Forget stored credentials |
| `lev mcp test <NAME>` | | Connect and list the server's tools |

Transport is inferred from whether you pass `--url` or `--command`.

### `lev auth`

| Command | Flags | Purpose |
|---|---|---|
| `lev auth status` | | Which credential backend is in use and what it holds |
| `lev auth migrate` | `--to-file`, `--dry-run` | Move secrets between `config.toml` and the OS keychain |

`lev auth migrate` moves keys into the OS store by default; `--to-file` moves them back out. Set
`[security] credential_store` in the [config](/docs/configuration#security) first.

### `lev tools`

| Flag | Purpose |
|---|---|
| `--json` | Emit the inventory as JSON |

Lists and validates the global [Rhai tool scripts](/docs/rhai-tools) in `~/.leviath/tools/`.

### `lev policy`

Manage [taint tracking](/docs/security#taint-tracking-experimental) policy rules.

| Command | Flags | Purpose |
|---|---|---|
| `lev policy list` | | List current rules, static and scripted |
| `lev policy add <TOOL>` | `--target <PATTERN>`, `--max-sensitivity <public\|internal\|private>` (default `internal`) | Add an allowlist rule |
| `lev policy test <TOOL>` | `--target <PATTERN>`, `--taint <public\|internal\|private>` (default `private`) | Check whether a call would be gated |

## Environment

`LEVIATH_HOME` redirects the whole data root, and `LEVIATH_CONFIG_PATH` points at an exact config
file. Those two plus the rest are in the
[configuration reference](/docs/configuration#environment-variables).
