---
title: Structured context
group: Concepts
order: 1
---

# Structured context memory

Most agent tools hand an LLM a flat message array. Leviath gives it **regions** — typed slices of
the context window with deterministic eviction, so a file dump can't push out your system prompt.

Six region kinds:

| Kind | Behavior |
|---|---|
| `pinned` | Never evicted (architecture, the task). |
| `sliding_window` | Keeps the most recent entries; the conversation lives here. |
| `compacting` | Summarizes instead of evicting — file reads and tool results. |
| `compact_history` | Rolls compacted summaries forward across stages. |
| `clearable` | Dropped wholesale at a stage boundary (scratch). |
| `hashmap` | Keyed entries; a write to a key replaces it. |

Tool output is **routed** to a region, so exploration lands in a persistent codebase region rather
than scratch:

```toml
[stages.analyze.tool_routing]
default_region = "scratch"
[stages.analyze.tool_routing.overrides]
read_file = "codebase"
```

Budgets can be **percentages of the model's context window**, so a blueprint's intent survives
across models of different sizes. Percentages are ceilings and may sum past 100%; absolute
`max_tokens` and `threshold_tokens` are hard guard-rails.
