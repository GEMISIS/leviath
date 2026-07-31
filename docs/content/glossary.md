---
title: Glossary
group: Guides
group_order: 4
order: 3
---

# Glossary

The vocabulary Leviath's docs use, in one place.

## Core

**Agent** — a directory with an [`agent.leviath`](/docs/agents) blueprint, run with `lev run`.

**Blueprint** — the TOML file describing an agent: its stages, models, tools, and context layout.

**Daemon** — the background process that hosts every running agent in one [shared world](/docs/daemon).

**ECS world** — the single [bevy_ecs](/docs/engine) world the daemon runs; agents are entities in it.

## Workflow

**Stage** — one node in a blueprint's [graph](/docs/stages), with its own model, tools, and context.

**Transition** — an edge between stages. A **hint** transition is chosen by the agent; a
**conditional** transition fires automatically on a runtime signal.

**Stuck** — a *measured* runtime condition (too many iterations, repeated edits, …) that escapes a
non-progressing stage. See [stuck detection](/docs/stages#graph).

**Seed command** — a shell command that pre-fills a context region before the run starts.

## Memory

**Context region** — a typed slice of the context window with its own budget and
[eviction rule](/docs/context) (`pinned`, `sliding_window`, `compacting`, …).

**Eviction** — what happens when a region exceeds its threshold — drop, summarize, or clear,
depending on the region kind.

**Budget** — a region's size, often a **percentage of the model's context window** so it travels
across models.

## Fan-out and tools

**Sub-agent** — a child agent spawned by another, running in the same process at some **depth**.

**Fan-out** — a [stage](/docs/sub-agents) that splits work into items and runs one sub-agent worker
per item, then merges results.

**MCP server** — an external [Model Context Protocol](/docs/mcp) tool server Leviath connects to.

**Provider** — a model backend (Anthropic, OpenAI, Ollama, …); see [Providers](/docs/providers).

## Safety

**Sandbox** — per-agent or per-stage [isolation](/docs/security) (container, namespace, or none).

**Taint** — the deterministic **Public / Internal / Private** sensitivity label on each region that
gates exfiltration-capable tools.

**Control socket** — the local Unix socket / Windows pipe (peer-cred checked) the CLI uses to reach
the daemon — not a network port. For network access, run [`lev serve`](/docs/api).
