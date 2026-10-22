---
title: Writing a blueprint
description: The agent.toml format section by section, with its short forms, a complete example, and how to convert an agent.leviath.
group: Reference
group_order: 3
order: 6
---

# Writing a blueprint

A run needs a lot of decisions written down. Which stages does it have, and which model and tools
does each one get? How is its memory laid out, and what does a caller fill in? A **blueprint**
keeps those decisions in one file, `agent.toml`, so every run of it starts from the same place.

The file has two tables. `[blueprint]` says what the blueprint is called. `[graph]` is the run
itself: its stages, the edges between them, the regions of its memory, and the inputs it takes.
That graph is the same structure a [spawn request](/docs/starting-a-run) carries when it sends a
whole graph, so anything you learn here works there too.

## A complete blueprint

This blueprint reads the commits since a tag and writes release notes. It has two stages, three
typed inputs, four regions and one edge.

```toml
[blueprint]
name = "release-notes"
version = "0.1.0"
description = "Reads the commits since a tag and writes release notes."

[graph]
entry = "gather"
tool_permissions = { shell = "ask", read_file = "allow" }

# What a caller fills in.
[[graph.inputs]]
name = "since"
type = "text"
required = true
description = "The tag or commit to start from."
binds = [{ region = "brief", template = "Changes since {since}, for {audience}. At most {max_items} items." }]

[[graph.inputs]]
name = "audience"
type = { kind = "choice", options = ["users", "developers"] }
default = { choice = "users" }

[[graph.inputs]]
name = "max_items"
type = { kind = "int", min = 1, max = 50 }
default = { int = 20 }

# The regions a run's context is made of.
[graph.layout]
total_budget_tokens = 0

[[graph.layout.regions]]
name = "brief"
kind = "pinned"
budget = "2%"
required = true

[[graph.layout.regions]]
name = "commits"
kind = "pinned"
budget = "20%"
seed = { command = "git log --oneline -200" }

[[graph.layout.regions]]
name = "notes"
kind = "clearable"
budget = "10%"

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 30 }
budget = "40%"

# The stages, in order.
[[graph.stages]]
name = "gather"
system_prompt = """
Read `brief` and `commits`. Group the changes that matter to the audience,
read the code with read_file where a commit message is unclear, and append
one line per change to `notes` with context_append."""
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5-5" }, { provider = "openai", model = "gpt-5.4-mini" }] }
tools = ["read_file", "shell", "context_append"]
max_iterations = 12

[[graph.stages]]
name = "write"
system_prompt = "Turn `notes` into release notes, most important first."
model = { models = [{ provider = "openai", model = "gpt-5.5" }] }
output = { format = "markdown" }

# How a run moves between them.
[[graph.edges]]
name = "write"
from = "gather"
to = "write"
```

Save it as `release-notes/agent.toml` and check it:

```text
$ lev validate release-notes
✓ Blueprint 'release-notes' is valid.
  2 stages, version 0.1.0
  Inputs: --since (text, required, seeds region 'brief'), --audience (one of "users", "developers"), --max_items (an integer 1 to 50)
  Note: this agent takes no --task; give it input via --since, --audience, --max_items
  Entry stage: 'gather'
  - gather → write
  - write (terminal)
```

