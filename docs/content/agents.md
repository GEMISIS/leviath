---
title: Agent blueprints
description: What an agent.toml blueprint holds and why: stages, models, tools, regions, inputs, seeds, and the checks to run first.
group: Concepts
group_order: 2
order: 5
---

# Agent blueprints (`agent.toml`)

A **blueprint** is a multi-stage [workflow graph](/docs/stages) you write once and run many times.
It is a directory holding an `agent.toml` file and the blueprint's own tools and scripts. Each
`lev run` of it is a **run**: one execution, with its own id and its own memory. The
[agent catalog](/docs/agent-catalog) has seven complete blueprints worth stealing from.

New to this? [Build your first agent](/docs/first-agent) walks through writing one stage by
stage. This page explains what each part of a blueprint is for. The
[blueprint format](/docs/blueprint-format) lists every key.

Start from a scaffold rather than a blank file:

```bash
lev create my-agent
cd my-agent
lev run . --task "Your task here"
```

A blueprint needs very little to run: a name, a version, a layout, and one stage with a prompt.
Everything else on this page is opt-in from there. A fuller one looks like this:

```toml
[blueprint]
name = "coder"
version = "0.2.0"
description = "Analyze, implement, and review with graph-based recovery"

[graph]
entry = "analyze"

[graph.tool_permissions]          # defaults for every stage; a stage may override them
read_file  = "allow"
write_file = "ask"
bash       = "ask"

[[graph.inputs]]                  # what each run is given: here, the task
name = "task"
type = { kind = "text", multiline = true }
required = true
binds = [{ region = "task" }]

[graph.layout]                    # the run's memory, as named regions
total_budget_tokens = 0

[[graph.layout.regions]]
name = "task"
kind = "pinned"
budget = "2%"

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 30 }
budget = "40%"

[[graph.stages]]
name = "analyze"
model = { models = [{ model = "claude-sonnet-5" }, { model = "gpt-5.4-mini" }] }
tools = ["read_file", "list_dir"]
required_tools = []               # human-in-the-loop tools kept in an unattended run
max_iterations = 15
system_prompt = "Understand the task and produce a short implementation plan."

[[graph.stages]]
name = "implement"
model = { models = [{ model = "gpt-5.5" }, { model = "claude-opus-5" }] }
tools = ["read_file", "write_file", "bash"]
max_iterations = 40
system_prompt = "Carry out the plan. Run the tests after each change."

[[graph.edges]]
name = "implement"
from = "analyze"
to = "implement"
hint = "Plan ready, begin implementation"
```

