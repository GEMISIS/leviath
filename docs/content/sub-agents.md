---
title: Sub-agents & fan-out
group: Concepts
group_order: 2
order: 6
---

# Sub-agents and fan-out

Agents spawn children with different blueprints, all in the same process and over one shared
inference driver, with no new OS processes and no IPC. Sub-agents are just more entities in the
[ECS world](/docs/engine).

## Fan-out

A **fan-out** stage splits a task into work items and runs one sub-agent worker per item
concurrently, bounded by `max_workers`, then merges their results back into the parent:

```mermaid
flowchart TB
  P["Parent fan-out stage"] --> Q{"split_prompt<br/>→ work items"}
  Q --> W1["worker 1"]
  Q --> W2["worker 2"]
  Q --> W3["worker 3"]
  W1 & W2 & W3 --> M["merge_stage<br/>(aggregate results)"]
  M --> P2["Parent continues"]
```

```toml
[stages.fix.fan_out]
worker_agent = "."          # blueprint each worker runs
split_prompt = "..."        # prompt that produces the JSON array of work items
merge_stage  = "verify"     # stage the parent resumes at once workers finish
max_workers  = 8            # concurrency bound
on_worker_failure = "continue"
```

This is how the `parallel-fixer` agent fixes many failing tests at once, one worker per failure,
and how a wide research sweep runs many sub-topics in parallel.

## Human-in-the-loop, at any depth

Any sub-agent, at any depth, can ask *you* a question directly, with no fire-and-forget and no
routing through the parent:

```mermaid
sequenceDiagram
  participant You
  participant Parent
  participant Worker as Sub-agent (depth 2)
  Parent->>Worker: spawn with a work item
  Worker->>You: ask_user "which API version?"
  You-->>Worker: "v2"
  Worker-->>Parent: result
```

> [!TIP]
> The [dashboard](/docs/dashboard) and the API's `GET /api/agents/tree` show the full sub-agent
> tree with per-subtree token roll-ups, so you can see exactly where the budget is going.
