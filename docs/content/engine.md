---
title: ECS engine
group: Concepts
order: 3
---

# The ECS agent engine

Leviath runs agents as **entities in a [bevy_ecs](https://bevyengine.org/) world**. Dozens of agents
share one process with game-engine-style scheduling, instead of one OS process each.

Why it matters: orchestration tools that spawn a separate process per agent carry the full weight of
a Node/Go runtime per agent, and each manages its own flat context window. Leviath's engine runs
them all over one shared, lock-free inference driver, so spinning up ten agents doesn't mean ten
times the device RAM.

Sub-agents and fan-out workers are just more entities in the same world — see
[Sub-agents](/docs/sub-agents).
