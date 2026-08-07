---
title: CLI reference
description: Every `lev` command and flag, what each does, and when you need it.
group: Reference
group_order: 3
order: 3
---

# CLI reference (`lev`)

Everything Leviath does is one binary, `lev`. This page lists every command and its flags.
`lev <command> --help` prints the same thing at the terminal.

If a command is not doing what you expect, [Troubleshooting](/docs/troubleshooting) is organised by
symptom, and `lev doctor` checks the usual causes for you.

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
| `-t`, `--task <TEXT\|FILE>` | The task prompt, or the path of a file holding it. Left off, your editor opens |
| `-m`, `--model <MODEL>` | Model override, as `provider/model` or a bare model name |
| `--workdir <DIR>` | Working directory for the run. Defaults to where you ran the command. File tools are confined to it, and relative `[read_paths]` entries resolve against it |
| `--yolo` | Run unattended. See below |
| `--allow <TOOL>` | Allow one tool outright. Repeatable |
| `--max-depth <N>` | Override the blueprint's maximum sub-agent tree depth |
| `--no-seed-commands` | Refuse the blueprint's `seed = { command = "..." }` regions for this run |
| `--count <N>` | Start this many runs of the same agent and task, each under its own run id, from one invocation |
| `--json` | Print the spawned run as JSON rather than a sentence, for a caller that parses the run id back out. With `--count` above 1 it is an array, one object per run |
| `--output-format <LABEL>` | Ask for the final output in this shape. See [Final outputs](/docs/outputs) |
| `--output-instructions <TEXT>` | Extra guidance about that shape |
| `--output-schema <JSON\|@FILE>` | A JSON Schema the final output must satisfy |
| `--<region> <TEXT\|@FILE>` | Seed a named context region. See below |

**`--yolo`** waives approvals, not checkpoints. It approves every tool call, and it takes away the
tools that wait on a person (`ask_user_*`, `present_for_review`, `edit_document`) so the run does
not stop for somebody who is not there.

