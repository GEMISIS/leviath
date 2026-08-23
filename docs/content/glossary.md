---
title: Glossary
description: Every term the Leviath docs use in a particular way, defined in one place.
group: Guides
group_order: 4
order: 4
---

# Glossary

Every word the Leviath docs use in a particular way, in one place. If a page uses a term you have
not met, it should be here.

## The basics

**Agent**: a directory holding an [`agent.leviath`](/docs/agents) blueprint, which you start with
`lev run`.

**Blueprint**: the TOML file describing an agent, listing its stages, models, tools, and context
layout. Some older text says *manifest*; it means the same file.

**Run**: one execution of an agent. An agent is the recipe, a run is the cooking.

**Run id**: the name a run is known by everywhere outside the engine, such as
`coder-1785568852-8b48c0d1e2f3`. The CLI, the API, and the dashboard all use it. It is the handle you pass to
`lev cancel`, `lev msg`, and the rest.

**Daemon**: the background process that holds every running agent. See [the daemon](/docs/daemon).

**Unattended**: a run with nobody watching, usually started with `--yolo`. Tools that wait for a
person are removed so the run does not stop for somebody who is not there. It waives approvals, not
checkpoints: a stage keeps whatever it lists in `required_tools`, and an interaction point declaring
`unattended = "ask"` still opens its prompt.

**Workdir**: the directory a run treats as its project. File tools cannot read or write outside it
unless you grant a [read path](/docs/security). Defaults to wherever you ran `lev run`.

## Workflow

**Stage**: one step of a blueprint's [graph](/docs/stages), with its own model, tools, and context.

**Transition**: an edge from one stage to another. A **hint** transition is chosen by the agent. A
**conditional** transition fires on its own, on a runtime signal.

**Transform**: what an edge does to the context on its way across. `direct` carries everything,
`clear` drops it, `compact` summarizes it. See [carrying context](/docs/stages).

**Stuck**: a *measured* condition, such as too many iterations or repeated edits to one file, that
lets a stage escape a loop. The agent does not get to declare it. See
[stuck detection](/docs/stages#stuck-detection).

**Nudge**: a short `[System]` message the runtime adds to a conversation to get an agent moving
again, for example when it replies with text instead of using its tools. It is only a message. It
never fails or reroutes a stage. See [nudging](/docs/stages#nudging).

**Interaction point**: a place in a blueprint where the run stops and asks a person something. Some
older text calls these checkpoints. See [Human-in-the-loop](/docs/interaction).

**Seed command**: a shell command that fills a context region before the run starts.

**Spawn**: starting a run. Also used for the moment it starts, as in "resolved at spawn", meaning
worked out once when the run began rather than repeatedly.

## Memory

**Context region**: a named slice of the model's context window with its own budget and its own rule
for what to throw away first. See [Structured context](/docs/context).

**Eviction**: what happens when a region goes over its budget. Depending on the region's kind, its
content is dropped, summarized, or cleared.

**Compaction**: summarizing a region's content with a model call so it takes fewer tokens, rather
than discarding it. Slower than eviction, and keeps more meaning.

**Budget**: how much of the context window a region may use. Often written as a percentage so the
same blueprint works across models with different window sizes.

**Journal**: the append-only record of what a run did, written as it happens. It is what lets the
daemon reload an interrupted run without repeating tool calls that already took effect.

## Running many agents

**Sub-agent**: a child agent started by another one, running in the same process at some **depth**.

**Fan-out**: a [stage](/docs/sub-agents) that splits work into items, runs one sub-agent per item,
and merges the results.

**Tick**: one pass of the engine over every agent. See [the engine](/docs/engine).

**Fingerprint**: a count of how many agents are in each state, plus a per-agent progress digest
that catches an agent leaving and re-entering the same state. When a whole tick leaves it
unchanged, nothing moved, so the engine sleeps.

**Dispatch**: handing work out. A dispatch system starts a model call or a tool batch. A collect
system picks the result up later.

**Marker**: the component that says what an agent is currently doing. An agent has no field for
its pipeline phase. It carries one of twelve markers, and each system acts on the marker it cares
about.

**Permit**: a slot in a pool. An agent takes one before calling a model and gives it back when the
call finishes.

**Inference pool**: the per-model cap on how many requests may be in flight at once. See
[inference pools](/docs/engine#inference-pools).

**Tool lane**: the shared cap on how many agents' tool batches may run at once across the whole
daemon. See [the tool lane](/docs/engine#the-tool-lane).

**Backpressure**: what happens when a pool is full. New work is not started yet. Nothing
fails and nothing queues up at the provider.

**Park**: to stop and wait, without holding anything up. It means two related things. A run parks
when it is waiting for a person to answer. A tool batch parks when it gives its lane slot back
while it waits, so other work can use it.

**Wedged**: stuck in a state nothing can move it out of. Different from parked, which is a normal
wait, and from slow. `[limits] wedge_timeout_secs` fails wedged runs instead of leaving them
reporting as running.

## Extending it

**Rhai**: a small scripting language embedded in Leviath. You use it to add tools, model providers,
context regions, and policy rules without rebuilding anything. See [Rhai scripting](/docs/scripting).

**Hook**: one of the functions your Rhai script provides for the runtime to call, such as `render`
or `on_write` on a custom region.

**MCP server**: an external [Model Context Protocol](/docs/mcp) tool server that Leviath connects to
in order to offer its tools to agents.

**Provider**: a model backend such as Anthropic, OpenAI, or Ollama. See
[Providers](/docs/providers).

## Safety

**Sandbox**: per-agent or per-stage [isolation](/docs/security), using a container, a namespace, or
nothing.

**Taint**: the sensitivity label on each region, one of **Public**, **Internal**, or **Private**.
The runtime assigns it. Model output never can.

**Clearance**: how sensitive a tool is allowed to handle. A tool that could send data off the
machine is blocked when the data's taint is above its clearance.

**Exfiltration-capable**: describes any tool that could carry bytes off the machine, such as an HTTP
request or an email. These are the tools the taint gate checks.

**Control socket**: the local Unix socket, or Windows named pipe, that the CLI uses to reach the
daemon. It is not a network port. For network access, run [`lev serve`](/docs/api).
