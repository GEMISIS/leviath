---
title: Agent blueprints
group: Guides
order: 2
---

# Agent blueprints (`agent.leviath`)

An agent is a directory with an `agent.leviath` file — a TOML **blueprint** describing a
multi-stage workflow graph. `lev create <name>` scaffolds one.

```toml
[agent]
name = "coder"
version = "0.2.0"
description = "Analyze, implement, and review with graph-based recovery"
entry_stage = "analyze"

[tool_permissions]           # global defaults; per-stage overrides allowed
read_file  = "allow"
write_file = "ask"
bash       = "ask"

[stages.analyze]
mode = "autonomous"
model = { models = [
  { provider = "anthropic", model = "claude-sonnet-4-6" },
  { provider = "openai",    model = "gpt-5.4-mini" },
] }
available_tools = ["read_file", "list_dir"]
max_iterations = 15
system_prompt = """Understand the task and produce a short implementation plan."""

[stages.analyze.transitions.implement]
hint = "Plan ready — begin implementation"
```

## Stages and models

Each stage gets its own **model** (an ordered provider/model fallback list — the first configured
provider wins), tools, iteration cap, and context layout. Transitions form a
[graph](/docs/stages): linear by default, or branch on conditions like `error` and `stuck`.

## Context regions

`[context.regions]` defines the memory layout. Budgets can be **percentages of the model's context
window** (ceilings — they may sum past 100%), with absolute `max_tokens` / `threshold_tokens`
guard-rails. Region kinds: `pinned`, `sliding_window`, `compacting`, `compact_history`, `clearable`,
`hashmap`. See [Context regions](/docs/context).

## Seed commands

A region can be seeded before the run starts:

```toml
[context.regions.codebase]
kind = "compacting"
seed = { command = "git ls-files" }
```

Seeds run at spawn **before any approval prompt**, confined to the workdir and routed through the
entry stage's sandbox, time- and size-capped. `lev validate` prints them; refuse with
`--no-seed-commands` or `[security] allow_seed_commands = false`.

Validate and test a blueprint before running it:

```bash
lev validate .
lev test .
```
