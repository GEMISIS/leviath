---
title: CLI reference
description: Every lev command and flag, which ones speak --json for scripts and CI, and how to read lev ps status and wait reasons.
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

Scripting against the CLI? `--json` is on `run`, `ps`, `doctor`, `validate`, `list`, `models list`,
`context`, `result`, `respond`, `stages`, `timeline`, `tools`, `update`, `approvals safe`, and `mcp
list`. Everything else prints
for a person. Warnings go to stderr, so stdout parses on its own. A service that would rather
speak HTTP should use [`lev serve`](/docs/api) instead.

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
| `-m`, `--model <MODEL>` | Model override for the whole run, as `provider/model` or a bare model name. Fan-out workers and sub-agents inherit it |
| `--workdir <DIR>` | Working directory for the run, defaulting to where you ran the command. See below |
| `--yolo` | Run unattended. See below |
| `--allow <TOOL>` | Allow one tool outright. Repeatable |
| `--max-depth <N>` | Override the blueprint's maximum sub-agent tree depth |
| `--no-seed-commands` | Refuse the blueprint's `seed = { command = "..." }` regions for this run |
| `--count <N>` | Start this many runs of the same agent and task, each under its own run id, from one invocation |
| `--wait` | Stay attached until the run finishes and print its final output the way `lev result` does. Exits non-zero when the run ends in error or is cancelled. See below |
| `--json` | Print the spawned run as JSON rather than a sentence. See below |
| `--output-format <LABEL>` | Ask for the final output in this shape. A label that differs from what the blueprint declares retires its Rhai validator and JSON schema, with a warning on stderr. See [Final outputs](/docs/outputs) |
| `--output-instructions <TEXT>` | Extra guidance about that shape |
| `--output-schema <JSON\|@FILE>` | A JSON Schema the final output must satisfy |
| `--<region> <TEXT\|@FILE>` | Seed a named context region. See below |

**`--workdir`** decides more than where commands run. File tools are confined to it, and relative
`[read_paths]` entries resolve against it.

**`--json`** is for a caller that parses the run id back out. With `--count` above 1 it prints an
array, one object per run.

**`--wait`** is for a script that wants the answer, not the run id. The command subscribes to the
daemon before spawning, follows the run to its end, and prints the final output once: the
`lev result` rendering, or with `--json` one object holding `run_id`, `status`, and
`final_output`. A run that ends in error or is cancelled exits non-zero, as does losing the
daemon. The run still belongs to the daemon, so an interrupted `lev run --wait` leaves it running
and `lev result <id>` collects the output later. `--wait` refuses `--count` above 1.

**`--yolo`** waives approvals, not checkpoints. It approves every tool call, and it takes away the
tools that wait on a person (`ask_user_*`, `present_for_review`, `edit_document`) so the run does
not stop for somebody who is not there.