After that come the lint's findings, if any: a stage left without a `max_iterations`, a tool name
that matches nothing, a seed command that runs at spawn. [`lev validate`](/docs/cli#lev-validate-path)
lists every one.

`lev add ./release-notes` installs it, and `lev run release-notes --input since=v0.6.0` runs it.
[Starting a run](/docs/starting-a-run) covers everything a run's request can say.

Every table refuses keys it does not know. A misspelled key is an error that names the line, never
a setting that is quietly ignored. Every reference to a stage, region or input is checked too:

```text
$ lev validate release-notes
Error: ✗ Validation failed: release-notes/agent.toml has 3 problem(s):
  graph.edges[0].to: dangling reference: no stage is named "wirte". Known: gather, write
  graph.inputs.since.binds[0].region: dangling reference: no region is named "brief". Known: brif, commits, conversation, notes
  graph.inputs.max_items.default: out of range: 99 is out of range (expected an integer 1 to 50; got the integer 99)
```

## `[blueprint]`

| Key | Required | What it is |
|---|---|---|
| `name` | yes | The name it is installed and run by. |
| `version` | yes | Its version. Bump it whenever the graph changes. |
| `description` | no | What it does, in a sentence or two. |

The name and description also become the run's title and description, unless `[graph]` sets its
own `title` and `description`. A blueprint's files sit in the same directory as `agent.toml`, and
paths to scripts, prompts and seed files are read relative to it.

## `[graph]`

The graph-wide settings sit directly under `[graph]`. Everything has a default except `stages` and
`layout`.

| Key | Default | What it does |
|---|---|---|
| `entry` | the first stage | The stage a run starts in. |
| `title`, `description` | from `[blueprint]` | What runs of it are called and do. |
| `max_child_depth` | the operator's | How deep a tree of child runs may grow. |
| `tool_permissions` | none | `allow`, `ask` or `deny` per tool, for every stage. |
| `read_paths` | none | Paths outside the workdir the run may read. |
| `write_paths` | none | Globs inside the workdir every write must match. See [Security](/docs/security#limiting-where-a-stage-writes). |
| `safe_commands` | none | `{ tools = [...], shell = [...] }` that run without approval. |
| `output` | none | The shape of the run's final answer. See [Outputs](/docs/outputs). |
| `compaction` | none | The models and prompts that summarize compacting regions. See below. |
| `taint_tracking` | off | Track where sensitive data flows. See [Security](/docs/security). |
| `sandbox` | the operator's | Where tools run. See [Containers](/docs/containers). |
| `nudge`, `repetition` | the operator's | The empty-reply nudge and loop detection. |
| `routing` | the operator's | How the "which stage next?" call is retried. See [Routing](#routing). |
| `file_tracking` | none | A region listing the files the run read and wrote. |
| `tool_rescan` | `"at_spawn"` | When the tool list is read again. |
| `dependencies` | none | What the run needs from the machine. |
| `mcp_servers` | none | MCP servers the blueprint brings with it. See [MCP](/docs/mcp). |
| `mime_types` | none | [Mime registry](/docs/mime) rows the run adds. |
| `script_permissions` | the operator's | What script tools may do, only ever stricter. |
| `transforms` | none | How a child run's context is built from this one's. |
| `batch_tool_hint`, `shell_hint` | the operator's | Two prompt hints, on or off. |

`tool_rescan` takes `"at_spawn"`, `"after_writes"` or `"before_dispatch"`.

`compaction` takes `model` (one entry) or `models` (a list, best first), each written like a
stage's entry: `{ model = "..." }` for an open route, `{ provider = "...", model = "..." }` to pin
one. Neither runs each summary on the current stage's model. `max_summary_tokens` defaults to 2000
and `temperature` to 0.2, and `system_prompt` and `user_prompt_template` replace the built-in
prompts. [Providers](/docs/providers#the-compaction-model) explains how the models are resolved. The [agent blueprint
guide](/docs/agents) explains the settings above in context.

## Inputs

`[[graph.inputs]]` declares a value a caller supplies. A caller changes a blueprint's run only
through these, so each one is a deliberate opening in the graph.

| Key | Required | What it is |
|---|---|---|
| `name` | yes | What a caller calls it: `--input since=...`, `"inputs": {"since": ...}`. |
| `type` | yes | Its type. See below. |
| `required` | no | Whether a run must be given it. One with a `default` never is. |
| `default` | no | The value used when none is given, tagged with its type. |
| `description` | no | What it is for. Forms and agents show it. |
| `binds` | no | Where the value goes: regions, a stage's model, an iteration cap. |

The types are `text`, `bool`, `int`, `float`, `choice`, `list`, `record`, `file`, `path`, `model`,
`blueprint`, `duration` and `url`. A type with no settings is written as its name, `type = "text"`.
One with settings is a table naming its `kind`:

```toml
type = { kind = "text", multiline = true, max_len = 4000 }
type = { kind = "list", item = "text", max = 10 }
type = { kind = "path", names = "dir", must_exist = true }
type = { kind = "record", fields = [{ name = "name", type = "text", required = true }, { name = "email", type = "text" }] }
```

A default is a checked value, written tagged with its type: `default = { int = 20 }`,
`default = { text = "none" }`, `default = { choice = "users" }`, `default = { bool = true }`.

`binds` lists slots. A region slot takes an optional template that can name any input in braces.

```toml
binds = [{ region = "task" }]
binds = [{ region = "brief", template = "Changes since {since}, for {audience}." }]
binds = [{ stage_model = "plan" }]
binds = [{ stage_max_iterations = "plan" }, { fan_out_max_workers = "split" }]
binds = ["output_format"]
```

[Input types](/docs/starting-a-run#input-types) has every type's settings and values, and [Where an
input goes](/docs/starting-a-run#where-an-input-goes) every slot and what it accepts.

A blueprint that takes a free-form job declares `task`, by convention a `text` input bound to a
region named `task`:

```toml
[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
required = true
binds = [{ region = "task" }]
```

## Layout and regions

`[graph.layout]` is the run's [context](/docs/context): an ordered list of named **regions**, each
a part of the run's memory with its own budget and rules.

| Key | Required | What it is |
|---|---|---|
| `regions` | yes | The regions, in the order the model reads them. |
| `total_budget_tokens` | yes | The whole layout's budget when every region has a fixed size. |
| `eviction_order` | no | Which regions give up entries first when the context is full. |

With any percentage budget, the total is worked out from each stage's model window and
`total_budget_tokens` is not used. Write `0` there. A `0` with fixed budgets means the same: the
layout has no total of its own, and the run gets the model's whole window.

`eviction_order` lists regions in the order they give way when the whole context is full. Only
clearable, temporary and unpinned custom regions ever do, and the named ones go before the rest of
their kind. A stage with its own `layout` uses that layout's order. [Eviction is
deterministic](/docs/context#eviction-is-deterministic) has the details.

Each `[[graph.layout.regions]]` entry:

| Key | Default | What it is |
|---|---|---|
| `name` | required | The region's name. Prompts, tools and inputs refer to it. |
| `kind` | required | How it keeps and drops entries. |
| `budget` | required | How big it may grow: tokens, or a share of the window. |
| `required` | `false` | Whether it must hold something. |
| `required_message` | none | What the refusal says when it is empty. |
| `seed` | none | What fills it at spawn, besides inputs bound to it. |
| `accepts` | any | Mime types a part in it may carry, such as `["image/*"]`. |
| `volatility` | `"rewritten"` | How often it changes, for prompt caching. |
| `admission` | `"evict"` | What happens to an entry that does not fit. |
| `compact_at` | 80% of the budget | The share of its budget at which a compacting region is summarized. |
| `summarizable` | `true` | Whether compaction may summarize it. |
| `description`, `describe_in_prompt` | none, `false` | What it is for, and whether the model is told. |

`kind` is a name, or a table for a kind with settings:

```toml
kind = "pinned"
kind = { kind = "sliding_window", max_items = 30 }
kind = { kind = "sliding_window", max_items = 40, eviction = { bulk = 20 } }
kind = { kind = "compact_history", source = "codebase" }
kind = { kind = "custom", code = { file = "regions/ledger.rhai" }, pinned = true }
```

The kinds are `pinned`, `sliding_window`, `temporary`, `compacting`, `clearable`,
`compact_history`, `keyed`, `checklist` and `custom`. [The nine region
kinds](/docs/context#the-nine-region-kinds) says what each one does.

`seed` fills a region at spawn from something other than a caller:

```toml
seed = { files = ["ARCHITECTURE.md", "docs/DESIGN.md"] }
seed = { glob = "src/**/*.proto" }
seed = { command = "git ls-files" }
seed = { literal = "Answer in British English." }
seed = { code = { file = "seeds/open_issues.rhai" } }
seed = { tools = { calls = [{ tool = "read_file", args = { path = "Cargo.toml" } }], refresh = "each_stage" } }
```

A value that changes per run is an input, not a seed. [Seeding a
region](/docs/context#seeding-a-region) has the details of each.

## Stages

Each `[[graph.stages]]` entry is one step of the run. The list order is the order `lev validate`
and the dashboard show, and the first stage is where a run starts unless `entry` says otherwise.

| Key | Default | What it is |
|---|---|---|
| `name` | required | The stage's name, unique in the graph. |
| `system_prompt` | none | What the model is told in this stage. |
| `model` | the operator's default | The models that may run it, best first. |
| `tools` | none | The tools it may call: names, or groups such as `"@builtin"`. |
| `max_iterations` | the operator's | The most model rounds before the stage is cut off. |
| `mode` | `"autonomous"` | How it runs: alone, taking turns, at checkpoints, as a fan-out. |
| `output` | none | The shape of its final answer. |
| `tool_routing` | none | Which region each tool's results land in. |
| `tool_permissions` | none | `allow`, `ask` or `deny` per tool, for this stage. |
| `write_paths` | none | Globs this stage's writes must match, on top of the graph's. |
| `max_revisits` | none | How often a gate may send the run back here. |
| `transition_prompt` | none | Extra guidance when the model picks the next edge. |
| `routing` | the graph's | How that pick is retried, and what a failure does. See [Routing](#routing). |
| `hide`, `reset` | none | Regions this stage does not show, or empties on entry. |
| `layout` | the graph's | A whole layout for this stage alone. |
| `add` | none | Regions shown beside the graph's layout, keyed by name. Not with `layout`. |
| `hooks` | none | Scripts run at points of the stage's life. |
| `description` | none | What the stage is for. |

Less common keys: `required_tools`, `connectors`, `requires_children`, `accepts_messages`,
`allow_complete`, `allow_as_worker`, `allow_blocking_tools`, `require_output`, `output_routing`,
`input_accepts`, `input_as_text`, `tool_accepts`, and per-stage `taint_tracking`, `sandbox`,
`nudge`, `batch_tool_hint` and `shell_hint`. `lev schema spawn-request` describes every one of
them under `StageDef`.

`model` lists models in the order they are tried. A model with no provider runs on the first
provider in your `provider_order` that serves it. If that provider answers that it has no such
model, the call moves to the next provider serving it, in the same turn. A `provider/model` pair
stays on that provider. [What moves a call](/docs/providers#per-stage-model-selection-with-fallback)
has the rest:

```toml
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5-5" }, { model = "gpt-5.5" }] }
model = { models = [{ model = "claude-opus-5-5" }], params = { temperature = 0.2, max_output_tokens = "40%" } }
model = { models = [{ model = "gpt-5.5" }], params = { extra = { reasoning_effort = "high" } }, request_timeout_secs = 600 }
```

`max_output_tokens` is a number of tokens, a share of the window such as `"40%"`, or a share of a
region such as `"100% of claims"`. `extra` passes settings the provider understands straight
through. Set `allow_user_default = false` to refuse the operator's default model as a stand-in.

`effort` asks the model to think less or more: `"low"`, `"medium"` or `"high"`. Each provider
sends it in its own field, so a stage whose list mixes providers sets it once:

```toml
model = { models = [{ provider = "meta", model = "muse-spark" }, { model = "claude-sonnet-5" }, { model = "gpt-5.5" }], effort = "low" }
```

| Provider | What it sends |
|---|---|
| OpenAI | `reasoning.effort`, to its reasoning models only |
| Anthropic | `output_config.effort`, to Opus 4.5 and every model from 4.6 on |
| Bedrock | Claude's `output_config.effort`, or `reasoningConfig` to Nova 2 |
| Gemini | `thinking_level`, to Gemini 3 text models |
| OpenRouter | `reasoning.effort`, at a level the model's listing names |
| Meta, Codex, xAI, Grok | `reasoning.effort`, to the models that take one |

A model with no such control is sent nothing. A level a model does not take moves to the nearest
one it does, so a model that takes only `high` gets `high`. Use `effort` rather than `extra` for
this: `extra` reaches every model in the list exactly as written, whichever provider serves it.

`mode` is `"autonomous"`, `"interactive"` or `"output"`, or a table for the modes that carry
settings.

An interactive stage takes turns with a person. After a reply with no tool calls it waits for the
next message instead of moving on: the run shows `waiting_input`, waiting on `awaiting_message`.
A message resumes the stage with its conversation intact. A message that arrives while the stage is
still replying is answered before the stage waits again. The stage moves on when the model calls
`end_conversation`, which the mode grants, or when a person ends the conversation. It also moves on
when its idle limit runs out:

```toml
mode = { interactive = { idle_timeout_secs = 900 } }
```

With no limit the stage waits for as long as it takes. In a run nobody is watching it waits 120
seconds: a run that answers its own questions (`--yolo` without `--allow-questions`), or a
sub-agent or fan-out worker, which its parent talks to. A
caller that starts runs of an interactive blueprint should set `idle_timeout_secs` rather than rely
on either. `lev msg <run> --end` ends a conversation from the command line. The limit is checked at least every 30 seconds, so a
stage can move on up to that much late. Each reply counts against the stage's `max_iterations`,
which caps the whole conversation.

A stage that stops at checkpoints lists them:

```toml
[[graph.stages.mode.interactive_points]]
name = "approve_plan"
prompt = "Approve this plan?"
style = "multiple_choice"
options = ["approve", "revise"]
```

A stage that splits its work over worker runs names itself and the workers it may hand items to,
each with what it is for:

```toml
[graph.stages.mode.fan_out]
stage = "split_review"
workers_available = [
    { name = "area", stage = "review_area", use_for = "Reviewing one area of the change" },
]
merge_stage = "report"
max_workers = 8
```

[Interaction](/docs/interaction) covers checkpoints, and [Sub-agents](/docs/sub-agents) covers
fan-out. Code is named the same way everywhere, as `{ file = "path.rhai" }` beside the blueprint or
`{ inline = "..." }`:

```toml
hooks = { on_stage_enter = { file = "hooks/load_ticket.rhai" } }
output = { format = "json", validator = { file = "validators/report.rhai" } }
```

An output can also be judged by a tool the stage is given, with
`validator_tool = { tool = "pw__check_report", content_arg = "report" }`. See
[Rhai output validators](/docs/rhai-validators#when-a-script-cannot-judge-it-a-validator-tool).

## Routing

When a stage with several normal edges is done, the model is asked which one to take. That short
call can fail like any other. `routing` sets how often it is tried and what happens after the
last try, for the whole graph under `[graph]` or for one stage:

```toml
[graph]
routing = { retry_attempts = 4 }

[[graph.stages]]
name = "verify"
routing = { on_error = "error_edge" }
```

| Key | Default | What it is |
|---|---|---|
| `retry_attempts` | the operator's `routing_retry_attempts`, `3` | Tries, the first one included. At least `1`. |
| `on_error` | `"fail"` | `"fail"` ends the run with the error; `"error_edge"` fails the stage. |
| `on_unmatched` | `"first_edge"` | What a second answer that names no stage does. `"first_edge"` falls back; `"fail"` ends the run. See below. |

Only a transient failure is tried again, as with any model call. A stage's setting wins over the
graph's, key by key. The run file records `routing`, so a run resumed after the daemon restarts
keeps it. A run recorded before the run file kept it (layout 5 and older) has none, and uses the
operator's settings.

The answer may run to 2,048 tokens, which leaves a model that reasons room to finish thinking and
still name a stage. On Meta, Codex, xAI and Grok, the call asks for `low` effort where the model
takes one. On OpenRouter, a model that must reason is asked for the least effort it takes, except
Anthropic's, which keep their cached context. A reply cut off by that limit, or one that holds only
the model's reasoning, is read as naming no stage.
A stage that sets `effort` sends it on its routing call too, in the same field its own calls use.
Where nothing above replaces it, the routing call asks for exactly what the stage asked for, which
keeps Anthropic's cached prompt.

An answer that names no stage is not taken as a choice. The question is asked once more with the
stage names alone, such as `Answer with exactly one of: build, summary.`, plus `DONE` where the
stage may end the run. That second call is a new routing call, so it gets its own
`retry_attempts`, and neither call spends the stage's retries. When the second answer names no
stage either, the run takes the stage's first edge, or ends where the stage may end the run. Both
steps leave a `[routing]` line in the stage log and a warning in `daemon.log`, and the transition's
reason is `Fallback`.

A blueprint that would rather stop than guess sets `on_unmatched = "fail"`, for the graph or for
one stage:

```toml
[[graph.stages]]
name = "verify"
routing = { on_unmatched = "fail" }
```

Then the second miss ends the run with an error that names the stage and quotes the answer, cut to
80 characters, such as `the stage after 'verify' could not be picked: the answer named no stage
("I think we should keep going"), asked twice`. The stage log and `daemon.log` carry the same line.
The stage's work is kept, and neither its `error` edge nor its retries are used, because the work
did not fail. An answer that names a stage, first or second, is taken as usual. Any other value is
refused when the blueprint loads.

With `"fail"`, the run stops with an error that names the routing call, and the stage's finished
work is kept. With `"error_edge"`, the stage fails, so its `error` edge runs when it has one. Choose
that only when the recovery stage can fix what made the routing call fail. Otherwise the run
redoes the stage and fails the same way until the stage runs out of revisits.

## Edges

`[[graph.edges]]` connects stages. An edge has a name, unique among the edges leaving its stage, and
says when the run takes it.

| Key | Default | What it is |
|---|---|---|
| `name` | required | The edge's name. The model picks edges by it. |
| `from`, `to` | required | The stage it leaves and the stage it enters. |
| `when` | `"always"` | `always`, `error`, `max_iterations`, `llm_choice`, `dead_end` or `stuck`. |
| `hint` | none | What the model is told about it when choosing. |
| `carry` | `"direct"` | What happens to the context on the way. |
| `gate` | none | What must be true before the run may take it. |
| `stuck` | none | When the stage counts as stuck, for a `stuck` edge. |

```toml
[[graph.edges]]
name = "review"
from = "implement"
to = "review"
hint = "The change is made and its tests pass"
carry = { compact = { prompt = "Summarize what changed and why." } }
gate = { require_modifications = true, message = "Change at least one file before review." }

[[graph.edges]]
name = "recover"
from = "implement"
to = "error_recovery"
when = "error"
```

`carry` is `"direct"`, `"clear"`, `{ compact = {} }` with an optional `prompt`, or a custom table
that names regions to keep, summarize and empty:

```toml
[graph.edges.carry.custom]
carry = ["plan", "discovery"]
compact = ["conversation"]
clear = ["scratch"]
compact_prompt = "Summarize the review as a numbered list of fixes."
```

The model picks among a stage's edges by their hints when its work there is done. An edge with a
condition such as `error` or `stuck` fires on its own when that happens. [Multi-stage
workflows](/docs/stages) explains choosing, gates and stuck detection.

A graph whose run could reach a stage and never get from there to an end still runs, with a loud
warning everywhere the run shows up. See [a run has to be able to
end](/docs/stages#a-run-has-to-be-able-to-end). Two other shapes are refused outright: a fan-out
into a stage of this graph that does not set `allow_as_worker = true`, and a `require_modifications`
gate on a stage with no tool that changes files.

## Short forms

A few values have a short form in TOML and JSON. Each reads into the same typed value as its long
form, so you can use whichever is clearer.

| Value | Short forms |
|---|---|
| A tool | `"read_file"`, or a group: `"@all"`, `"@builtin"`, `"@subagent"`, `"@scripts"`, `"@mcp"` |
| A model setting | `true`, `3`, `0.9`, `"high"`, `["STOP", "END"]` |
| A region budget | `4000`, `"10%"`, `{ percent = "10%", min = 500, max = 8000 }` |
| A reply cap | `8000`, `"40%"`, `"100% of claims"` |
| An input slot | `"output_format"`, `{ region = "task", template = "..." }`, `{ stage_model = "plan" }` |
| An input type | `"text"`, `{ kind = "int", min = 1, max = 5 }`, `{ kind = "choice", options = ["a"] }` |
| A region kind | `"pinned"`, `{ kind = "sliding_window", max_items = 20 }` |

TOML's own inline tables help too. `[graph] edges = [{ name = "write", from = "gather", to = "write" }]`
is the same as the `[[graph.edges]]` block above. `lev blueprint migrate` writes short tables inline
and leaves out every key that holds its default.

## Editor setup

An editor that reads JSON Schema can complete keys, explain them on hover and underline a typo
before you run `lev validate`. Leviath gives you two schemas for `agent.toml`:

| Schema | What it holds |
|---|---|
| [`blueprint.schema.json`](https://leviath.dev/docs/stable/blueprint.schema.json) | The format itself, the same on every machine |
| `~/.leviath/agent.schema.json` | The same rules, plus your tool, MCP tool and provider names as completions |

`lev setup`, `lev mcp add` and `lev mcp remove` rewrite the second file. An MCP server's tool names
are known only once Leviath has reached it, so `lev mcp test <name>` adds them as
`<server>__<tool>`. After you add a script tool or provider, run `lev schema blueprint --write` to
refresh it. The names are suggestions only, so a name the file does not list is never marked wrong.

For VS Code with Even Better TOML, or any editor that uses Taplo, put a `.taplo.toml` in your
project, with your home directory written out in full rather than as `~`:

```toml
[[rule]]
include = ["**/agent.toml"]

[rule.schema]
path = "/home/you/.leviath/agent.schema.json"
```

A single file can name its schema in a first-line comment instead:
`#:schema /home/you/.leviath/agent.schema.json`. Taplo reads a schema once, so reload the editor
window after the file changes.

`lev schema blueprint` prints the same schema to stdout, which is handy when an agent writes a
blueprint.

## Migrating from `agent.leviath`

`lev blueprint migrate` converts an `agent.leviath` into an `agent.toml` that describes the same
run:

```bash
lev blueprint migrate ./summarizer -o ./summarizer/agent.toml
lev validate ./summarizer/agent.toml
```

Without `-o` it prints the new file. An existing output file is left alone unless you pass
`--force`. When the manifest cannot be read, every problem is listed at once. A setting the new
format spells differently gets a `note:` line on stderr. A fan-out's `max_workers = 0` is one: it
meant no cap, and a graph says no cap by leaving `max_workers` out.

A key Leviath 0.6.4 and earlier accepted but never read, such as `max_stored` on a region or a
misspelled key, is dropped with a `warning:` line naming the blueprint, the key and its value. It
never changed a run, so the new file runs the same without it, but you may have meant something by
it. The daemon's upgrade and the conversion of old runs drop the same keys and warn the same way:
see [upgrading from an earlier release](/docs/daemon#upgrading-from-an-earlier-release).

Here is a small manifest and what it becomes. Before:

```toml
[agent]
name = "summarizer"
version = "0.3.0"
description = "Reads a file and summarizes it."

[context.regions]
task = { kind = "pinned", max_tokens = 2000, seed = "task", required = true }
source = { kind = "pinned", budget = "30%", seed = "source" }
conversation = { kind = "sliding_window", max_items = 20, budget = "40%" }

[tool_permissions]
read_file = "allow"

[stages.read]
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5-5" }] }
available_tools = ["read_file", "context_write"]
max_iterations = 6
system_prompt = "Read the file named in task and note what matters."

[stages.read.transitions.write]
hint = "Notes taken"

[stages.write]
model = { models = [{ provider = "openai", model = "gpt-5.5" }] }
system_prompt = "Write the summary."
```

After:

```toml
[blueprint]
name = "summarizer"
version = "0.3.0"
description = "Reads a file and summarizes it."

[graph]
edges = [{ name = "write", from = "read", to = "write", hint = "Notes taken" }]
tool_permissions = { read_file = "allow" }

[[graph.stages]]
name = "read"
system_prompt = "Read the file named in task and note what matters."
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5-5" }] }
tools = [
    "read_file",
    "context_write",
]
max_iterations = 6

[[graph.stages]]
name = "write"
system_prompt = "Write the summary."
model = { models = [{ provider = "openai", model = "gpt-5.5" }] }

[graph.layout]
total_budget_tokens = 2000

[[graph.layout.regions]]
name = "task"
kind = "pinned"
budget = 2000
required = true

[[graph.layout.regions]]
name = "source"
kind = "pinned"
budget = "30%"

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 20 }
budget = "40%"

[[graph.inputs]]
name = "source"
type = { kind = "text", multiline = true }
binds = [{ region = "source" }]

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
required = true
binds = [{ region = "task" }]
```

What changes on the way:

| `agent.leviath` | `agent.toml` |
|---|---|
| `[agent]` | `[blueprint]`, and `entry_stage` becomes `[graph] entry` |
| `[stages.<name>]` tables | `[[graph.stages]]` entries with a `name` |
| `available_tools` | `tools` |
| `[stages.<a>.transitions.<b>]` | `[[graph.edges]]` with `from = "a"`, `to = "b"` |
| `condition`, `transform` | `when`, `carry` |
| `[context.regions]` | `[[graph.layout.regions]]` with `kind` and `budget` |
| `max_tokens` | `budget` |
| `seed = "<name>"` | a `text` input named `<name>`, bound to the region |
| `[tool_permissions]`, `[[mcp_servers]]` | `tool_permissions` and `mcp_servers` under `[graph]` |

A region seeded from the caller becomes a declared input of the same name. A run of the new file
takes it as `--input source=...`, or `--source ...` for short. Each such input is `text`, so the
new file is a good moment to give the ones that are really numbers, choices or paths a better type.

`migrate` drops comments. Copy the ones worth keeping into the new file by hand, then delete
`agent.leviath`.
