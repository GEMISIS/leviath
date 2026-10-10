---
title: Structured context
description: How structured context regions keep an agent coherent across hundreds of tool calls, where a flat message list drifts.
group: Concepts
group_order: 2
order: 6
---

# Structured context memory

The usual way to give a model its history is one flat list of messages. That has a failure mode: read
one large file and it pushes everything else toward the edge of the window, including the system
prompt and the task the agent was given. The agent then forgets what it was doing, and nothing chose
that outcome.

Leviath splits the window into named **regions** instead. Each one has its own size limit and its
own rule for what to throw away first, so a big file read can only ever crowd out the region it
landed in.

```mermaid
flowchart LR
  subgraph FLAT["One flat list"]
    direction TB
    F1["task<br/>(oldest, first out)"]
    F2["early turns"]
    F3["a large file read"]
    F4["recent turns"]
  end
  FLAT -->|"window fills"| LOST["The task falls off the end"]

  subgraph REG["Named regions"]
    direction TB
    R1["task, pinned"]
    R2["codebase, compacting"]
    R3["conversation, sliding"]
  end
  REG -->|"window fills"| KEPT["Only the region that filled sheds;<br/>the task is untouched"]
```

## What that looks like

A typical coding agent might divide its window like this:

| Region | Share | Kind | Holds | When it fills |
|---|---|---|---|---|
| `task` | 12% | `pinned` | The task and the ground rules | Nothing. Pinned regions are never dropped |
| `codebase` | 20% | `compacting` | Files the agent has read | Older content is summarized, not lost |
| `conversation` | 33% | `sliding_window` | The back-and-forth | Oldest turns drop off |
| `history` | 15% | `compact_history` | Summaries carried from earlier stages | Rolls forward, compacted |
| headroom | 20% | | Left free for the reply | |

The point is the last column. In a flat message list, all five of those compete for the same space
and the loser is whatever happens to be oldest. Here, a file dump can fill `codebase` completely and
`task` is still exactly where it was.