Two things still hold it. A stage keeps whatever it lists in
[`required_tools`](/docs/tools#these-tools-need-someone-there), and an
[interaction point](/docs/interaction#interaction-points) declaring `unattended = "ask"` opens its
prompt however the run was launched. A blueprint can ask for that on its plan
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
region from somewhere else and takes no caller input. A `--<name>` naming any other region is
dropped.

> [!NOTE]
> `--task` fills the caller-input key `task`. A blueprint receives it only if some region asks for
> that key, either with `seed = "task_input"` or by being named `task` (which gets the seed
> implicitly). Passing a task to a blueprint with neither is refused at spawn rather than dropped,
> because the run would otherwise answer a question it was never given. The error names the caller
> input the agent does take, so `lev run reviewer -t "..."` points you at `--diff` instead.

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

`-t` reads a file when the value names one that exists. A value that looks like a path with no
file behind it is an error, so a mistyped filename fails instead of quietly becoming the prompt.
"Looks like a path" means no spaces, plus a `/`, a `\`, or a leading `~`. Region flags work the
other way round and want an explicit `@` before a path, because a region seed is usually a file
while a task is usually a
sentence.

### `lev stages <RUN-ID>`

The per-stage ledger, which is where a staged agent's cost lives. A single loop has one
number you can eyeball; a staged agent has a different window per stage, regions that persist
across stages, and per-stage models with different prices.

| Flag | Purpose |
|---|---|
| `--regions` | Also show each stage's per-region token high-water mark, largest first |
| `--visits` | Break each stage into its stays, so a stage entered twice is two rows |
| `--json` | Print the ledger as JSON |

```
STAGE                STATUS         PROMPT     OUTPUT   CACHE RD   CACHE WR        COST
ingest               complete        16832       2249          0          0    ~$0.0891
report               complete        37644        493          0          0    ~$0.1932
summary              complete       252848        648          0          0    ~$1.2812
TOTAL                               307324       3390          0          0    ~$1.5635
```

`CACHE WR` is the write half of a cache decision. Without it a stage showing no reads might be
paying to write a prefix nothing reuses, or might not be caching at all, and the ledger could not
tell those apart.

`COST` is what that stage spent, and it is deliberately not a number you can always get. A leading
`~` means the figure was reconstructed from published rates rather than read off the provider's own
answer, and a bare `?` means at least one call in that stage could not be priced at all, by either
route. A `?` is not a zero: a stage whose calls went unpriced has not been shown to be free, and one
unpriced stage takes the `TOTAL` with it for the same reason.

Every call a run bills counts against the stage it was made in, including the compaction calls that
summarize a full window and the routing call a stage makes at its own boundary. The exception is the
run's title call, which happens once at spawn beside the run rather than inside any stage, so this
column can sum to slightly less than the run's own figure.

`--visits` splits a stage by each stay in it. The row above is the sum across every visit, which is
the right total for the stage and the wrong number to put on a graph of the path the run took, where
a stage entered twice is two nodes. Leviath records up to 128 stays per stage; past that the split
stops and the table says so, while the stage's own row keeps counting.

`--regions` answers the question a structured layout is really asking: what am I paying to carry,
and where. The number shown is the largest each region reached while the stage was active, since a
region is re-sent whole on every call.

Leviath also warns once when a stage's per-call prompt grows past four times its first call. That
is the shape of a region accumulating without a cap, which is the failure that costs money and
the hardest one to spot by eye.

### `lev timeline <RUN-ID>`

Where a run's wall-clock time went. `lev stages` says what each stage cost; this says what the
run was doing for an hour. Every number comes from the journal (`run.lvr`), which timestamps each
model call, tool batch, tool result and status change, so the split is exact.

| Flag | Purpose |
|---|---|
| `--calls` | List every model call: when it started, how long it took, prompt, cached and output tokens |
| `--tree` | Include the run's children (and theirs), one line per run, plus the peak number of calls per model in flight at once |
| `--json` | Print the same data as JSON |

```
deep-researcher-1787779292 (deep-researcher, complete) wall 1:17:02 = model calls 46:55 + tools 0:35 + waiting on children 29:59 + other 0:00

STAGE                MODEL TIME  CALLS     OUTPUT    LARGEST
(title)                    0:03      1         30         30
gather                     6:14     12      19330       7879
polish                    19:59      6     116670      23996
```

A model call's time is measured from the moment the run had nothing else in flight, so it
includes any wait for an inference slot. That is deliberate: the run experienced the queue as
latency, and `--tree` shows which model was oversubscribed.

The command warns about one shape it recognises: several back-to-back replies in the same stage,
each thousands of tokens and all the same size. That is what a reply cut off by the stage's output
cap and retried looks like, and it is easy to miss in a column of token counts.

### `lev create <NAME>`

Scaffold a new [blueprint](/docs/agents) directory.

| Flag | Default | Purpose |
|---|---|---|
| `-t`, `--template <NAME>` | `default` | Starting template. `coder` scaffolds the multi-stage shape; anything else gives a single-stage starting point |

### `lev validate [PATH]`

Check a blueprint before running it. `PATH` is a blueprint directory, an `agent.leviath` file, or
the name of an installed agent, so `lev validate coder` checks `~/.leviath/agents/coder/` the same
way `lev run coder` would run it. It defaults to `.`. A name is looked up only in the install
tree, never in the current directory, so a typo is an error even when the directory you are in
holds a manifest.

Beyond parsing and structural validation, it reports what the blueprint leaves unsaid. Findings come
in three levels: an **error** exits non-zero, a **warning** does not, and a **note** never does.

Parsing itself refuses a few things outright, before any finding is reported: a negative count
anywhere in the file (`max_items = -1`), an unknown key under `[sandbox]`, and an unknown key in a
stage table. Each of those errors names the key.

The blueprint's scripts are compiled too, exactly as a spawn would compile them: a custom region
script, an output validator, or a stage hook script that is missing or will not load fails the
command, since it would fail the run. And because a clean verdict against a config the daemon
would grumble about is worth less than it looks, the command reads your `config.toml` on the way
past: keys nothing reads are named as warnings (usually a typo'd setting), and a
`[model_providers.*]` script entry whose `.rhai` file is not on disk is named along with the path
that was looked for.

| Level | Code | What it means |
|---|---|---|
| error | `unknown-tool` | A name in `available_tools` matches nothing. See below |
| error | `unparseable-safe-command` | A `[safe_commands] shell` entry no call can ever match. See below |
| error | `output-missing-submit-tool` | A stage must produce an output and has no way to submit one. See below |
| error | `orphan-stage-permission` | A `[stages.X.tool_permissions]` key names a tool the stage never granted. It reads as a grant and is not one. |
| error | `unserved-model` | A stage names a model the provider that would run it does not carry. See below |
| warning | `stage-missing-model` | No `[stages.X.model]` block, so the stage runs on whatever your `default_provider` is. |
| warning | `stage-missing-mode` | No `mode`, so the stage runs as `autonomous`. |
| warning | `stage-missing-max-iterations` | Unbounded unless `[limits] default_max_iterations` is set. Fan-out stages are exempt. |
| warning | `agent-model-block-ignored` | A top-level `[model]` block. Nothing reads it; model selection is per stage. |
| warning | `region-seed-not-understood` | A region's `seed` is not a recognized form, so the region starts empty. See below |
| warning | `blocking-tool-in-autonomous-stage` | An autonomous stage grants a tool that waits for a person. See below |
| warning | `implicit-shell-policy` | A shell grant with no policy behind it. See below |
| warning | `unknown-model` | A model this build has not heard of. See below |
| warning | `catalog-unchecked` | A script provider that will not say which models it serves, so a name against it went unchecked. See below |
| warning | `no-reachable-provider` | Nothing in the stage's models list can run here, so it falls through to your default model. See below |
| warning | `compact-summarizes-deliverable` | A `compact` edge would hand a `required` region to the summarizer. See below |
| warning | `unreachable-stage`, `cycle-without-max-revisits`, `broad-read-path` | Graph and `[read_paths]` shape. |
| warning | `dead-end-possible` | Every route out of a stage can run out of budget. See below |
| warning | `fanout-no-escape` | A `fan_out` stage with no `error` or `dead_end` edge, so an unusable split degrades to an empty fan-out. See [sub-agents](/docs/sub-agents) |
| warning | `read-paths-not-granted` | The blueprint declares `[read_paths]` your `config.toml` does not grant. See below |
| warning | `read-paths-grant-invalid` | A `read_paths` grant in your own config will not compile. It is a hard spawn error, named here first. |
| note | `holds-under-yolo` | A checkpoint that still stops an unattended run for a person. See below |
| note | `safe-commands-declared` | The blueprint declares `[safe_commands]`. Declaring is not granting. See below |
| note | `command-seed`, `read-paths-declared` | Things worth knowing before you run the blueprint. See below |

Fifteen of those findings need more than a phrase.

**`unknown-tool`** means the name matches no built-in, no sub-agent tool, and no `tools/*.rhai`
file. The stage then advertises one tool fewer, so the model is told a tool it was meant to have
does not exist. MCP names (`server__tool`) are skipped, since they resolve only once that server is
installed.

**`unparseable-safe-command`** fires on an entry that is not a bare command prefix, so no call can
ever match it. Write a program, optionally with the subcommand that narrows it: `rg`, `cargo test`.

**`output-missing-submit-tool`** means a stage sets `require_output` but never grants
`submit_output`. Use `mode = "output"`, which grants the tool.

**`region-seed-not-understood`** is usually a typo in a table key. It is `{ caller = "task" }`, not
`{ caller_input = "task" }`. An unrecognized seed is ignored, and the region starts empty.

**`blocking-tool-in-autonomous-stage`** fires when an autonomous stage grants `ask_user_*`,
`present_for_review` or `edit_document`. With nobody attached, the run parks there until it is
killed. Set `allow_blocking_tools = true` on the stage to say you meant it.

**`implicit-shell-policy`** matters because the default is `ask`. An unattended run waits on that
prompt rather than being denied.

**`unserved-model`** is the one model finding that fails the command, because it is the one that can
be proved. The provider is configured here, it published the full list of what it carries, and the
model the stage names is not on it. That is a typo or a renamed model rather than anything about your
machine, so a stage naming one is refused at spawn too. The message carries a few of the ids the
provider does list; `lev models list --provider <name>` prints the rest.

A provider publishes its list either by answering `list_models` (a Rhai provider, or a gateway whose
catalogue Leviath has read) or by having one written down under `[model_providers.<name>] serves`.
The `serves` route needs no network and no key, which makes it the way to get a script provider
checked in CI.

**`catalog-unchecked`** is the same question with no answer: the script provider loaded, but it has
neither a `list_models(state)` function nor a `serves` list, so it has never said what it takes and
nothing here can tell a good model id from a bad one. It is a warning rather than an error because
saying nothing is not a refusal. It exists so that "checked and fine" and "never checked" stop
looking identical. Only script providers are named this way; a built-in that keeps quiet is either
covered by `unknown-model` below or has a genuinely open catalog.

**`unknown-model`** is the older, weaker check: the table of models compiled into this build, which
covers Anthropic, OpenAI and Google. It is skipped for any provider that answered for itself, since
a live catalog knows about models released after this build was cut. A provider that neither
publishes a catalog nor appears in that table is not checked at all, which is what keeps an open
catalog (Ollama serves whatever you have pulled) from raising false alarms.

**`no-reachable-provider`** means every entry in the stage's list names something this install cannot
run: a pinned entry whose provider is not configured, or a bare model name nothing here serves. One
entry that works is enough to keep the stage quiet, since the list is an ordered set of fallbacks and
a machine declining some of the options is the normal case. This is also the check that catches a
misspelled model in a stage that names only one: nothing serves `claude-sonet-5`, so the stage would
have fallen through to your default model without saying so.

**`compact-summarizes-deliverable`** means a later stage reads a paraphrase of a region you marked
`required`. Set `summarizable = false` on the region.

**`dead-end-possible`** fires when every normal edge's target has a `max_revisits` budget, so the
run errors once they are spent. Add a `condition = "dead_end"` edge to a stage without one. A
`max_iterations` edge does not count, because it fires on the iteration cap rather than on this
path.

**`read-paths-not-granted`** is the declaring-is-not-granting case. Those reads are refused at
runtime, and the fix line carries the stanza that would grant them.

**`holds-under-yolo`** names an interaction point declaring `unattended = "ask"`, or a blocking tool
a stage keeps in `required_tools`. Both are deliberate wherever they appear. It is a note because
`--yolo` reads as "run without me".

**`safe-commands-declared`** applies only where you opt in. That is per agent via
`[agent_safe_commands.<name>] allow_blueprint`, or globally via
`[security] allow_blueprint_safe_commands`.

**`command-seed`** and **`read-paths-declared`** say what the blueprint will do before you run it.
`read-paths-declared` carries the granted and declared counts, plus each entry's status.

| Flag | Purpose |
|---|---|
| `--deny-warnings` | Exit non-zero on warnings too. Notes still never fail. |
| `--json` | Print the report as one JSON object with `valid`, `blueprint`, `error`, and `findings` |
| `--graph` | Draw the stage graph after the report, as plain text: the same picture the dashboard's stage explorer shows, escape edges included. Ignored with `--json` |
| `--width <COLS>` | How many columns `--graph` may use (default 120); a wider graph is shrunk to fit. Only with `--graph`: on its own it is refused |

The same findings are written to `daemon.log` when a run spawns, so a blueprint that was never
validated still says what is wrong with it. Nothing there refuses a spawn.

`[read_paths]` entries are checked against your own `config.toml`, entry by entry, because
declaring one is not the same as being allowed to read it. Anything your config does not grant is
named as such, with the stanza that would grant it. The daemon's own lint has no user config to
consult, so there it stays the plain "these need granting" note. See
[reading outside the workdir](/docs/security#reading-outside-the-workdir).

### `lev test [PATH]`

Run a blueprint's tests: everything in its `tests/` directory, against the real provider.

| Flag | Purpose |
|---|---|
| `-f`, `--filter <PATTERN>` | Only run cases whose name contains this substring |
| `--dry-run` | Parse and report the cases without calling a provider, so nothing is spent |

Each `tests/*.toml` file holds one or more cases:

```toml
[[test]]
name = "greeting"
input = "Say hello"
expect_contains = "hello"

[[test]]
name = "reads the config"
input = "What is in config.toml?"
expect_tool_call = "read_file"
max_tokens = 500
```

| Key | Meaning |
|---|---|
| `name` | Case name. `--filter` matches on it |
| `input` | Seeded as the task, exactly as `lev run "..."` would |
| `expect_contains` | Case-insensitive substring the response must contain |
| `expect_tool_call` | A tool the model must call. It has to be one the stage lists in `available_tools` |
| `max_tokens` | Caps this case's output. Narrows the ceiling the window and model already impose; it cannot raise it |

**What a case actually runs.** One inference, not a run. `lev test` builds a fresh context
window from the blueprint's layout, seeds `input` as the task, and assembles the request exactly
as a live run's *first* turn would. That means iteration 0, region hooks active, and the first
stage's model and tools. It then calls the provider once and checks the assertions. Nothing
executes: a tool call is
asserted on, never performed, so a case can expect `write_file` without a file appearing.

A `tests/*.rhai` file is run instead as a script through the scripting engine, and fails the run if
it returns `false`.

Before any case runs, the blueprint's own scripts are compiled the way a spawn would compile them:
custom region scripts, output validators, and stage hook scripts all have to load. `--dry-run`
includes those checks, so a broken script is caught without spending anything.

### `lev models`

| Command | Flags |
|---|---|
| `lev models list` | `-p/--provider <NAME>`, `--offline` (this build's table only, no network), `-a/--all` (include providers with no credential here), `--json`. `-r/--remote` is accepted and changes nothing: asking the providers is the default |
| `lev models show <MODEL>` | `-p/--provider <NAME>` (ask only this provider), `--offline`. `-r/--remote` is accepted and changes nothing, as above |

Both ask every configured provider for its own listing by default, waiting up to five seconds each,
and print what the provider said: the columns include the release date and the input and output
price per million tokens where the listing or the build's price table carries them (`n/a` where
neither does), and a trailing line says how many rows came from a provider and how many from the
table compiled into this build. `lev models show` names where a table row's rate came from and the
day the table was read; see [where the prices come from](/docs/costs#where-the-prices-come-from). A provider that could
not be reached keeps its table rows, with a warning naming it. `--offline` skips the network and
prints the table alone. `-r/--remote` is still accepted for older scripts and changes nothing.

`--provider` naming a [Rhai script provider](/docs/rhai-providers) loads that script and calls its
`list_models`: a script names its own catalog at run time, so there is no built-in table to read it
from. What it answers counts as a real provider listing, toward the trailing line and as
`"learned": true` in `--json`. A `serves = [...]` or `[model_capabilities]` claim in your config
never becomes a listing row at all: those feed validation, not this table. A `--provider` that names nothing at all - no configured
provider, no row in the built-in table, no script of that name that loads - **exits non-zero**
rather than printing an empty table, since there is nothing an empty table could be reporting.
A provider the built-in table knows but this install has no credential for is still an empty table
and still exits 0.

### `lev agent-client`

Serve an agent over the [Agent Client Protocol](/docs/agent-client-protocol) as JSON-RPC on stdio.

| Flag | Purpose |
|---|---|
| `--agent <NAME\|PATH>` | Blueprint to serve. Omitted, each session's working directory is searched for an `agent.leviath` |
| `--yolo` | Approve every tool call without prompting. Recommended for hosts that do not implement `session/request_permission` |
| `--allow <TOOL>` | Allow one tool outright. Repeatable |
| `--max-depth <N>` | Override the maximum sub-agent tree depth |
| `--no-seed-commands` | Refuse the blueprint's command seeds |
| `--output-format <LABEL>` | Ask the agent for its [final output](/docs/outputs) in this format. A differing label retires the blueprint's declared validator and schema |
| `--output-instructions <TEXT>` | Extra instructions for that final output |

## Blueprints and packaging

| Command | Flags | Purpose |
|---|---|---|
| `lev list` | `--json`, `-f`, `--filter <all\|agents\|blueprints>` | List installed and bundled blueprints. See below |
| `lev add <PACKAGE>` | | Install a blueprint directory or `.leviath-bundle`. Prints what the package grants itself before installing |
| `lev remove <NAME>` | | Uninstall a blueprint |
| `lev pack [PATH]` | `-o`, `--output <FILE>` (default `{name}-{version}.leviath-bundle`) | Bundle a blueprint for [sharing](/docs/packaging) |

`lev list --filter` narrows the listing to installed agents or to bundled blueprints. An
unrecognized value is an error rather than a silent ignore. An agent declaring
[`[read_paths]`](/docs/security#reading-outside-the-workdir) also shows how many of its entries your
config grants.

## Watching and steering

| Command | Flags | Purpose |
|---|---|---|
| `lev ps` | `--json`, `--all` | List runs in the daemon with their status. `--all` also reads the runs dir. See [below](#reading-lev-ps) |
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
| `--feedback <TEXT>` | With `--deny`, what the model should do instead. It reads the text inside the refused call's tool result. An error with anything but `--deny` |
| `--stage` | With `--approve`, allow what this call runs until the run leaves the current stage |
| `--session` | With `--approve`, allow what this call runs for the rest of the run (alias `--run`) |

See [Human-in-the-loop](/docs/interaction) for what raises these.

### Reading `lev ps`

```
RUN                             TITLE                  STATUS                  STAGE         ITER   TOOLS  AGE  WORK  MOVED
solo-1785568852-9fa61fd279dd    Retry backoff audit    waiting: tool approval  work          1      1      12m  41s   41s
busy-1785568852-384bad04c9ac    Index the changelog    active                  work          13824  13824  12m   12m  0s
waiter-1785568852-7895a2209850  Split the log sweep    waiting: children(1)    delegate 1/2  2      1      12m  12m   41s

1 run needs an answer: lev respond
```

`TITLE` is the [generated one-line title](/docs/configuration#title), and the column appears only
when at least one listed run has one - a run whose titling was turned off or did not finish leaves
the cell empty rather than widening every row for nothing.

### Age, work, and moved

Three columns, because a run can look very different under each and the difference is
usually the thing you are trying to see. The first row above is the case: alive for twelve
minutes, at work for forty-one seconds of them, and holding a prompt open for the rest.

`AGE` is how long since the run was launched. It says nothing about whether the run has
done anything.

`WORK` is how long the run actually spent working. The clock runs while it is inferring,
calling tools, or held for its own fan-out workers and sub-agents, and stops for
everything that is not the run's doing: paused, blocked on a person, parked until the
machine is fixed, finished. This is the figure to call a run's duration - `AGE` counts the
overnight pause, and this does not. It is written to disk as `active` in `meta.json`, and
each stage keeps one of its own in `stages.json`.

`MOVED` is how long since the run last actually moved: a new iteration, a new stage, or a
change of status. It is deliberately not `meta.json`'s `updated_at`, which also advances
on a 30-second heartbeat so that observers can tell a live daemon from a dead one. A fresh
`updated_at` is therefore not evidence of progress; a fresh `MOVED` is. The same figure is
written to disk as `last_progress_at`, so a script can read it without the daemon.

> [!NOTE]
> `MOVED` was headed `AGE` before, and showed what `MOVED` shows now. If you have a script
> reading the table, read `lev ps --json` instead: every row there carries `started_at`,
> `last_progress_at` and `active` raw, plus `age_secs` and `working_secs` already computed
> - the same two keys the [HTTP API](/docs/api#how-long-a-run-has-taken) serves.

`lev ps` lists what the daemon is holding, plus the runs that finished within the
retention window above. `lev ps --all` adds a second block read from the runs dir instead,
so runs older than that window, and runs from before the last daemon restart, are still
accounted for:

```
NOT RUNNING
RUN                             STATUS               AGE  WORK  MOVED
coder-1785568100-a1b2c3d4e5f6   complete             1h   22m   4m
coder-1785567000-c3d4e5f6a1b2   error                3h   1m    1h
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
| `waiting` | Blocked. The reason follows the colon |
| `complete` | Finished |
| `cancelled` | Cancelled with `lev cancel` |
| `error` | Ended with the error shown |

A `waiting` run always says what it is blocked on, because the answer decides whether you
need to do anything. These are stopped until a person acts:

| Reason | What to do |
|---|---|
| `tool approval` | A tool call needs approving with `lev respond` |
| `user prompt` | The agent asked a question (`ask_user_*`). Answer it |
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
| `--no-remote-seed-commands` | off | Treat every spawn as `"no_seed_commands": true`, so a blueprint's command seeds never run for a run started over the API |
| `--max-concurrent-requests <N>` | `[serve]` key, else `64` | Requests in flight before the next is answered 503. `0` disables the cap. Websocket routes are not counted |
| `--request-timeout-secs <SECS>` | `[serve]` key, else `30` | Seconds a request may take before it is answered 408. `0` disables the deadline. Websocket routes are not timed |
| `--tls-cert <PATH>` | unset | PEM certificate chain. Serves HTTPS; needs `--tls-key` too |
| `--tls-key <PATH>` | unset | PEM private key for `--tls-cert` |

> [!TIP]
> A browser cannot call an `http://` Leviath that is not on loopback, whatever `--cors` says. That
> holds on a LAN too. That is what the TLS flags are for. See
> [reaching a Leviath on another machine](/docs/api#reaching-a-leviath-on-another-machine).

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
| `config` | `config.toml` parses and a provider registry can be built. The OK line also carries notes for a file that loads with problems in it: keys nothing reads, and `[model_providers.*]` script entries whose `.rhai` file is not on disk | The config file is malformed |
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
| `-m`, `--model <MODEL>` | Test a specific model. Takes the same forms as `lev run --model` |
| `--no-daemon` | Stop after the third check. Contacts no daemon, starts none, and creates no run |
| `--offline` | Stop after the second check. Proves the config parses and the model resolves, and bills nothing |
| `--json` | Print the checks as `{"checks": [...], "passed": bool}` |

`--model` takes `provider/model` to pick both, and a bare model id pairs with your
`default_provider`. `--model provider/model` is the way to reach a
[Rhai script provider](/docs/rhai-providers), which is resolved by name. Use it to try a model
string before wiring it into a blueprint: it goes further than `lev models list --provider`, which
compiles the provider and reads its catalog but never sends an inference.

`lev doctor` exits non-zero when a check fails, so it works as a CI gate. It bills two inferences
per run, each capped at 64 output tokens; `--no-daemon` bills one, and `--offline` none.

### `lev setup`

The interactive [provider](/docs/providers) wizard. Every credential and agent choice it asks for
has a flag, so headless setup is scriptable. The wizard's Limits screen edits the
[`[limits]`](/docs/configuration#limits) keys, which have no flags: script those by writing
`config.toml` directly. That screen is opt-in - every limit already has a working default, so it
only appears once you turn on **Show advanced tuning** on the Defaults screen. Skipping it changes
nothing about what gets written.

The Defaults screen leads with **Provider priority** - the order a bare model name prefers, whose
head is your default provider. Enter opens a modal to arrange it: drag a row by its `⠿` grip, or
move the one under the cursor with `Shift+↑`/`Shift+↓`. It writes the same
[`provider_order`](/docs/configuration#provider-preference-order) that `lev providers order` and
`PUT /api/config` set, so putting a subscription like Codex first there is how you route bare model
names onto your plan.

| Flag | Purpose |
|---|---|
| `--non-interactive` | Use only flag values, ask nothing |
| `--no-verify` | Skip checking credentials against the provider APIs |
| `--anthropic-key`, `--openai-key`, `--google-key`, `--openrouter-key <KEY>` | Provider API keys |
| `--ollama-url <URL>` | Ollama base URL |
| `--default-model <MODEL>` | Default model override |
| `--claude-code <true\|false>` | Enable the Claude Code CLI transport. Off unless set, and the wizard does not ask about it: this flag is the way to turn it on |
| `--claude-code-effort <LEVEL>` | `low`, `medium`, `high`, `xhigh`, or `max` |
| `--codex <true\|false>` | Enable the Codex transport, which bills a ChatGPT subscription. Flips the switch only: interactive `lev setup` signs in from its own screen, and a non-interactive run has nobody watching a browser, so sign in with `lev auth login codex` on that path |
| `--install-agents` | Install the bundled blueprints without asking |

```bash
lev setup --non-interactive --anthropic-key sk-ant-... --install-agents
```

The provider list ends with three entries for servers that speak OpenAI's chat API, and picking
any of them writes a [`kind = "openai-compatible"`](/docs/configuration#openai-compatible-endpoints)
entry rather than a key. **llama.cpp** and **LM Studio** are presets: each starts at its server's
default address (`http://localhost:8080/v1` and `http://localhost:1234/v1`) with no key, and is
written as `llama-cpp` or `lm-studio`. **Custom OpenAI-compatible endpoint** asks for a name, a
base URL, an optional key and optional headers (`Name: value`, several separated by semicolons).
All three repeat: the credential screen for a preset is a small form per endpoint with **Add
another** at the end and **Remove this endpoint** on each, so two llama.cpp servers on two ports
are two entries. **Check this endpoint** asks the server for its models; on success they are
listed and the **Default model** row cycles through them, and on failure the entry is kept and the
**Models** row takes the ids by hand, which is what the entry's `models` list is. Every endpoint
appears in the default-provider choice by its own name. These entries have no flags; script them
by writing `config.toml`.

> [!NOTE]
> The bundled agents are **not** installed unless `--install-agents` is passed in non-interactive
> mode. That is deliberate, so a scripted setup does not write blueprints you did not ask for.

Each blueprint is listed with what setup would do to it: install it, update it from the version on
disk, or nothing. A copy at the bundled version whose files differ from the bundled ones reads as
`edited locally` and is offered **unchecked**, because installing removes the destination directory
first and would take your edits with it.

`lev run` says the same thing at the moment it matters: a run starting on an installed bundled
blueprint that this build ships a different version of prints a one-line note before it spawns.

Setup remembers what you turned down. An MCP server you left unchecked, or a blueprint you chose
not to install, is still listed the next time you run `lev setup` - so you can change your mind -
but it is no longer pre-selected, and finishing the wizard again will not quietly bring it in.
Only refusals are remembered, and only from a run you finished: accepting needs no memory, because
the server lands in your config and the blueprint lands on disk. A blueprint's refusal is recorded
against the version that was offered, so a newer bundled version is a fresh offer and gets asked
about again rather than being hidden by an old "no thanks". This lives in `ui-state.json` under the
data directory, alongside what the dashboard remembers, and never in `config.toml`.

Inside the wizard, the keys work the same way on every screen:

| Key | Meaning |
|---|---|
| `↑` `↓` (or `k` `j`) | Move between rows |
| `←` `→` (or `h` `l`) | Cycle a choice or the reasoning effort |
| Space or Enter | Select the focused row; Enter also opens editors for typed values |
| Enter on a default | Opens a searchable list of providers or models, with what the choice decides |
| PgUp / PgDn, Home / End | Scroll a long screen; the selection moves with the view |
| Enter on `[ Continue ]` | Move to the next screen (the button is the last row) |
| Tab / Shift-Tab | Next / previous screen |
| Esc | Previous screen, or cancel an edit or dialog |
| `v` | Re-check a credential against the provider's API |
| `o` | Open the provider's signup page |
| Ctrl-R | Show or hide credentials |
| Ctrl-S | Write the config and finish, from anywhere |
| `?` or F1 | Help overlay. It scrolls, so a long list is not cut off |
| `q` / Ctrl-C | Quit without writing. If you changed anything, it asks first |

Nothing is written until you confirm on the Review screen. Leaving the provider screen with
nothing selected asks before letting you continue, since an agent cannot run without one.

### `lev integrate <HOST>`

Register Leviath as an MCP server in a host coding agent and install the skill that routes "use
leviath to ..." to it. `HOST` is one of `claude-code`, `grok`, `codex`, `gemini`, `hermes` or
`all` (every host whose dot-directory exists under your home). The full walkthrough, the files each
host gets and the long-run behaviour are in [Claude Code, Grok and other agents](/docs/host-agents).

```bash
lev integrate claude-code
lev integrate all --print          # show every file and command, write nothing
lev integrate claude-code --project
```

| Flag | Purpose |
|---|---|
| `--project` | Register for this project only: Claude Code writes `./.mcp.json` and `./.claude/skills`, Grok writes `./.grok/config.toml`. No effect for the other hosts |
| `--print` | Print what would be written or run, and touch nothing |
| `--no-skill` | Register the server without installing the `SKILL.md` |
| `--no-agents` | Do not install or update the bundled blueprints the server's default agent runs |

The command merges into an existing config with a real parser (JSON or TOML), so keys it did not
touch survive and running it twice changes nothing. For Claude Code it prefers `claude mcp add-json
--scope user` when the `claude` CLI is on `PATH`, and edits `~/.claude.json` (or
`$CLAUDE_CONFIG_DIR/.claude.json`) itself otherwise. Hermes only gets the `mcp_servers:` snippet
printed, since Leviath carries no YAML parser; paste it and run `/reload-mcp`.

It finishes with next steps: restart the host, then say "use leviath to <task>". If no provider is
configured it points at `lev setup`, and if `[limits]` sets no write ceiling it prints the
[two lines to add](/docs/configuration#limits), because an unattended run has no other byte limit.

### `lev providers`

Show the configured providers and set their **priority order** - the
[`[providers] provider_order`](/docs/configuration#provider-preference-order) that decides which
provider serves a bare model name (one a blueprint lists with no provider) when more than one
serves it.

| Command | Options | Purpose |
|---|---|---|
| `lev providers` (or `lev providers list`) | `--json` | List configured providers and the current priority order |
| `lev providers order <NAME>...` | | Set the order, best first (e.g. `lev providers order codex openrouter openai`) |
| `lev providers order --clear` | | Remove the order, so `default_provider` alone decides |

Naming a provider in the order is also how a subscription transport (Codex, Claude Code) becomes
eligible for a bare model name - it is otherwise reachable only by an explicit `provider/model`, so
that enabling it never silently moves billing. A name that is not a configured provider is refused
rather than written, since it could never win a route.

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

### `lev mcp serve`

Serve Leviath itself as an MCP server over stdio, so a host agent (Claude Code, Grok, Codex,
Gemini, Hermes) hands a task to Leviath with a tool call instead of hunting for the `lev` binary.
You rarely type this yourself: `lev integrate <host>` registers the command in the host's MCP
configuration under the server name `leviath` and installs a skill that tells the host when to
reach for it. The tools the server exposes, and how a host session uses them, are on
[Claude Code, Grok and other agents](/docs/host-agents).

| Flag | Purpose |
|---|---|
| `--attended` | Runs the host starts ask before effectful tool calls, as a plain `lev run` does. Without it they run as `--yolo`, since a host session usually has nobody watching the Leviath side |
| `--allow <TOOL>` | Allow one tool outright on every run the host starts. Repeatable |
| `--default-agent <NAME>` | The agent a `run` call gets when it names none. Defaults to `orchestrator` |
| `--workdir <DIR>` | The working directory for a `run` call that passes none. Defaults to `CLAUDE_PROJECT_DIR` when the host sets it, else the directory the server was started in |

The wire format is newline-delimited JSON-RPC 2.0 on stdin and stdout, one object per line, so
stdout belongs to the protocol and everything the server has to say for itself goes to stderr. The
daemon is started by the first call that needs one; `initialize`, `list_agents`, `list_tools`, and
`install_tool` never do.

A host that stops waiting on a tool call, because its own timeout fired or the user cancelled,
only stops waiting. The Leviath run continues in the daemon, and the host finds it again with
`list_runs`, then `wait`, `status`, or `cancel`. The `cancel` tool is the one way a host ends a
run.

### `lev auth`

| Command | Flags | Purpose |
|---|---|---|
| `lev auth status` | | Which credential backend is in use and what it holds |
| `lev auth login <provider>` | | Sign in with a browser (`codex`); stores the grant outside `config.toml`. `lev setup` does this on its own screen, so this is for headless machines and revoked sessions |
| `lev auth logout <provider>` | | Forget a browser sign-in, leaving the provider enabled |
| `lev auth migrate` | `--to-file`, `--dry-run` | Move secrets between `config.toml` and the OS keychain |

`lev auth migrate` moves keys into the OS store by default; `--to-file` moves them back out. Set
`[security] credential_store` in the [config](/docs/configuration#security) first.

### `lev update`

Update Leviath, then offer to bring everything else up to date with it: the binary, the bundled
blueprints, and the config file, in that order.

The binary is updated with the installer that put it there, and which one that was is read off the
filesystem rather than guessed from the version string. The version cannot answer: every
[channel](/docs/releases) ships the same number, because the `-alpha` and `-beta` suffixes live in
the tap manifests and not in the binary. Where the file sits does answer.

| Found at | What it runs |
|---|---|
| A Homebrew Cellar path, or a Homebrew-only prefix | `brew upgrade <formula>` |
| `scoop/apps/<package>` or a scoop shim | `scoop update <package>` |
| `~/.cargo/bin` | Nothing. It says to run `cargo install leviath-cli` |
| `/usr/local/bin`, `/usr/bin`, `~/.local/bin`, `%LOCALAPPDATA%\Leviath\bin` | `curl -fsSL https://leviath.dev/install.sh \| sh -s -- --channel <CHANNEL>` |
| Anywhere else | Nothing. It names the path and leaves the choice to you |

A Cellar or `apps` path carries the package name, and the package name carries the channel, so a
beta install updates to beta without being told. The install script records nothing at all, so its
channel is genuinely unknowable: that arm defaults to `stable` and `--channel` is how you say
otherwise.

A `cargo install` is described rather than run, because updating it is a full compile and that is
not something to start because somebody typed `lev update`.

| Flag | Purpose |
|---|---|
| `--check` | Print the plan and change nothing |
| `--json` | Print the plan as JSON and change nothing |
| `--channel <stable\|beta\|alpha>` | The channel to re-install. Only the install-script method reads it |
| `--dry-run` | Walk the whole flow, prompts and all, printing each action instead of doing it |
| `--yes` | Answer yes to the binary upgrade and the config write. It does **not** install blueprints |
| `--install-agents` | Install the bundled blueprints without asking |

```bash
$ lev update --check

lev 0.3.5, installed with Homebrew (formula leviath-beta, beta channel)

  binary   brew update && brew upgrade leviath-beta
  agents   1 of 7 would change
             data-analyst - update 0.0.1 → 0.0.2
  config   nothing to migrate
```

All three steps run every time, whatever the binary step did. That is the point of the command:
`brew upgrade` and `scoop update` hand you a new binary and say nothing about the blueprints in
`~/.leviath/agents` or the config beside them, so anyone who has ever updated that way is running
blueprints from whenever they last ran `lev setup`. A binary that needs no update is not evidence
that anything else is current.

The blueprint step is the same offer `lev setup` makes, and nothing is written to your agents
directory without a yes. The whole list is printed first, then one confirmation covers it;
`--install-agents` is how a script says yes. `--yes` alone is deliberately not enough, because
updating a binary and replacing the blueprints in your agents directory are different requests.

A copy at the bundled version whose files differ from the bundled ones reads as edited locally. It
is named as edited, asked about on its own, and no flag covers it: installing removes the
destination directory first and would take your edits, and any file you added, with it.

The config step applies any migration this build knows how to make, printing every change before it
asks to write anything. Today there are none: no released `config.toml` has to change to work with
this version, so the step exists to explain a future one rather than to do work now.

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
