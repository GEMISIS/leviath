---
title: ECS engine
group: Concepts
group_order: 2
order: 2
---

# The ECS agent engine

Leviath runs agents as **entities in a [bevy_ecs](https://bevyengine.org/) world**. Hundreds of
agents share one process with game-engine-style scheduling, instead of one OS process each.

## Why an ECS?

Orchestration tools that spawn a separate process per agent carry the full weight of a Node or Go
runtime per agent, and each manages its own flat context window. Leviath's engine runs them all
over one shared, lock-free inference driver, so spinning up ten agents doesn't mean ten times the
device RAM.

```mermaid
flowchart LR
  subgraph tick["Each scheduler tick"]
    direction TB
    S1["advance stages"] --> S2["dispatch inference"]
    S2 --> S3["route tool output<br/>to context regions"]
    S3 --> S4["evaluate transitions<br/>(error / stuck / hint)"]
  end
  tick -->|systems run over| ents["Agent entities<br/>(components: stage, context, model, status)"]
  ents --> tick
```

Each agent is an **entity**; its stage, context regions, model selection, and status are
**components**; and the runtime's **systems** advance every agent a step per tick. Because there's
one world, cross-cutting features come for free:

- **[Sub-agents and fan-out](/docs/sub-agents)** are just more entities in the same world: no new
  processes, no IPC.
- **Shared inference** means rate limits, retries, and provider clients are pooled across all
  agents rather than duplicated per process.
- **One context store** lets the [dashboard](/docs/dashboard) and [API](/docs/api) read every
  agent's state without touching hundreds of separate processes.

## Inference pools

"Shared inference" is concrete: the world holds a per-model pool that caps in-flight requests.
An agent acquires a permit before calling the provider and holds it for the whole request; agents
waiting for a permit just stay in their ready-to-infer state, which costs nothing - no thread is
parked, no request is queued at the provider. The default cap comes from
`[limits] max_concurrent_inferences` in the config, and per-model limits override it.

This is a different knob from the two it is often confused with: fan-out's `max_workers` bounds
how many *sub-agents* a stage spawns ([Sub-agents](/docs/sub-agents)), and
`[rate_limits.<provider>]` shapes *request rate* to a provider. The pool bounds concurrency; the
rate limiter bounds throughput; both apply.

> [!NOTE]
> The engine is an implementation detail you rarely configure directly. You describe *what* an
> agent does in its [blueprint](/docs/agents), and the engine schedules it. This page is here so
> the "hundreds of agents, one process" claim isn't a black box.