Two things still hold it. A stage keeps whatever it lists in
[`required_tools`](/docs/tools#these-tools-need-someone-there), and an
[interaction point](/docs/interaction#interaction-points) declaring `unattended = "ask"` opens its
prompt however the run was launched. The shipped `software-engineer` does exactly that for its plan
approval, because everything after that gate writes code. `lev run --yolo` prints what will hold
before the run starts, and `lev validate` reports it as `holds-under-yolo`.

`--yolo` can turn an `ask` into an `allow`, but it can never lift a `deny`.

Region seed flags are dynamic, because region names come from the blueprint. Any `--<name>` that is
not one of the flags above is read as a seed for the region called `<name>`, and a value starting
with `@` is read from that file:

```bash
lev run reviewer --task "Review the auth module" --standards @./team-standards.md
```

A region only accepts a seed if the blueprint declares it as caller input: a string
`seed = "<key>"` in its `[context.regions]` entry, or being named `task`, which asks for the `task`
key implicitly. A table seed (`seed = { glob = ... }`, `{ command = ... }`, and so on) fills the
region from somewhere else and takes no caller input, and a `--<name>` naming any other region is
dropped.

> [!NOTE]
> `--task` fills the caller-input key `task`. A blueprint receives it only if some region asks for
> that key, either with `seed = "task_input"` or by being named `task` (which gets the seed
> implicitly). A blueprint with neither has nowhere to put the prompt, and it is dropped.

#### Writing the task in your editor

Run `lev run <agent>` with no `-t` and Leviath opens your editor on a short commented template.
Type the task, save, and the run starts. Lines beginning with `#` are stripped, so none of the
template reaches the agent. Save an empty file and the run is cancelled.

The editor is `$VISUAL`, then `$EDITOR`, then the first of `vim`, `nano`, `vi` that is installed.
On Windows it is `edit`, then `notepad`, then `vim`. `$VISUAL` and `$EDITOR` are split on
whitespace, so `code --wait` works, but a program path containing spaces needs a wrapper script on
your PATH.

Stdin has to be a terminal for any of this. In a script, a pipeline, or CI, pass `-t` and Leviath
says so rather than blocking.

`-t` reads a file when the value names one that exists. It is an error when the value looks like a path but no such
file is there, so a mistyped filename fails instead of quietly becoming the prompt. "Looks like a
path" means no spaces, plus a `/`, a `\`, or a leading `~`. Region flags work the other way round and want an
explicit `@` before a path, because a region seed is usually a file while a task is usually a
sentence.

### `lev create <NAME>`

Scaffold a new [blueprint](/docs/agents) directory.

| Flag | Default | Purpose |
|---|---|---|
| `-t`, `--template <NAME>` | `software-engineer` | Starting template: `software-engineer`, `coder`, or `researcher` |

### `lev validate [PATH]`

Check a blueprint before running it. `PATH` defaults to `.`.

Beyond parsing and structural validation, it reports what the blueprint leaves unsaid. Findings come
in three levels: an **error** exits non-zero, a **warning** does not, and a **note** never does.

| Level | Code | What it means |
|---|---|---|
| error | `unknown-tool` | A name in `available_tools` matches no built-in, sub-agent tool, or `tools/*.rhai`. The stage silently advertises one tool fewer, so the model is told it does not exist. MCP names (`server__tool`) are skipped, since they resolve only once that server is installed. |
| error | `unparseable-safe-command` | A `[safe_commands] shell` entry that is not a bare command prefix, so no call can ever match it. Write a program, optionally with the subcommand that narrows it: `rg`, `cargo test`. |
| error | `orphan-stage-permission` | A `[stages.X.tool_permissions]` key names a tool the stage never granted. It reads as a grant and is not one. |
| warning | `stage-missing-model` | No `[stages.X.model]` block, so the stage runs on whatever your `default_provider` is. |
| warning | `stage-missing-mode` | No `mode`, so the stage runs as `autonomous`. |
| warning | `stage-missing-max-iterations` | Unbounded unless `[limits] default_max_iterations` is set. Fan-out stages are exempt. |
| warning | `agent-model-block-ignored` | A top-level `[model]` block. Nothing reads it; model selection is per stage. |
| warning | `blocking-tool-in-autonomous-stage` | An autonomous stage grants `ask_user_*`, `present_for_review` or `edit_document`. With nobody attached the run parks there until it is killed. Set `allow_blocking_tools = true` on the stage to say you meant it. |
| warning | `implicit-shell-policy` | A shell grant with no policy behind it. The default is `ask`, and an unattended run waits on that prompt rather than being denied. |
| warning | `unknown-model` | A model this build has not heard of, checked only against providers with a closed catalog. Ollama, OpenRouter and script providers are never checked. |
| warning | `no-reachable-provider` | Nothing in the stage's models list is configured here, so it falls through to your default model. |
| warning | `unreachable-stage`, `cycle-without-max-revisits`, `broad-read-path` | Graph and `[read_paths]` shape. |
| warning | `read-paths-not-granted` | The blueprint declares `[read_paths]` your `config.toml` does not grant. Declaring is not granting, so those reads are refused; the fix line carries the stanza to add. |
| warning | `read-paths-grant-invalid` | A `read_paths` grant in your own config will not compile. It is a hard spawn error, named here first. |
| note | `holds-under-yolo` | A checkpoint that still stops an unattended run for a person: an interaction point declaring `unattended = "ask"`, or a blocking tool a stage keeps in `required_tools`. Deliberate where it appears; noted because `--yolo` reads as "run without me". |
| note | `safe-commands-declared` | The blueprint declares `[safe_commands]`. Declaring is not granting: it applies only where the user opts in, per agent via `[agent_safe_commands.<name>] allow_blueprint` or globally via `[security] allow_blueprint_safe_commands`. |
| note | `command-seed`, `read-paths-declared` | What the blueprint will do that you should know about before running it. `read-paths-declared` carries the granted/declared counts and each entry's status. |

| Flag | Purpose |
|---|---|
| `--deny-warnings` | Exit non-zero on warnings too. Notes still never fail. |

The same findings are written to `daemon.log` when a run spawns, so a blueprint that was never
validated still says what is wrong with it. Nothing there refuses a spawn.

`[read_paths]` entries are checked against your own `config.toml`, entry by entry, because
declaring one is not the same as being allowed to read it. Anything your config does not grant is
named as such, with the stanza that would grant it. The daemon's own lint has no user config to
consult, so there it stays the plain "these need granting" note - see
[reading outside the workdir](/docs/security#reading-outside-the-workdir).

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
| `lev list` | `-f`, `--filter <agents\|blueprints\|all>` (default `all`) | List installed and bundled blueprints. An agent declaring [`[read_paths]`](/docs/security#reading-outside-the-workdir) also shows how many of its entries your config grants |
| `lev add <PACKAGE>` | | Install a blueprint directory or `.leviath-bundle`. Prints what the package grants itself before installing |
| `lev remove <NAME>` | | Uninstall a blueprint |
| `lev pack [PATH]` | `-o`, `--output <FILE>` (default `{name}-{version}.leviath-bundle`) | Bundle a blueprint for [sharing](/docs/packaging) |

## Watching and steering

| Command | Flags | Purpose |
|---|---|---|
| `lev ps` | `--json`, `--all` | List runs in the daemon with their status. `--all` adds runs on disk the daemon is not hosting, finished ones included. See [below](#reading-lev-ps) |
| `lev dash` | | Full-screen TUI [dashboard](/docs/dashboard) |
| `lev msg <AGENT_ID> <CONTENT>` | | Deliver a message into a running agent's context |
| `lev pause <RUN_ID>` | | Pause a run. It finishes its in-flight step, then holds |
| `lev resume <RUN_ID>` | | Un-pause a run |
| `lev cancel <RUN_ID>` | `--force` | Cancel a run. Also aliased as `lev kill` |
| `lev context <RUN_ID>` | `--json`, `--full` | Show a run's context-window history from its `run.lvr` archive |
| `lev result <RUN_ID>` | `--json`, `--raw` | Print what the agent handed back. See [below](#lev-result) |

`lev cancel --force` writes the run's on-disk state terminal without asking the daemon, for when
the daemon is gone or unresponsive. Without it, the daemon is asked first, since it can stop the
work rather than only record the outcome, and the on-disk write is the fallback.

`lev context --full` includes each region's entry contents instead of per-region summaries.

### `lev result`

Print the answer a finished run submitted. It reads the run's `meta.json`, so it needs no daemon and
works for a run that finished last week.

```bash
lev result agent-abc123          # the answer, with its run and stage
lev result agent-abc123 --raw    # the answer alone, for a pipeline
lev result agent-abc123 --json   # the answer plus its shape and stage
```

A run that produced no answer exits non-zero rather than printing nothing. So
`lev result <id> > answer.txt` in a script cannot quietly write an empty file.

Files the run produced are listed under the answer. Fetch one however you normally would; the paths
are relative to the run's working directory.

Only an agent that calls `submit_output` has an answer to show. See
[Final outputs](/docs/outputs) for how a blueprint asks for one.

### `lev respond [REQUEST_ID] [VALUE]`

Answer an interaction the daemon is holding. With no `REQUEST_ID`, lists the open ones.

| Flag | Purpose |
|---|---|
| `--choice <INDEX>` | Answer a multiple-choice interaction by zero-based option index |
| `--approve` | Approve a tool-approval or confirm interaction. Conflicts with `--deny` |
| `--deny` | Deny it |
| `--stage` | With `--approve`, allow what this call runs until the run leaves the current stage |
| `--session` | With `--approve`, allow what this call runs for the rest of the run (alias `--run`) |

See [Human-in-the-loop](/docs/interaction) for what raises these.

### Reading `lev ps`

```
RUN                             STATUS                  STAGE         ITER   TOOLS  AGE
solo-1785568852-9fa61fd279dd    waiting: tool approval  work          1      1      41s
busy-1785568852-384bad04c9ac    active                  work          13824  13824  0s
waiter-1785568852-7895a2209850  waiting: children(1)    delegate 1/2  2      1      41s

1 run needs an answer: lev respond
```

`AGE` is how long since the run last moved: a new iteration, a new stage, or a change of
status. It is deliberately not `meta.json`'s `updated_at`, which also advances on a
30-second heartbeat so that observers can tell a live daemon from a dead one. A fresh
`updated_at` is therefore not evidence of progress; a fresh `AGE` is. The same figure is
written to disk as `last_progress_at`, so a script can read it without the daemon.

`lev ps` lists what the daemon is holding, plus the runs that finished within the
retention window above. `lev ps --all` adds a second block read from the runs dir instead,
so runs older than that window, and runs from before the last daemon restart, are still
accounted for:

```
NOT RUNNING
RUN                             STATUS               LAST MOVED
coder-1785568100-a1b2c3d4e5f6   complete             4m
coder-1785567000-c3d4e5f6a1b2   error                1h
router-1785560000-e5f6a1b2c3d4  running (abandoned)  2h
```

`(abandoned)` means the run claims on disk to be running, the daemon is not holding it,
and it has not moved in five minutes. Clear it with `lev cancel <run-id>`. With `--all` a
daemon that is down is reported rather than fatal, and nothing is marked abandoned in that
case, because an unreachable daemon looks exactly like every run dying at once. See
[reconciling an external work queue](/docs/work-queues) if
you are driving Leviath from a scheduler.

| Status | Meaning |
|---|---|
| `active` | Running a turn, or waiting on the model or a tool |
| `idle` | Spawned, not yet started |
| `paused` | Paused with `lev pause` |
| `waiting` | Blocked - the reason follows the colon |
| `complete` | Finished |
| `cancelled` | Cancelled with `lev cancel` |
| `error` | Ended with the error shown |

A `waiting` run always says what it is blocked on, because the answer decides whether you
need to do anything. These are stopped until a person acts:

| Reason | What to do |
|---|---|
| `tool approval` | A tool call needs approving - `lev respond` |
| `user prompt` | The agent asked a question (`ask_user_*`) - answer it |
| `taint gate` | A call needs clearance for the data it touches |
| `checkpoint` | A blueprint stage-boundary review |

These resolve on their own, and are a normal part of a healthy multi-agent run:

| Reason | Meaning |
|---|---|
| `workers(n)` | A [fan-out](/docs/stages) parent, `n` workers still to finish |
| `children(n)` | A stage holding for `n` spawned [sub-agents](/docs/sub-agents) |

That distinction is the useful one. `waiting: children(3)` next to three busy children is a healthy
run doing exactly what it should. `waiting: tool approval` at ten minutes is a run nobody answered.

Launch with `--yolo` to approve automatically. Sub-agents and fan-out workers inherit it, and it
survives a daemon restart.

#### `(no output)`

A finished run can read `complete (no output)`, and likewise for `cancelled` and `error`. It means
the run changed no files, even though its agent had a tool for changing them.

Almost always the edits went through the shell, which Leviath cannot see. `sed -i`, `tee`, and
redirects leave no trace, so nothing downstream knows the work happened. Either re-apply those edits
with `write_file` or `edit_file`, or name the tool you do write with in a transition
[gate](/docs/stages#what-counts-as-output) so that it counts.

Agents that never had a file-writing tool are never marked this way. A router that delegates, or a
researcher whose answer is its report, has no file changes to be missing.

#### The `READS` column

This column only appears when one of the listed runs declares
[`[read_paths]`](/docs/security#reading-outside-the-workdir). It reads granted over declared, as
resolved when the run spawned.

`0/2` is the one to watch for. That run is up and looks healthy, and every read its author designed
it around will be refused. Run `lev validate <agent>` to see which entries, and the config block
that grants them.

### Runs that have finished

A run keeps its place in the listing for five minutes after it ends, then drops out.

That window exists so a run that failed is still there to say so. Without it, a run that died on its
first model call would leave the listing within seconds, and read exactly like a run that was never
spawned at all.

So you get this instead of an empty listing:

```
RUN                             STATUS                              STAGE  ITER  TOOLS  AGE
worker-1785616492-6f0d21ab4c11  error: HTTP 402 Payment Required    work   0     0      41s
```

`ITER 0` and `TOOLS 0` next to an error mean the run never got as far as its first turn. Set
`[limits] finished_retention_secs` to widen or narrow the window, or `0` to drop a run as soon
as it finishes. The record is held in memory, so restarting the daemon clears it early; the
durable copy is the run's `meta.json`, which `GET /api/agents` reads.

Two things this does not cover. A spawn that fails outright never becomes a run, so it is
reported by `lev run` itself rather than here. And a run that finished longer ago than the
window is gone from the listing for good.

`lev ps --json` prints the same data unformatted, for scripts:

```json
{ "runs": [ ... ], "finished": [ ... ], "health": { ... } }
```

Finished runs are their own key rather than mixed into `runs`, so counting what is running
stays a matter of reading one list. Both carry the `empty_output` field, and a `read_paths`
object with the granted and declared counts when the blueprint declares any. The completion
webhook carries the `empty_output` key.

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
| `--no-remote-yolo` | off | Refuse `"yolo": true` and `"allow": [...]` on spawn requests |

> [!WARNING]
> Prefer `LEVIATH_API_TOKEN` over `--token`. A command-line argument is visible in `ps` to every
> local user for the life of the process.
>
> `--allow-admin` is off by default because the MCP write routes are remote code execution by
> construction: adding a server writes a `command` into your config, which Leviath then spawns.
> `--workdir-root` matters for the same reason: without it a token holder can point a
> tool-executing agent at any directory, including `/`.

## Configuration and tools

### `lev doctor`

Check that provider wiring works, end to end. Four checks run in order, the first failure stops the
rest, and the one that fails is the diagnosis.

| Check | What it proves | A failure means |
|---|---|---|
| `config` | `config.toml` parses and a provider registry can be built | The config file is malformed |
| `resolve` | Your defaults pick a provider that is actually registered | A key is missing or misspelled |
| `inference` | One real call reaches the model | A bad key, an unknown model id, or a billing problem |
| `daemon` | A one-stage agent spawns over the control socket, runs, and finishes | The handoff is broken even though the credentials are fine |

```bash
$ lev doctor

  config     OK  default_provider=openrouter; registered: ollama, openrouter (script providers resolve by name)
  resolve    OK  openrouter / anthropic/claude-sonnet-4.5
  inference  OK  12 in / 4 out / 16 total, replied PONG  (1.2s)
  daemon     OK  run doctor-1785649252-bf7b3d07a265 Complete after 1 iteration(s)  (0.3s)

doctor passed
```

The fourth check spawns a throwaway one-stage agent with no tools, waits for it, and then deletes
the run. Nothing is left in `lev ps` or on disk.

| Flag | Purpose |
|---|---|
| `-m`, `--model <MODEL>` | Test a specific model. Takes the same forms as `lev run --model`: `provider/model` picks both, a bare model id pairs with your `default_provider` |
| `--no-daemon` | Stop after the third check. Contacts no daemon, starts none, and creates no run |
| `--json` | Print the checks as `{"checks": [...], "passed": bool}` |

`--model provider/model` is the way to reach a [Rhai script provider](/docs/rhai-providers), which
is resolved by name and so cannot be listed. Use it to try a model string before wiring it into a
blueprint.

`lev doctor` exits non-zero when a check fails, so it works as a CI gate. It bills two inferences
per run, each capped at 64 output tokens; `--no-daemon` bills one.

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

Each blueprint is listed with what setup would do to it: install it, update it from the version on
disk, or nothing. A copy at the bundled version whose files differ from the bundled ones reads as
`edited locally` and is offered **unchecked**, because installing removes the destination directory
first and would take your edits with it.

`lev run` says the same thing at the moment it matters: a run starting on an installed bundled
blueprint that this build ships a different version of prints a one-line note before it spawns.

Inside the wizard, the keys work the same way on every screen:

| Key | Meaning |
|---|---|
| `↑` `↓` (or `k` `j`) | Move between rows |
| `←` `→` (or `h` `l`) | Cycle a choice or the reasoning effort |
| Space or Enter | Select the focused row; Enter also opens editors for typed values |
| Enter on `[ Continue ]` | Move to the next screen (the button is the last row) |
| Tab / Shift-Tab | Next / previous screen |
| Esc | Previous screen, or cancel an edit or dialog |
| `v` | Re-check a credential against the provider's API |
| `o` | Open the provider's signup page |
| Ctrl-R | Show or hide credentials |
| Ctrl-S | Write the config and finish, from anywhere |
| `?` | Help overlay |
| `q` / Ctrl-C | Quit without writing. If you changed anything, it asks first |

Nothing is written until you confirm on the Review screen. Leaving the provider screen with
nothing selected asks before letting you continue, since an agent cannot run without one.

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

### `lev approvals safe`

Print what runs without an approval prompt, and which file put each entry there. This is the answer
to "why did it not ask me".

| Flag | Purpose |
|---|---|
| `--agent <NAME>` | Include that agent's `[agent_safe_commands.<name>]` entries |
| `--json` | Emit the inventory as JSON |

There is no `list` or `clear`: nothing is persisted. A grant made at a prompt dies with the run that
made it, so the only durable state is the config this reports. See
[Human-in-the-loop](/docs/interaction) for what the entries mean.

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

Examples on this page use Unix shell syntax. On Windows, set variables the way your shell does:

```powershell
$env:LEVIATH_HOME = "D:\leviath"          # PowerShell
```

```bat
set LEVIATH_HOME=D:\leviath
```

The per-command Unix prefix form (`LEVIATH_HOME=/tmp/lev lev ps`) has no direct equivalent; set the
variable first, then run the command.