The file has two tables. `[blueprint]` names and versions it. `[graph]` is the run graph itself:
stages, the edges between them, the layout of the run's memory, and the inputs a run takes. A
[raw spawn request](/docs/starting-a-run#blueprint-or-raw-graph) carries the same graph, so what
you learn here applies there too.

`lev blueprint migrate <dir>` converts an `agent.leviath` manifest into this form. See
[migrating from agent.leviath](/docs/blueprint-format#migrating-from-agentleviath).

## Inputs: what each run is given

A blueprint is written once, and each run needs its own task, its own diff, its own deadline.
Those per-run values are **inputs**. You declare each one in `[[graph.inputs]]` with a type, and
bind it to the place it goes, usually a region:

```bash
lev run coder --task "Add a --json flag to lev ps" --input constraints="No new dependencies"
```

By convention every blueprint declares a plain `text` input named `task`, usually bound to a
region of the same name. That is why `--task` works everywhere. Any other input is `--input name=value`, or the short
form `--<name> value`. A run that leaves out a required input, gives one the wrong type, or names
one the blueprint does not declare is refused before it starts, with every problem listed at once.
[Starting a run](/docs/starting-a-run#inputs) covers the types and where an input can go.

## The run loop

Within a stage, a run goes round a tight loop (infer, act on tool calls, repeat) until the model
signals it's done or an [edge](/docs/stages) fires:

```mermaid
flowchart LR
  I["Infer<br/>(stage model)"] --> T{"Tool calls?"}
  T -->|yes| X["Execute tools<br/>route output to regions"]
  X --> I
  T -->|no| D{"Transition?"}
  D -->|hint / error / stuck| N["Next stage"]
  D -->|none, done| E["Finish"]
  N --> I
```

## Lifecycle

A run moves through a handful of states the [dashboard](/docs/dashboard) and [API](/docs/api)
report on:

```mermaid
stateDiagram-v2
  [*] --> Starting
  Starting --> Running
  Running --> WaitingInput: prompt, or holding for children
  WaitingInput --> Running: answered, or children done
  Running --> Paused: lev pause
  Paused --> Running: lev resume
  Running --> Complete
  Running --> CompleteInteractive: done, still accepting messages
  Running --> Error: unrecoverable error
  Running --> Cancelled: lev cancel
  Complete --> [*]
  CompleteInteractive --> [*]
  Error --> [*]
  Cancelled --> [*]
```

These are the exact `RunStatus` values the [dashboard](/docs/dashboard) and [API](/docs/api)
report. `CompleteInteractive` means every required stage finished but the run is still
accepting [messages](/docs/interaction).

`WaitingInput` covers two very different situations: a run stopped on a prompt somebody has
to answer, and a run parked while its own [sub-agents](/docs/sub-agents) or
[fan-out](/docs/stages) workers get on with it. The second needs nothing from you.
[`lev ps`](/docs/cli#reading-lev-ps) tells them apart, so reach for it before concluding a
run is stuck.

## Stages and models

Each stage gets its own **model** (an ordered list of models, best first: the first one a configured
provider serves wins), tools, iteration cap, and context layout. Edges (`[[graph.edges]]`) form a
[graph](/docs/stages): each names the stage it leaves and the stage it reaches, and can fire on
conditions like `error` and `stuck`.

```toml
[[graph.stages]]
name = "analyze"
system_prompt = "Understand the task."

[graph.stages.model]
allow_user_default = true          # let the host's override_model and fallback_model apply;
                                   # false keeps this list exactly as written
models = [
    { model = "claude-sonnet-5" }, # name models, not routes: whichever provider
    { model = "gpt-5.4-mini" },    # the user configured is asked which it serves.
                                   # Pin one only for a model a single route can
                                   # reach: { provider = "ollama", model = "..." }
]
request_timeout_secs = 120         # per-stage inference wall-clock cap

[graph.stages.model.params]
temperature = 0.2
max_output_tokens = "40%"          # see below
extra = { reasoning_effort = "high" }   # passed to the provider as written
```

`temperature` and `max_output_tokens` are the two settings Leviath knows. Anything else goes in
`extra` and reaches the provider untouched. `max_output_tokens` is the most one reply may
contain. Three forms:

| Form | Meaning |
|---|---|
| `max_output_tokens = 8000` | a fixed number of tokens, sent as written |
| `max_output_tokens = "40%"` | that share of the model's context window |
| `max_output_tokens = "100% of claims"` | that share of the `claims` region's budget, for a stage whose reply fills a region |

A relative cap is resolved when each request is built, against whichever model the stage landed on, and is never
more than that model's own maximum. A cap the loader cannot read fails the load, because a limit
that silently becomes "no limit" is the kind of typo that only shows up as a bill.

Prefer a relative cap for a stage that writes something whose size follows the material (a report,
a rewrite of a file). A fixed number is easy to set smaller than the thing being written, and a
reply cut off by its cap is not an answer. The runtime sends it back with the reason and retries
once at the model's maximum, but the first attempt is still paid for.

A tool call cut off halfway through its arguments is not run. The model is shown what arrived and
told how to split the call. For `write_file` that means writing the first part, then adding each
later part with `"append": true`. From the second cut-off in a row the model is also told not to
resend the call and how many tries it has left. If a fourth reply in a row is cut off in a tool call,
the stage ends with an error. It takes the stage's `error` edge when there is one, and the run fails
when there is not. Replies that are not cut off, however many tool calls they make, start the count
again.

Model selection is per stage, and only per stage. A top-level `[model]` table is refused as an
unknown key. A stage naming no model takes the host default without saying so, and
`lev validate` warns about it. See
[every stage should name its own model](/docs/stages#every-stage-should-name-its-own-model).

### Which tools a stage gets

A stage's `tools` lists what it may call, by name or by kind. `@builtin`, `@subagent`,
`@scripts`, `@mcp` and `@all` each grant every tool of that kind, so `["@builtin", "@scripts"]`
is every built-in and every Rhai tool with nothing to keep in step. See
[tool groups](/docs/tools#tool-groups) for what each reaches and what none of them grant.

`required_tools` is the exception to the unattended cut. A [`--yolo`](/docs/glossary) run drops
every tool that waits on a person, and this is where a stage names the ones it wants kept anyway.
Every entry must also appear in `tools`, by name or through a group that reaches it;
`lev validate` checks the group case against the tools this install has.

Naming a tool here also settles the `blocking-tool-in-autonomous-stage` lint for it, since listing
it is how you say you meant it. See
[human-in-the-loop tools](/docs/tools#these-tools-need-someone-there).

#### Naming a tool from an MCP server

An MCP tool is always named `<server>__<tool>`: the server it came from, two underscores, then
the tool the server calls it:

```toml
tools = ["read_file", "tracker__create_issue"]
```

The server is part of the name whether or not anything would have collided. Two servers that both
offer `search` are `tracker__search` and `wiki__search`, and a grant means the same thing however
your `config.toml` is ordered.

The separator is `__` rather than a dot because the name is passed to the model provider, and
providers only accept `[A-Za-z0-9_-]`. A dot anywhere in a server or tool name is rewritten to `_`
for the same reason, so a server called `my.tools` offering `find.all` is advertised as
`my_tools__find_all`.

To grant a server's whole set instead of naming its tools one at a time, see
[granting a whole server](/docs/mcp#granting-a-whole-server).

## Context regions

A **region** is a named part of a run's memory. `[graph.layout]` lists the regions every stage
sees, in the order the model reads them. Each one has a `name`, a `kind` and a `budget`. There
are nine region kinds; see [Structured context](/docs/context) for what each one does. Budgets
come in three forms:

```toml
[graph.layout]
total_budget_tokens = 0    # matters only when every budget is a fixed number

[[graph.layout.regions]]
name = "codebase"
kind = "compacting"
budget = { percent = "35%", min = 4000, max = 60000 }
                           # a share of the model's context window, held between
                           # an absolute floor and an absolute ceiling

[[graph.layout.regions]]
name = "notes"
kind = "clearable"
budget = "10%"             # a share of the window with no clamp

[[graph.layout.regions]]
name = "task"
kind = "pinned"
budget = 2000              # a bare number is a fixed budget in tokens
```

Percentages are **ceilings, not allocations**. They may sum past 100%, because regions rarely all
fill at once. In the table form, `max` caps and `min` floors the resolved value. A compacting
region also takes `compact_at`, the share of its budget that triggers compaction, or
`kind = { kind = "compacting", threshold_tokens = 8000 }` for a fixed fill level. The
[blueprint format](/docs/blueprint-format#layout-and-regions) lists every region key.

### Say how much a region moves

```toml
[[graph.layout.regions]]
name = "task"
kind = "pinned"
budget = 2000
volatility = "stable"      # filled once at spawn, never written again

[[graph.layout.regions]]
name = "findings"
kind = "pinned"
budget = "10%"
volatility = "grows"       # the run appends to it

[[graph.layout.regions]]
name = "plan"
kind = "pinned"
budget = "5%"
volatility = "rewritten"   # revised in place each time; the default
```

Providers cache the prompt by prefix, so a region that changes invalidates the cache for every
region assembled behind it. `volatility` is what orders them: `stable` first, `grows` next and
split so its settled part still caches, `rewritten` last where it invalidates only itself.

The kind cannot answer this, which is why the setting exists. Every region above is `pinned`.
That means "never evicted", not "never written", and `context_write` into a findings region is an
ordinary move. Only the blueprint knows which is which.

Leaving it out is safe: an unclassified region is assumed to change and placed last, so declaring
can only improve matters. A region that claims `stable` and then keeps changing is named in the
log, because a wrong declaration is worse than none. It puts churn at the front of the prompt,
where it costs the most. See [what caching costs](/docs/context#what-caching-costs).

A stage can replace the whole layout for itself alone with its own `layout`. The per-stage layout
applies when the stage is entered, and uses the same keys:

```toml
[[graph.stages]]
name = "plan"
system_prompt = "Plan."

[graph.stages.layout]
total_budget_tokens = 0
regions = [
    { name = "constraints", kind = "pinned", budget = "10%" },
]
```

**A region the stage leaves out is hidden, not destroyed.** It keeps its contents and is left out
of that stage's prompt. It comes back with everything in it as soon as a later stage declares it
again. That is what makes this usable for narrowing: a compute stage need not carry a large data
preview through every one of its calls, and a summary stage further on can still read it.

`conversation`, `tool_results` and `final_output` are always visible, whatever a stage declares.
The first two hold the typed tool-call turns the next stage's own turns attach to, and an answer
submitted early has to survive to the end.

Re-declaring a layout is the heavy form. When a stage only needs to leave one or two regions out,
name them instead:

```toml
[[graph.stages]]
name = "polish"
system_prompt = "Polish."
hide = ["sources"]        # everything else is carried exactly as the graph's layout says
```

`hide` is the right tool for the common case: a region of raw material (fetched pages, tool
output) that an early stage fills and a late stage never reads. Left in, it is re-sent on every
call of every later stage; a report-polishing stage in the bundled deep-researcher was carrying
125,000 tokens of sources it had no instruction to look at. A name that matches no region fails
the load, and the always-visible regions above cannot be hidden. The hidden set is decided afresh
by each stage: a stage that declares neither its own `layout` nor `hide` carries everything.

## Seed commands

A region can be filled before the run starts. An input bound to the region fills it from the
caller. A `seed` fills it from somewhere the blueprint names: files, a glob, literal text, a
script, a list of tool calls, or a shell command:

```toml
[[graph.layout.regions]]
name = "codebase"
kind = "compacting"
budget = { percent = "35%", min = 4000, max = 60000 }
seed = { command = "git ls-files" }
```

Command seeds run at spawn **before any approval prompt**, confined to the workdir and routed through the
entry stage's sandbox, time- and size-capped.

> [!WARNING]
> A seed command runs a shell command before you approve anything, so it must be covered by
> [`[safe_commands]`](/docs/interaction#what-runs-without-asking) to run at all. `lev validate`
> prints every seed a blueprint will run; review them for third-party blueprints. Refuse with
> `--no-seed-commands`, `launch.seed_commands` in a [spawn request](/docs/starting-a-run#launch-policy),
> or `[security] allow_seed_commands = false`.

## Read paths

A blueprint whose runs need to *read* beyond their workdir, for run archives, design docs, or
sibling directories, declares the paths:

```toml
[graph]
read_paths = ["~/.leviath/runs", "../shared-docs", "glob:~/design-docs/**"]
```

The declarations do nothing on their own: the user's config must grant them, they are
read-only, and every access is checked against the symlink-resolved real path. Run
`lev validate` to see which of them the config on this machine actually grants. See
[Security](/docs/security) for the grant stanzas and the full matching rules.

## Mime types the blueprint brings

A blueprint whose tools make or take a format nothing else knows can describe it itself:

```toml
[graph.mime_types."application/x-acme-scene"]
family = "model"
extensions = ["scene"]
magic = "41434D45"
check = { file = "checks/scene.rhai" }   # relative to this directory; refuses bytes that are not a scene
```

The rows are the same shape as the operator's
[`mime_types.toml`](/docs/configuration#mime_typestoml) and layer over it for this blueprint's
runs only, so a blueprint travels with the types it needs and never changes what another run
sees. They are checked when the blueprint is read, so a misspelled field fails `lev validate`
and the spawn. A `check` script is compiled beside the blueprint's other scripts with the same
fence: it has to live inside the blueprint's directory. [Mime](/docs/mime#the-registry) has
every field and [Rhai mime checks](/docs/rhai-mime-checks) the script.

## Dependencies

A blueprint can say what has to be in place on the machine before it runs, as a
`[[graph.dependencies]]` array. This is a declaration, never a grant: Leviath shows the operator
what is missing and how to fix it, and a run whose required dependency is unmet fails to spawn
before any model is billed. `lev deps check <agent>` reports the same findings, and
`lev deps install <agent>` sets them up after asking.

Each entry has a `name`, a `needs`, an optional `required` (true by default), a human `remedy`
shown when it is missing, an optional `description`, and an optional `install`. `needs` chooses
what must be present:

```toml
# An MCP server that must be configured, plus the secret it needs.
[[graph.dependencies]]
name = "meshy"
needs = { mcp_server = { server = "meshy", env = ["MESHY_API_KEY"] } }
remedy = "Run: lev deps install my-agent, then set MESHY_API_KEY"

# What lev deps install writes into the user's config for that server. Secrets
# are never stored here: they are named in env above and prompted for.
[graph.dependencies.install.server]
transport = "http"
url = "https://www.meshy.ai/mcp"
headers = { Authorization = "Bearer ${MESHY_API_KEY}" }

# A program that must be on PATH, with how to install it.
[[graph.dependencies]]
name = "blender"
needs = { binary = "blender" }

[graph.dependencies.install]
command = "brew install blender"                       # or per-OS:
commands = { linux = "apt-get install -y blender" }

# An environment variable that must be set and non-empty.
[[graph.dependencies]]
name = "token"
needs = { env = "ACME_TOKEN" }

# A condition a Rhai script decides.
[[graph.dependencies]]
name = "acme-setup"
needs = { check = { file = "deps/check.rhai" } }       # returns () when satisfied, else a remedy string
install = { script = { file = "deps/install.rhai" } }   # optional; runs only via lev deps install
```

A `check` script runs on a hardened engine with three read-only probes and nothing else:
`has_env(name)`, `env(name)` and `path_exists(path)`. It returns `()` when the dependency is in
place, or a string remedy when it is not. An `install` script gets one host function, `sh(command)`,
and runs only when the user asks for it with `lev deps install`. Checking never changes the machine;
only install does, and only after a confirmation.

The bundled `sprite-to-3d` blueprint declares the Meshy dependency above: it turns a sprite sheet or
character image into a rigged, game-ready model, and a run refuses to start until Meshy is set up.
Set it up with `lev deps install sprite-to-3d`, then run it with `lev run sprite-to-3d`.

It works in stages. First it renders a clean front T-pose from the sprite, then back and side views
that match it, drawing more than one where it is unsure. A filter stage deletes the bad renders
(off-model, pixel-art, or a multi-angle turnaround sheet) and keeps the good single views. A critique
stage, on a second model chosen for a sharp eye, then compares each kept view against the source
part by part. It records anything missing or wrong into a dedicated region, such as an absent arm
cannon or a shoulder pad on the wrong side. It is the only stage that clears an item, and only
after confirming on the images that the detail is now there. A coverage stage decides whether the
angles are covered and that list is clear. If not, a generation pass redraws the views to fix
exactly those items, conditioned on both the sprite sheet and the good views already in hand. The
whole filter to critique to generate loop is bounded. Meshy then builds one model from the chosen
views, with
symmetry turned off so a one-sided detail like an arm cannon survives instead of being mirrored away.
A final stage compares the model against the views and, if it drifted, sends it back to be rebuilt a
bounded number of times before finishing.

## How the coding agent verifies its work

A run of the bundled `coder` blueprint decides what "done" means before it starts, rather than
judging it at the end. Its entry stage is `discover`: before planning anything, the run classifies the
project's testing story and writes a `workflow` region ending in three literal lines that later
stages execute verbatim:

```text
BASELINE: <command to run BEFORE any edit>
VERIFY: <command to re-run after each change>
DONE WHEN: <the completion bar, including "no regressions vs baseline">
```

The baseline is captured before the first edit, so "a test that was already failing" and "a test
I broke" are distinguishable. Each change re-runs VERIFY and compares against the baseline, and
the run is only done when DONE WHEN holds, not when "most tests pass". Regions that carry this
state are marked `required = true`; if one is empty when a stage needs it, the workflow routes
back through discovery instead of guessing. Projects with no tests at all are handled explicitly:
the plan must include *building* verification (a smoke test to write and run), stated plainly
rather than invented.

## Tracking files a run touches

`[graph.file_tracking]` keeps a running list of what the run has read and written, in a region
you name, so a later stage knows what has already been looked at.

```toml
[graph.file_tracking]
region          = "files"    # the region the list is kept in
track_reads     = true       # default true
track_writes    = true       # default true
max_file_tokens = 4000       # cap on how much of one file is tracked
```

## Catching a run going in circles

`[graph.repetition]` watches for a run making the same call over and over, or reading without
ever writing. When it sees one, it writes a `[System]` note into the run's conversation telling
the model what it is doing and to try something else.

It nudges, it does not intervene. The run keeps going either way, the stage does not fail, and no
edge fires. If you want a loop like this to actually route somewhere, use a `stuck` edge in
[stages](/docs/stages#stuck-detection). The two work well together: the nudge gives the model a
chance to correct itself, and the edge catches it if it does not.

```toml
[graph.repetition]
enabled             = true   # default
max_repeat_calls    = 3      # default; identical tool call, back to back
max_readonly_streak = 10     # default; read-only calls with no modification in between
```

## Who does the summarizing

A [`compacting` region](/docs/context) summarizes rather than evicting, and something has to write
that summary. By default it is `claude-sonnet-4-6` on Anthropic, whatever the stage itself runs on,
because a summary is cheap work that does not need the stage's model. `[graph.compaction]`
replaces that default, and then names all three of `model`, `max_summary_tokens` and
`temperature`:

```toml
[graph.compaction]
model              = { provider = "openai", model = "gpt-5.4-mini" }
max_summary_tokens = 2000
temperature        = 0.2
system_prompt      = "..."   # optional; replaces the built-in summarizer prompt
```

Point it at a provider you have configured if you do not use Anthropic. A run whose compaction
provider is not registered loses compaction rather than failing, so a blueprint with no
`[graph.compaction]` on an OpenAI-only machine quietly stops summarizing. `lev doctor` reports
which providers are registered.

## Discovering tools mid-run

By default a stage advertises a fixed tool set resolved at spawn, and a tool that appears later is
invisible to it. `tool_rescan` says when a run looks again:

```toml
[graph]
tool_rescan = "after_writes"   # at_spawn | after_writes | before_dispatch
```

| Value | When it looks | What that catches |
|---|---|---|
| `at_spawn` | Once, at spawn | The default. A run cannot grow its own capabilities |
| `after_writes` | Before the next turn, when the run writes a script | A tool this run installed, from its next turn on |
| `before_dispatch` | Also at the directories, before each batch | A tool that arrived without this run writing it |

Anything but `at_spawn` puts the run's own `tools/` directory in the scan set. The directory is the
workdir's, so anything else running there sees the same tools and a sub-agent inherits it.

The difference between the last two is what they notice. `after_writes` is told about a tool when
the run writes one with `write_file` or `edit_file`, or installs one. A tool that arrives any
other way sets nothing, and stays invisible for the rest of the run. That covers one written by a
shell command or a script tool, one dropped in by a sub-agent or fan-out worker sharing the workdir,
or one a person adds while the run is going. `before_dispatch` looks at the directories rather than
waiting to be told, so it sees those, and sees a tool that was edited or removed too. The cost is
one `stat` per scanned directory per batch, and a re-scan only when something changed.

Neither value makes a tool callable in the batch that creates it. Every call in a batch is checked
before any of them runs, so a batch that writes a script and calls it has the call refused either
way.

## Handing context to a sub-agent

`[[graph.transforms]]` maps one blueprint's regions onto another's when a parent run spawns a
child run, so the child starts with the parent's findings under its own region names.

```toml
[[graph.transforms]]
from = "researcher"
to   = "reviewer"
mappings = [
    { from = "findings", to = "source_material" },                     # transform = "direct", the default
    { from = "conversation", to = "brief", transform = "summarize" },
    { from = "notes", to = "facts", transform = { extract = ["claim", "source"] } },
]
```

`direct` copies the region as it is, `summarize` condenses it, and `extract` pulls the named fields
out of JSON content. See [Sub-agents](/docs/sub-agents).

## Counts are never negative

Every count a blueprint carries (`max_iterations`, `max_items`, a fixed `budget`,
`max_child_depth`, `request_timeout_secs`, a gate's `max_attempts`, a `stuck` threshold, and the
rest) is a whole number, zero or more. A negative value fails the read, and the error points at
it:

```text
kind = { kind = "sliding_window", max_items = -1 }
                                              ^^
invalid value: integer `-1`, expected u32
```

Zero keeps whatever meaning the key gives it (`max_iterations = 0` is unlimited, a gate's
`max_attempts = 0` never holds, a `stuck` threshold of zero is unset).

## Validate before you run

```bash
lev validate .                    # check the graph, and what the blueprint leaves unsaid
lev validate . --deny-warnings    # for CI: warnings fail too
lev run . --task "..." --check    # resolve one run the whole way without starting it
lev test .                        # run the blueprint's tests/ cases (real API calls)
lev test . --dry-run              # parse and report them without calling a provider
```

`lev validate <dir>` reads the blueprint the way a spawn would and lists every problem at once,
each with its path, such as `graph.stages.polish.hide`. It reports a stage, region or input that
names something undeclared, and a setting out of range.

Beyond the graph, `lev validate` reports the fields whose absence quietly changes what a run does.
That covers a stage with no model, a tool name that matches nothing, and an autonomous stage
offering a tool that waits for a person. Errors exit non-zero, warnings do not, notes never can. The
[CLI reference](/docs/cli#lev-validate-path) lists every check. The dashboard's blueprint editor
reports the same findings as you edit.

`lev run --check` goes one step further. It takes the blueprint together with this run's inputs
and this machine's providers, and reports which model each stage would get, or every problem it
found. See [check before you start](/docs/starting-a-run#check-before-you-start).
