---
title: Structured context
description: How structured context regions keep an agent coherent across hundreds of tool calls, where a flat message list drifts.
group: Concepts
group_order: 2
order: 4
---

# Structured context memory

The usual way to give a model its history is one flat list of messages. That has a failure mode: read
one large file and it pushes everything else toward the edge of the window, including the system
prompt and the task the agent was given. The agent then forgets what it was doing, and nothing chose
that outcome.

Leviath splits the window into named **regions** instead. Each one has its own size limit and its
own rule for what to throw away first, so a big file read can only ever crowd out the region it
landed in.

> [!NOTE]
> **Before this page:** [Agent blueprints](/docs/agents).
> **In one line:** you decide what the window is made of, and what gets dropped when it fills.

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

```toml
[context.regions]
task         = { kind = "pinned", budget = "12%", seed = "task_input" }
codebase     = { kind = "compacting", budget = "20%" }
conversation = { kind = "sliding_window", budget = "33%", max_items = 20 }
history      = { kind = "compact_history", budget = "15%", source_region = "codebase" }
```

## The eight region kinds

| Kind | Behavior |
|---|---|
| `temporary` | The **default** when `kind` is omitted; recent entries, trimmed first under budget pressure. |
| `pinned` | Never evicted (architecture, the task). |
| `sliding_window` | Keeps the most recent entries; the conversation lives here. |
| `compacting` | Summarizes instead of evicting: file reads and tool results. |
| `compact_history` | Carries summaries from earlier stages forward, so a later stage knows what happened without holding the raw content. Names the region it summarizes with `source_region`. |
| `clearable` | Wiped in one shot when space is needed (scratch). |
| `hashmap` | Keyed entries (alias `hash_map`); a write to a key replaces it. |
| `custom` | Behavior defined by a Rhai script (see [Rhai regions](/docs/rhai-regions)). |

An unrecognized `kind` is a hard parse error, not a silently ignored region.

### Per-kind keys

Most kinds take extra keys that only make sense for them:

```toml
[context.regions.conversation]
kind      = "sliding_window"
max_items = 20                 # default 10
strategy  = "per_item"         # per_item (default) | bulk | compact
overflow  = 10                 # with strategy = "bulk": how many to drop at once
compact_count = 10             # with strategy = "compact": how many to fold into a summary

[context.regions.codebase]
kind             = "compacting"
budget           = "20%"
compact_at       = "80%"       # compact once this full, see below
threshold_tokens = 30000       # a hard token ceiling, applied as well as compact_at

[context.regions.history]
kind          = "compact_history"
source_region = "codebase"     # which region's summaries roll forward

[context.regions.findings]
kind        = "hashmap"
max_entries = 50               # a write to an existing key replaces it

[context.regions.brain]
kind       = "custom"
script     = "context_hooks/brain.rhai"   # relative to the agent directory
persistent = false             # true behaves pinned-like: never evicted
```

### Keys every region accepts

| Key | Default | Meaning |
|---|---|---|
| `budget` | unset | A share of the model's context window, written as `"35%"` |
| `max_tokens` | `5000` | A token ceiling. See below for how it interacts with `budget` |
| `min_tokens` | unset | A floor for a percentage budget, so the region stays usable on a small model |
| `seed` | unset | What the region starts with. See below |
| `required` | `false` | The stage re-runs rather than moving on while this region is empty |
| `required_message` | generated | What the model is told when a required region is empty. Supports `{region}` |

**Resolved budget** is the phrase used for the number a region actually gets, once the percentage
has been worked out against the model in front of it. A `budget = "20%"` region on a 200k-token
model resolves to 40,000 tokens. `compact_at = "80%"` then means 80% of *that*, so 32,000.

`max_tokens` behaves differently depending on whether `budget` is set. On its own it is a plain
ceiling. Alongside `budget`, it caps the resolved percentage, so the region gets whichever is
smaller.

A malformed `budget` or `compact_at` string is a hard error at load, so `lev validate` catches it
instead of a run failing later.

### Seeding a region

`seed` fills a region before the first inference:

```toml
[context.regions]
task      = { kind = "pinned", seed = "task_input" }
standards = { kind = "pinned", seed = "input" }
readme    = { kind = "pinned", seed = { files = ["README.md"] } }
layout    = { kind = "temporary", seed = { glob = "src/**/*.rs" } }
rules     = { kind = "pinned", seed = { literal = "Never edit generated files." } }
env       = { kind = "pinned", seed = { command = "git log --oneline -20" } }
computed  = { kind = "temporary", seed = { rhai = "seeds/plan.rhai" } }
inherited = { kind = "pinned", seed = { caller = "brief" } }
```

| Form | Fills from |
|---|---|
| `"task_input"` | The caller's `task` key, which is what `lev run --task` sets |
| `"input"` | A caller key named after this region, so `--<region>` on the CLI reaches it |
| `"<any-other-string>"` | The caller key of that name |
| `{ files = [...] }` | The contents of those files |
| `{ glob = "..." }` | Every file matching the pattern |
| `{ literal = "..." }` | Fixed text |
| `{ command = "..." }` | The stdout of a shell command |
| `{ rhai = "..." }` | The return value of a Rhai script |
| `{ caller = "..." }` | A named value passed by a parent agent |

A region literally named `task` gets `seed = "task_input"` implicitly, so older blueprints keep
working.

> [!WARNING]
> A `command` seed runs at spawn, before the first inference and therefore before any tool-approval
> prompt. Because there is nobody to ask in the moment, it must also be covered by
> [`[safe_commands]`](/docs/interaction#what-runs-without-asking), or it does not run at all.
> `lev validate` prints every command seed in a blueprint, `lev run --no-seed-commands` refuses
> them for one run, and `[security] allow_seed_commands = false` refuses them machine-wide. Seeds
> run once: a daemon restart does not replay them.

#### Seed paths stay in the working directory

`files`, `glob` and `rhai` seeds resolve against the run's working directory and may not leave it.
A path that does is refused at spawn, before anything is read.

The rule is the one `read_file` follows, for the same reason: the *blueprint* chose this path, not
you. Seeded contents land in a region the model reads on its first turn, so a path that escaped
would put whatever it named in front of the model without anything having asked you.

To read outside on purpose, declare it under `[read_paths]` and grant it in your config. That is
already the mechanism for "this agent is meant to read there and I agreed", and seeding answers to
it rather than having a second one of its own. A glob is checked per match, since `../*.toml` cannot
be judged before it is expanded.

Scripts are stricter and have no `[read_paths]` escape: a stage hook, a custom-region script and an
output validator must all live inside the blueprint's own directory. A script is code the agent
ships, and there is no such thing as loading your logic from somewhere else on purpose.

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

## Routing tool output

Tool output is **routed** to a region, so exploration lands in a persistent codebase region rather
than scratch:

```toml
[stages.analyze.tool_routing]
default_region = "scratch"
[stages.analyze.tool_routing.overrides]
read_file = "codebase"
```

## Budgets travel across models

This is why budgets are written as percentages. A region sized at 20% of the window is 20% whether
the model has 32k or 200k tokens, so the same blueprint keeps its shape when you switch models. Fixed
token counts would need rewriting every time.

> [!NOTE]
> Percentages are ceilings, and they may add up to more than 100%. That is deliberate: regions
> rarely fill at the same time, so reserving exact shares would waste most of the window. Use
> `max_tokens` and `threshold_tokens` when you need a limit that really is hard.