In a blueprint's `agent.toml`, each region is one `[[graph.layout.regions]]` entry with a `name`, a
`kind` and a `budget`. The task arrives as an [input](/docs/starting-a-run#where-an-input-goes)
bound to the `task` region:

```toml
[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
binds = [{ region = "task" }]

[graph.layout]
total_budget_tokens = 0

[[graph.layout.regions]]
name = "task"
kind = "pinned"
budget = "12%"

[[graph.layout.regions]]
name = "codebase"
kind = "compacting"
budget = "20%"

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 20 }
budget = "33%"

[[graph.layout.regions]]
name = "history"
kind = { kind = "compact_history", source = "codebase" }
budget = "15%"
```

`total_budget_tokens` is required. It only matters when every budget is a fixed number, so a layout
written in percentages sets it to `0`.

## The nine region kinds

| Kind | Behavior |
|---|---|
| `temporary` | Recent entries, trimmed first under budget pressure. |
| `pinned` | Never evicted (architecture, the task). |
| `sliding_window` | Keeps the most recent entries; the conversation lives here. |
| `compacting` | Summarizes instead of evicting: file reads and tool results. |
| `compact_history` | Carries summaries from earlier stages forward, so a later stage skips the raw content. `source` names what it summarizes. |
| `clearable` | Wiped in one shot when space is needed (scratch). |
| `keyed` | Keyed entries; a write to a key replaces it. |
| `checklist` | A task list whose entries carry state. Written through `todo_add` / `todo_done` / `todo_note`, never evicted, and rendered open-items-first. |
| `custom` | Behavior defined by a Rhai script (see [Rhai regions](/docs/rhai-regions)). |

A `custom` region gets keyed writes too: entries written under one key render last-wins, like
`keyed`, though the shadowed entries keep holding budget until something evicts them. [Rhai
regions](/docs/rhai-regions#keyed-writes-render-as-an-upsert) has the details.

Every region names its `kind`. An unrecognized one is a hard parse error that lists the nine, not
a silently ignored region. So is an unrecognized `eviction`: `eviction = "per-item"` with a hyphen
is refused, rather than leaving the region to evict one entry at a time as if the line had not been
written.

### Per-kind keys

A kind that takes settings is written as a table, `kind = { kind = "...", ... }`:

```toml
[[graph.layout.regions]]
name = "conversation"
# max_items is required. eviction: "per_item" (the default), { bulk = 10 } lets
# the window run 10 over its cap and then drops back to it, { compact = 10 }
# folds the oldest 10 into a summary.
kind = { kind = "sliding_window", max_items = 20, eviction = { bulk = 10 } }
budget = "33%"

[[graph.layout.regions]]
name = "codebase"
# threshold_tokens is a hard token ceiling, applied as well as compact_at.
kind = { kind = "compacting", threshold_tokens = 30000 }
budget = "20%"
compact_at = 0.8               # compact once this full, see below

[[graph.layout.regions]]
name = "history"
kind = { kind = "compact_history", source = "codebase" }   # whose summaries roll forward
budget = "5%"

[[graph.layout.regions]]
name = "findings"
kind = { kind = "keyed", max_entries = 50 }   # a write to an existing key replaces it
budget = "10%"

[[graph.layout.regions]]
name = "brain"
# The file is relative to the blueprint's directory. pinned = true behaves
# like a pinned region: never evicted.
kind = { kind = "custom", code = { file = "context_hooks/brain.rhai" }, pinned = false }
budget = "5%"
```

### Tracking work with a checklist

A pinned region plus `context_append` gives persistence, which is the easy half. What it does not
give is *state*: "compute the fee table" and "~~compute the fee table~~ done" are two different
strings, so nothing can count what is left and no gate can ask.

```toml
[[graph.layout.regions]]
name = "todos"
kind = "checklist"
budget = "3%"
```

The agent writes to it through tools rather than free text, so the state cannot drift from what the
model believes it wrote:

| Tool | Effect |
|---|---|
| `todo_add(region, item)` | Adds an open item, returns its id |
| `todo_done(region, id)` | Ticks it off |
| `todo_note(region, id, note)` | Records a note **without** closing it |

It renders as one stable block with open items first. Sitting in the system section is what keeps it
in front of the model every turn, as instruction rather than history.

An id is never reused, so a `todo_done` cannot land on a different item than the one it names. An id
that matches nothing is an error the model can read rather than a silent no-op.

The gate is the part that makes any of this enforceable:

```toml
[[graph.edges]]
name = "review"
from = "implement"
to = "review"
gate = { require_no_open_items = "todos", message = "Finish or explicitly drop the open items first." }
```

The nudge names the items that are still open. It shares the same `max_attempts` budget as every
other gate, so it cannot wedge a run. A gate naming a region no stage declares, or a region that is
not a `checklist`, is refused by `lev validate`. At runtime such a gate could only ever count zero
and pass on the first attempt, which looks exactly like a stage that finished its work.

### Keys every region accepts

| Key | Default | Meaning |
|---|---|---|
| `name` | required | The region's name, unique in its layout |
| `kind` | required | One of the nine kinds above |
| `budget` | required | Tokens (`4000`), a share of the window (`"35%"`), or a clamped share. See below |
| `compact_at` | 80% | For a compacting region, the fraction of its budget at which it compacts, as `0.8` |
| `seed` | unset | What the region starts with. See below |
| `required` | `false` | The stage re-runs rather than moving on while this region is empty |
| `summarizable` | `true` | Set false to keep an edge's `carry = { compact = {} }` from paraphrasing this region. See [carrying context](/docs/stages#carrying-context-across-an-edge) |
| `description` | unset | One line on what the region is for. See below |
| `describe_in_prompt` | `false` | Also show the `description` to the model, above the region's contents. See [what the model sees](#what-the-model-sees) |
| `volatility` | `"rewritten"` | How much the region's contents move between requests. See [what caching costs](#what-caching-costs) |
| `admission` | `"evict"` | What happens when a write does not fit. See [letting the agent decide what to forget](#letting-the-agent-decide-what-to-forget) |
| `accepts` | unset | Mime types the region takes, as `type/subtype` or `type/*`. See [More than text](/docs/mime) |
| `required_message` | generated | What the model is told when a required region is empty. Supports `{region}` |

Four of those keys carry more than a table cell holds. A `description` is documentation by
default: it reaches `lev dash` and the API, not the model. `volatility` decides where the region
sits in the prompt, and `admission = "reject"` refuses a write that does not fit instead of dropping
something to make room. An unset `accepts` takes anything, and a write carrying a type the list does
not cover is refused with that list.

**Resolved budget** is the phrase used for the number a region actually gets, once the percentage
has been worked out against the model in front of it. A `budget = "20%"` region on a 200k-token
model resolves to 40,000 tokens. `compact_at = 0.8` then means 80% of *that*, so 32,000.

`budget` takes three forms:

```toml
budget = 4000                                        # a fixed token count
budget = "10%"                                       # a share of the window
budget = { percent = "10%", min = 500, max = 8000 }  # a share, clamped
```

The clamp is worth dwelling on, because it is easy to write a cap that quietly cancels the
percentage. `budget = { percent = "30%", max = 40000 }` is 30% only below a 133k window. Above that
it is a flat 40,000 however large the model. A 1M-context run held its findings to 40k that way
while its own blueprint asked for 314k. If you mean the percentage, write the percentage on its
own. Reach for `max` when a region genuinely must not grow, and `min` when a small one must not
shrink on a narrow window. Check what the pair resolves to at the largest model you expect to run.

A malformed `budget` is a hard error at load, so `lev validate` catches it instead of a run failing
later.

### Seeding a region

A region starts empty unless something fills it before the first inference. Two things can.

**Values that change from run to run arrive as inputs.** The task, a diff, a focus area: whatever
the caller supplies is a typed input the blueprint declares in `[[graph.inputs]]` and binds to a
region. `lev run --task` fills the input named `task`, and `--input standards=...` (or the short
form `--standards ...`) fills `standards`. [Where an input
goes](/docs/starting-a-run#where-an-input-goes) covers binding, templates and attached files:

```toml
[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
required = true
binds = [{ region = "task" }]

[[graph.inputs]]
name = "standards"
type = "text"
binds = [{ region = "standards" }]
```

A parent run passes values to a child the same way, as the child's inputs. By convention a
blueprint names its main input `task` and binds it to a region named `task`. A blueprint that
declares no `task` input refuses a task outright rather than running without it.

**Everything the blueprint fills by itself is a `seed`** on the region:

```toml
[[graph.layout.regions]]
name = "readme"
kind = "pinned"
budget = "5%"
seed = { files = ["README.md"] }

[[graph.layout.regions]]
name = "layout"
kind = "temporary"
budget = "10%"
seed = { glob = "src/**/*.rs" }

[[graph.layout.regions]]
name = "rules"
kind = "pinned"
budget = "1%"
seed = { literal = "Never edit generated files." }

[[graph.layout.regions]]
name = "env"
kind = "pinned"
budget = "2%"
seed = { command = "git log --oneline -20" }

[[graph.layout.regions]]
name = "machine"
kind = "pinned"
budget = "1%"
seed = { tools = { calls = [{ tool = "current_time", args = {} }, { tool = "system_info", args = {} }] } }

[[graph.layout.regions]]
name = "computed"
kind = "temporary"
budget = "5%"
seed = { code = { file = "seeds/plan.rhai" } }
```

| Form | Fills from |
|---|---|
| `{ files = [...] }` | The contents of those files |
| `{ glob = "..." }` | Every file matching the pattern |
| `{ literal = "..." }` | Fixed text |
| `{ command = "..." }` | The stdout of a shell command |
| `{ code = { file = "..." } }` | The return value of a Rhai script beside the blueprint |
| `{ tools = { calls = [...] } }` | The results of tool calls. See [seeding from tools](#seeding-from-tools) |

A region takes one seed, and inputs bound to it land beside whatever the seed wrote. The table keys
are exactly the ones in the left column. Anything else, such as `{ caller_input = "..." }`, is a
parse error naming the six forms, so `lev validate` catches it instead of a region coming out
blank.

> [!WARNING]
> A `command` seed runs at spawn, before the first inference and therefore before any tool-approval
> prompt. Because there is nobody to ask in the moment, it must also be covered by
> [`[graph.safe_commands]`](/docs/interaction#what-runs-without-asking), or it does not run at all.
> `lev validate` prints every command seed in a blueprint, `lev run --no-seed-commands` refuses
> them for one run, and `[security] allow_seed_commands = false` refuses them machine-wide. Seeds
> run once: a daemon restart does not replay them.

#### Where a seed path resolves

`files` and `glob` seeds resolve against the run's working directory and may not leave it. A path
that does is refused at spawn, before anything is read.

The rule is the one `read_file` follows, for the same reason: the *blueprint* chose this path, not
you. Seeded contents land in a region the model reads on its first turn, so a path that escaped
would put whatever it named in front of the model without anything having asked you.

To read outside on purpose, list it in the blueprint's `read_paths` (under `[graph]`) and grant it
in your config. That is already the mechanism for "this blueprint is meant to read there and I
agreed", and seeding answers to it rather than having a second one of its own. A glob is checked per
match, since `../*.toml` cannot be judged before it is expanded.

A blueprint can also seed from files it ships itself. The `blueprint:` prefix resolves the rest of
the path against the blueprint's own directory instead of the working directory, so bundled
material travels with the blueprint:

```toml
[[graph.layout.regions]]
name = "guidelines"
kind = "pinned"
budget = "3%"
seed = { files = ["blueprint:config/style.md"] }

[[graph.layout.regions]]
name = "rubric"
kind = "pinned"
budget = "3%"
seed = { glob = "blueprint:rubrics/*.md" }
```

A prefixed path may never leave the blueprint's directory, and `read_paths` does not apply to it.
A grant widens what a run may read on your machine, not what a package pretends to ship, so
`blueprint:../secrets.txt` is refused however the config is set. This is the same containment a
script gets, because the claim is the same: these are the blueprint's own files, and a blueprint's
own files live beside it.

Scripts proper are stricter still and have no `read_paths` escape at all. Every `{ file = "..." }`
a blueprint names, whether a `code` seed, a stage hook, a custom-region script or an output
validator, resolves against the blueprint's own directory and must stay inside it. A script is code
the blueprint ships, and there is no such thing as loading your logic from somewhere else on
purpose. A graph sent whole in a request has no directory, so it carries its code inline as
`{ inline = "..." }` instead.

### Seeding from tools

A `tools` seed calls the run's own tools at spawn and writes their output into the region, so the
agent's first inference already knows what the tools would have told it. Several calls fill one
region, in order, each under a heading naming the tool:

```toml
[[graph.layout.regions]]
name = "environment"
kind = "pinned"
budget = "1%"
volatility = "stable"
seed = { tools = { calls = [
  { tool = "current_time", args = {} },
  { tool = "system_info", args = {} },
  { tool = "locale_info", args = {} },
] } }
```

```
--- current_time ---
{ "utc": "2026-08-18T19:32:07Z", ... }

--- system_info ---
{ "os": "macos", ... }
```

Any tool the agent could call works, spelled as the agent would spell it: a built-in, an
[MCP server's](/docs/mcp) `<server>__<tool>`, or a [Rhai script tool](/docs/rhai-tools). Each call
names its `tool` and its `args`, written as the model would write them; `{}` for none:

```toml
[[graph.layout.regions]]
name = "toolchain"
kind = "pinned"
budget = "1%"
seed = { tools = { calls = [
  { tool = "which_command", args = { command = "git" } },
  { tool = "locale_info", args = {} },
] } }
```

Use it for anything the agent should not have to think to ask for. The clearest case is the date: a
research agent that never calls `current_time` reasons from its training cutoff, and seeding the
answer costs it no turn.

> [!IMPORTANT]
> Unlike a `command` seed there is no separate kill switch, because a tool seed reaches nothing
> new. Every call resolves against the same `tool_permissions` the tool lane applies mid-run, so a
> seed can call exactly what the agent could call and nothing more, and a `deny` counts here too.
>
> A tool set to `ask` is **refused**, not prompted: a seed runs before the first inference, so there
> is nobody to answer. Set it to `allow` if the agent is meant to call it at spawn. `lev validate`
> lists every tool a blueprint seeds from, as `tool-seed`.

A failed call is skipped with a warning and the other calls still fill the region; if the region is
`required`, a failure is a spawn error naming the tool.

#### Refreshing on every stage

Seeds resolve once, at spawn, like every other kind. `refresh = "each_stage"` resolves them again
whenever a stage is entered:

```toml
seed = { tools = { calls = [{ tool = "current_time", args = {} }], refresh = "each_stage" } }
```

Use it where the answer moves. A run that spends an hour in one stage and then enters another
should date the second stage from when it started, not from when the run did. The stage waits for
the refreshed region before its first request, so the values are in place for the turn that reads
them.

It costs a tool call per stage entry for the life of the run, and it rewrites a region that would
otherwise sit still in the cached prefix. Leave it at the default for anything that does not
actually change. A call that fails leaves the region as it was rather than blanking it: the
previous value is stale, and stale beats absent. `lev validate` marks a refreshing seed
"on every stage entry".

Seeds do not re-run when a run is resumed from its [run file](/docs/run-file#resuming-a-run), whatever
their `refresh` setting.

## What the model sees

Regions that assemble into the system prompt are labelled with their own name:

```
## task
research what meta's most recent earnings call was about

## sources_index
[1] RFC 9110 - https://example - 2022 - credibility: high
```

The name is the part that earns its tokens. An agent writes to a region *by
name*, as in `context_write { region: "sources_index", … }`. Without the heading
it reads a region's contents with nothing saying which region they came from. It
could read `sources_index` and write to `sources_index` and have no way to know
they were the same place. A heading costs three tokens, once per region, however
many entries the region holds.

A `description` says what the region is for. On its own it is documentation:
`lev dash` shows it under the region, `GET /api/blueprints/{name}` returns it,
and the model never sees it. Add `describe_in_prompt` to spend the tokens and
put it in front of the model too:

```toml
[[graph.layout.regions]]
name = "sources_index"
kind = "pinned"
budget = "4%"
description = "One bibliography line per source actually used."
describe_in_prompt = true
```

which renders as:

```
## sources_index
One bibliography line per source actually used.

[1] RFC 9110 - …
```

The split is deliberate. Describing every region for the people who maintain the
blueprint should not quietly cost tokens on every turn, and most region names
are already the explanation. Turn it on where the region has a convention the
agent has to follow: a format, an ordering, a rule about what belongs. Leave it
off for a purpose the agent can infer from the name.

Empty regions contribute nothing, not a heading and not a blank block. A
blueprint can declare the regions it might need without paying for the ones it
has not filled yet.

### Stored parts in the prompt

An entry can hold more than text: an image, a clip, a document, any
[stored part](/docs/mime). In a region that renders into the system prompt
the part appears as its one-line stand-in, `[image/png 1024x768, 240 KB] hero.png`.
The bytes travel in one user message placed before the conversation, each after
a pointer naming the region, the key and the part. In the conversation the
part sits in its own turn, after the text it came with. A tool result's parts
follow the result in the same turn.

Whether the model gets the bytes is decided when the request is built, not
when the entry is written. A model whose input types cover the part gets it as
that provider's native block. A part whose bytes are text reaches any model as
text. Anything else is the stand-in alone, which still names the part so the
model can hand it to a tool. The entry itself carries a reference to the part,
never the bytes.

## What caching costs

A provider caches the prompt by **prefix**: it stores everything up to a marker, and next
request it reuses that only if every byte in front of the marker is identical. So one region
that changes invalidates the cache for every region behind it, however still those are.

That makes the ordering of the prompt worth money, and the ordering is decided by what each
region declares:

```toml
[[graph.layout.regions]]
name = "task"
kind = "pinned"
budget = "2%"
volatility = "stable"      # set once at spawn

[[graph.layout.regions]]
name = "sources"
kind = "pinned"
budget = "30%"
volatility = "grows"       # appended to as the run goes

[[graph.layout.regions]]
name = "scratch"
kind = "keyed"
budget = "5%"
volatility = "rewritten"   # rebuilt each turn
```

| value | means | gets |
|---|---|---|
| `stable` | written rarely or never after setup | sorted first, forming the prefix everything else caches behind |
| `grows` | appended to, existing entries untouched | sorted next, and split so its settled part caches while only the newest is re-sent |
| `rewritten` | existing content changes in place | sorted last, where it invalidates nothing but itself |

The default is `rewritten`, which is the pessimistic one. A region nobody has classified is
assumed to move, so leaving this out never puts a region somewhere that invalidates another;
declaring it is what earns the caching.

That is worth stating in money, because "safe default" reads as "no decision needed" and the
region this matters most for is the biggest one you have. A research run measured here left its
280,000-token findings region undeclared: assumed rewritten, cached at 4%, so the same content
was re-sent, re-billed and re-processed on every inference for the rest of the stage. Declared
`grows`, almost all of it caches. The bigger the region, the more the default costs, and the
biggest region in a blueprint is usually the append-only one tool results land in.

> [!NOTE]
> The region's **kind** does not answer this, which is why the setting exists. A `pinned`
> region sounds immutable and is written constantly. `context_write` into a findings region
> is an ordinary move, and [tool routing](#routing-tool-output) sends read results straight
> into one. Only the blueprint knows which of yours is which.

`temporary` and `clearable` are worth declaring for the same reason, and the payoff is larger.
Both names describe when the region is *thrown away*, one at stage exit and the other on demand.
Neither says anything about whether the contents hold still in between. Undeclared they are treated
as uncacheable, which is right at the boundary and wrong everywhere else. A stage that reads a
corpus into a `temporary` region and then works through it for forty calls re-sends the whole corpus
at full rate on every one of them. Measured on one such stage: 5.36M tokens across 46 calls, the
largest single cost line in the run. Declaring it `grows` splits it the same way any other growing
region is split, so the part already read caches and only the newest excerpt is re-sent.

If a region declares `stable` and then keeps changing, Leviath says so in the log rather than
silently paying for it: the declaration is a hint it checks, not a promise it trusts.

### A region that stops growing caches itself

Declare a region by what it does across the whole run, not per stage. A `grows` region is split
into chunks that freeze once full. When the appending stops, every chunk is already frozen and the
whole region caches. That is the shape of a gathering stage that ends, with a planning stage only
reading what it collected. Measured on that shape: 99% of the prompt cacheable in the planning
stage, with only the plan itself, rewritten each turn, outside it.

Re-declaring such a region `stable` for the later stage changes nothing worth having. The same
bytes are cached either way. `stable` renders as one block where `grows` renders as several, so
there are fewer places to put a marker and one fewer *fallback*. A fallback only pays if the region
turns out to change, which in that stage it does not. Declare a region by what it does
across the run and leave it alone.

Caching is also per model, so a stage that switches model starts cold whatever the blocks look
like. The benefit concentrates inside a stage rather than across a model change, and no layout
avoids that.

A stage can still replace the layout, volatility included, with a `layout` of its own
(`[graph.stages.layout]`). See [layout and regions](/docs/blueprint-format#layout-and-regions). That
is for a stage whose memory is genuinely shaped differently, not for this.

## Where a stage's own instructions live

A stage's `system_prompt` is pinned context, which is why it reads as instruction rather than
history. It goes into a region like everything else. By default that region is *whichever pinned
region you declared first*. That costs three things: its tokens are charged to that region's name in
the [stage ledger](/docs/cli#lev-stages-run-id), you cannot size or scope it, and it lands wherever
that region sits in the cacheable prefix.

Name a region for it and all three go away:

```toml
[[graph.layout.regions]]
name = "stage_instructions"
kind = "pinned"
budget = "3%"
```

The runtime writes the entering stage's prompt there, replacing the previous stage's. It is always
assembled **after** every other pinned block, however you declared it, so the content in front of it
stays byte-identical when the stage changes. That content is what a provider's prompt cache
matches on. Instructions sitting in front of the shared prefix rewrite its head on every transition,
which invalidates everything behind them.

Measured on a two-stage agent whose prompts are about 63 tokens each:

| Region | Without the declaration | With it |
|---|---|---|
| `task` | 65 | 2 |
| `stage_instructions` | not present | 63 |

The 65 is the whole problem in one number: two tokens of task and sixty-three of somebody else's
instructions, under a heading that says `task`.

A blueprint that declares no such region keeps the old behaviour exactly, so this costs nothing to
ignore. The region is never hidden by a stage's `hide` list or its own `layout`: it
holds the instructions of the stage being entered, so hiding it would drop that stage's prompt.

### What a stage change still costs

The declaration keeps the system prompt's *head* cacheable across a transition. It cannot keep the
**conversation** cacheable, and on a long run the conversation is most of the prompt.

A provider matches one prefix running from the start of the request. The system prompt comes first
and the conversation second, so a stage's new instructions sit in front of every message. Change
them and nothing behind them matches, however byte-identical the transcript is. Measured on a run
whose closing stage rewrote its prompt: the final call read 2,376 tokens of stable system head and
paid full price for 246,812 tokens of conversation. That was about 40% of what the whole run cost
after caching.

The remedy is to not change the system prompt on the last hop. A closing instruction delivered as a
[nudge](/docs/stages) goes into the conversation instead, which leaves the prefix in front of it
untouched, so the transcript still matches and only the nudge itself is new.

Worth the trouble only where the conversation is large and the stage is short. A wind-down stage
that makes one expensive call is exactly that shape. A stage that makes twenty calls amortizes its
transition over all of them and this is not worth restructuring for.

## Eviction is deterministic

When a region crosses its threshold, the runtime acts by the region's *kind*, never by pushing out
whichever message is oldest across the whole window:

```mermaid
flowchart TD
  W["New entry routed to a region"] --> C{"Region over<br/>threshold?"}
  C -->|no| K["Keep"]
  C -->|yes| T{"Region kind?"}
  T -->|pinned| K2["Keep, never evicted"]
  T -->|sliding_window| D["Drop oldest entries"]
  T -->|compacting / compact_history| S["Summarize into a compact form"]
  T -->|clearable / temporary| CL["Trimmed or cleared under budget pressure"]
```

When the whole window is full, the runtime makes room by kind as well. Clearable regions are
emptied whole first. Then temporary and unpinned custom regions give up their oldest entries, and
compacting regions are summarized last, by the graph's `compaction` model. A blueprint without one
still has room made the first two ways; its compacting regions are left as they are. Pinned regions, sliding windows, histories, keyed regions,
checklists and anything under `admission = "reject"` are never touched.

A layout's `eviction_order` says which regions go first when clearable regions are emptied and
when temporary and custom ones give up entries:

```toml
[graph.layout]
total_budget_tokens = 0
eviction_order = ["raw_pages", "scratch"]
```

Here `raw_pages` is emptied before `scratch` loses anything, and both before the other regions of
their kind. Those others take turns, one entry each. Naming a region that is never touched changes
nothing, and `lev validate` warns about it. A stage with its own `layout` uses that layout's order;
every other stage uses the graph's.

A summary is text. When a compacting region's entries carried stored [parts](/docs/mime) (an
attached image, a file a tool stored), the summary is written from their stand-ins and the parts
leave the window with the entries they sat on. The bytes stay in the run's store, `lev blobs`
still lists them, and the run log names which ones a compaction dropped. Pin a region whose files
a later stage needs, or have the stage put them somewhere pinned with `context_attach`.

## Letting the agent decide what to forget

Everything above is reactive: a region crosses a threshold and the runtime makes room. That is the
right default, and it has a blind spot. The runtime knows sizes; only the agent knows when it is
*done* with something. A gather stage that fetches a spec, pulls out the three paragraphs that
matter and writes them to a curated region has no further use for the raw text. The raw text sits
there anyway, until pressure happens to push it out, or, with a generous budget, until the run
ends.

An agent can release an entry the moment it is spent:

```
context_delete { region: "sources", key: "rfc-9110" }
context_delete { region: "sources", index: 2 }
context_delete { region: "sources", oldest: 3 }
```

Name the entry by `key` if it was written with one, by `index` as shown in `context_list`, or ask
for the oldest few. Releasing returns the tokens immediately.

Giving an entry a key when you write it is what makes the first form possible:

```
context_append { region: "sources", key: "rfc-9110", content: "<the raw spec>" }
```

### Making the agent choose

By default a full region evicts, and the agent is never told. For a region holding material the
agent curated, that is the wrong trade: whichever write arrives when the region is full silently
decides what was least important.

`admission = "reject"` hands that decision back:

```toml
[[graph.layout.regions]]
name = "sources"
kind = "temporary"
budget = "30%"
admission = "reject"
```

Now a write that does not fit fails, and the agent is told the region is full and to release
something first. Nothing already in the region is lost to a write the agent did not know would
displace it. A region set this way is also exempt from the window-level eviction cascade. Otherwise
`reject` would only change which code did the silent dropping.

This turns memory management into an explicit decision: *you must choose what to forget before you
can read more*. It is a better failure mode than a silent omission the agent never learns about. It
is also a genuinely different memory discipline from mechanical eviction, worth reaching for when
the region holds findings rather than transcript.

## Routing tool output

Tool output is **routed** to a region, so exploration lands in a persistent codebase region rather
than scratch:

```toml
[[graph.stages]]
name = "analyze"
# ...

[graph.stages.tool_routing]
default_region = "scratch"
keep_results = true               # false sends every result to `scratch` instead
max_result_tokens = 4000          # ceiling for any tool without one of its own
tool_regions = { read_file = "codebase" }
# A stage that both greps and reads files needs two numbers, not one: a cap
# sized for the file read lets every grep through untouched, and one sized for
# the grep truncates every file.
tool_max_result_tokens = { read_file = 20000 }
```

Both maps are keyed by tool name, and an alias matches the tool it aliases. Writing `bash` covers
the `shell` the model actually calls.

A stage may only route into a region it can see. A region the stage hides, or leaves out of its own
`layout`, is somewhere it cannot read a result back. `lev validate` refuses such a blueprint and
says which region to add. The four the runtime always carries are always
valid targets: `conversation`, `tool_results`, `final_output` and `stage_instructions`.

### What the model is told about a routed result

A routed result cannot sit in the message stream, because a `tool_result` has to follow its
`tool_use` immediately. The full output goes to the region instead, and a short pointer stays in the
conversation in its place. The pointer names the region, and says the contents are already in the prompt under that
heading, because they are: a region the stage carries is rendered into the system prompt every turn.

That wording matters more than it looks. The pointer used to end "read that region for the full
result", which is an instruction with no tool behind it. The model, holding `read_file` and no
`context_read`, would aim `read_file` at the region name and keep trying spellings. Grant
`context_read` on a stage that routes and reads files; `lev validate` warns
(`routing-without-region-read`) when one does not, and a path tool pointed at a region name now says
so in its error.

The pointer also says what actually happened rather than what was meant. A region too full to take
the result whole reports the truncation or the refusal, instead of promising a full result that is
not there.

The two maps are independent, which is usually what you mean when a tool needs its own region *and*
its own ceiling:

```toml
[graph.stages.tool_routing]
default_region = "scratch"
tool_regions = { read_file = "codebase", grep = "scratch" }   # grep: routed, no cap
tool_max_result_tokens = { read_file = 20000, list_dir = 500 } # list_dir: capped, not moved
```

A tool in `tool_regions` alone routes without capping, and a tool in `tool_max_result_tokens` alone
caps without moving the result out of `default_region`. A key outside these is an error rather than
a line that is quietly skipped.

`read_file` also has a hard byte cap of its own, independent of any of this, and says so in the
result when it applies. Without one, a large file went into its region whole and was either
truncated or dropped as `[result omitted]` depending on how full the region already was. That is a
cliff rather than a limit.

## Routing produced parts

`tool_routing` moves *tool results*. A model can also **produce parts of its own**: a picture from
an image model, audio from a speech model, a document a generator returns. Those default to the
conversation, riding the assistant turn like its text. `output_routing` sends them somewhere else,
**by mime type**, so a produced file lands in a region a later stage reads instead of in the running
transcript:

```toml
[[graph.layout.regions]]
name = "artwork"
kind = "pinned"
budget = "10%"
accepts = ["image/*"]

[[graph.layout.regions]]
name = "handouts"
kind = "pinned"
budget = "10%"
accepts = ["application/pdf"]

[[graph.stages]]
name = "draw"
output_routing = { "image/*" = "artwork", "application/pdf" = "handouts" }
```

Each key is a mime pattern (`image/png`, `image/*`, `*/*`) and each value a region. A reply that
mixes text and other parts is split part by part. Every part goes to the region of the **most
specific** matching pattern (`image/png` beats `image/*` beats `*/*`). The reply's text stays in
`conversation` as before, along with any part no rule matched. Nothing here names a family in code.
It is mime types all the way down, so the same table routes audio, video, 3D models or any type you
register the same way it routes images.

Unlike `tool_routing`, the target need not be a region *this* stage reads back, since the whole
point is usually to hand a produced file forward. So it is checked against every region the
blueprint declares, not only the ones the producing stage can see. A target no layout declares is
refused by `lev validate`.

A pinned target lifts its stored parts into the leading user turn, and a sliding window renders them
as a user message. The next stage's model sees the bytes either way, subject to that model taking
the type. If it does not, it sees the stand-in, as anywhere else.

### A clean slate for the next stage

Routing the produced part out of the conversation is half of handing it on; the other half is the
receiving stage not inheriting the producing stage's transcript. `conversation` cannot be hidden,
because the model's own turns live there. A stage can still **empty** a region as it is entered:

```toml
[[graph.stages]]
name = "describe"
reset = ["conversation"]
```

`reset` clears the named regions on entry, and the content is gone rather than hidden from this
stage alone. The stage then starts on a clean conversation with only what its visible regions hold,
such as the routed image in `artwork`. A re-entered stage clears them again each visit. Unlike `hide`, `reset` may
name `conversation`; like `hide`, a name no layout declares is refused.

## Requests are measured before they are sent

The window sizes what it holds with a byte estimate, corrected by what earlier calls in the run
were charged. That is cheap and it is usually close. A provider whose window is a hard ceiling
rejects a request that is over by one token. That rejection is not transient: the retry resends the
same request and the stage dies.

So a request that could be near the window is measured before it goes out. When the corrected
estimate plus the reply budget reaches half the model's window, the runtime asks the provider's own
tokenizer what the request costs. It refuses to send one that would not fit. The tokenizer is
`/messages/count_tokens` on Anthropic, `:countTokens` on Gemini, tiktoken locally on OpenAI, and a
script's `count_tokens` on a [Rhai provider](/docs/rhai-providers). The refusal names all three
numbers it was computed from: the prompt count, the `max_output_tokens` reply budget, and the
window. The reply budget is usually the one that tipped the sum. An error that only showed the
prompt against the window pointed at the wrong number. A request under that line is sent as it is,
so a short turn
pays nothing. Every lane is guarded the same way: the stage's own call, the routing call at a
stage boundary, compaction and titling.

The window in that refusal is whatever the provider declares for the model, and a declared window
that is too small refuses everything a real one would have carried. The common case is a
[Rhai provider](/docs/rhai-providers#the-script-contract) left on its 8192-token
`@max_context_tokens` default. A stage asking for an 8192-token reply can then never send anything,
and the fix is the script's annotation (or a `[model_capabilities]` entry), not the stage.

The count is also fed back into the correction, so a refused request tightens the estimate for the
retry rather than being rediscovered by it.

## Budgets travel across models

This is why budgets are written as percentages. A region sized at 20% of the window is 20% whether
the model has 32k or 200k tokens, so the same blueprint keeps its shape when you switch models. Fixed
token counts would need rewriting every time.

> [!NOTE]
> Percentages are ceilings, and they may add up to more than 100%. That is deliberate: regions
> rarely fill at the same time, so reserving exact shares would waste most of the window. A ceiling
> also costs nothing until it is reached, because a region is charged for what is stored in it, not
> for its budget. That is why raising one is cheap and capping one is not. Use a fixed `budget`, a
> `max` on a percentage, or `threshold_tokens` when you need a limit that really is hard, and
> remember a `max` overrides the percentage rather than sitting beside it.

## Regions on a provider with no cache breakpoints

Not every provider takes cache markers. The Codex transport, which bills a
ChatGPT subscription, has no `cache_control` and no TTL to choose: it caches by
literal prefix and nothing else.

Your regions still arrive whole, one block each, in the order assembly sorted
them. What changes is what that order is worth. Elsewhere the stable-first sort
is an optimisation on top of explicit markers; there it is the entire strategy,
because a cache hit runs up to the first byte that moved and stops. A region
that declares `volatility = "stable"` and is rewritten every turn costs more
there than anywhere else, and the warning about an unstable declaration is
worth acting on rather than noting.
