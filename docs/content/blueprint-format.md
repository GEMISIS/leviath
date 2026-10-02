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
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }, { provider = "openai", model = "gpt-5.4-mini" }] }
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
| `safe_commands` | none | `{ tools = [...], shell = [...] }` that run without approval. |
| `output` | none | The shape of the run's final answer. See [Outputs](/docs/outputs). |
| `compaction` | none | The model and prompts that summarize a full context. |
| `taint_tracking` | off | Track where sensitive data flows. See [Security](/docs/security). |
| `sandbox` | the operator's | Where tools run. See [Containers](/docs/containers). |
| `nudge`, `repetition` | the operator's | The empty-reply nudge and loop detection. |
| `file_tracking` | none | A region listing the files the run read and wrote. |
| `tool_rescan` | `"at_spawn"` | When the tool list is read again. |
| `dependencies` | none | What the run needs from the machine. |
| `mcp_servers` | none | MCP servers the blueprint brings with it. See [MCP](/docs/mcp). |
| `mime_types` | none | [Mime registry](/docs/mime) rows the run adds. |
| `script_permissions` | the operator's | What script tools may do, only ever stricter. |
| `transforms` | none | How a child run's context is built from this one's. |
| `batch_tool_hint`, `shell_hint` | the operator's | Two prompt hints, on or off. |

`tool_rescan` takes `"at_spawn"`, `"after_writes"` or `"before_dispatch"`. The [agent blueprint
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
`total_budget_tokens` is not used. Write `0` there.

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
| `max_revisits` | none | How often a gate may send the run back here. |
| `transition_prompt` | none | Extra guidance when the model picks the next edge. |
| `hide`, `reset` | none | Regions this stage does not show, or empties on entry. |
| `layout` | the graph's | A whole layout for this stage alone. |
| `hooks` | none | Scripts run at points of the stage's life. |
| `description` | none | What the stage is for. |

Less common keys: `required_tools`, `connectors`, `requires_children`, `accepts_messages`,
`allow_complete`, `allow_as_worker`, `allow_blocking_tools`, `require_output`, `output_routing`,
`input_accepts`, `input_as_text`, `tool_accepts`, and per-stage `taint_tracking`, `sandbox`,
`nudge`, `batch_tool_hint` and `shell_hint`. `lev schema spawn-request` describes every one of
them under `StageDef`.

`model` lists models in the order they are tried. A model with no provider runs on the first
provider in your `provider_order` that serves it:

```toml
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }, { model = "gpt-5.5" }] }
model = { models = [{ model = "claude-opus-5" }], params = { temperature = 0.2, max_output_tokens = "40%" } }
model = { models = [{ model = "gpt-5.5" }], params = { extra = { reasoning_effort = "high" } }, request_timeout_secs = 600 }
```

`max_output_tokens` is a number of tokens, a share of the window such as `"40%"`, or a share of a
region such as `"100% of claims"`. `extra` passes settings the provider understands straight
through. Set `allow_user_default = false` to refuse the operator's default model as a stand-in.

`mode` is `"autonomous"`, `"interactive"` or `"output"`, or a table for the two modes that carry
settings. A stage that stops at checkpoints lists them:

```toml
[[graph.stages.mode.interactive_points]]
name = "approve_plan"
prompt = "Approve this plan?"
style = "multiple_choice"
options = ["approve", "revise"]
```

A stage that splits its work over worker runs names its worker:

```toml
[graph.stages.mode.fan_out]
worker = { stage = "review_area" }
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
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
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
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }
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
