---
title: Multi-stage workflows
group: Concepts
group_order: 2
order: 5
---

# Multi-stage workflows

A blueprint is a **graph of stages**. Each stage has its own model, tools, and context layout. Run
them linearly, or branch with conditional transitions, LLM-driven routing, and error recovery.

```mermaid
flowchart LR
  analyze -->|hint: plan ready| implement
  implement -->|hint: ready for review| review
  implement -->|condition: stuck| reassess
  review -->|condition: error| implement
  reassess -->|hint| implement
  review --> done["done"]
```

Every blueprint's graph is checkable before you run it:

```bash
lev validate .             # verifies the graph is well-formed and reachable
```

## Transitions

Each edge is one of two kinds:

- **hint** transitions are chosen by the agent (LLM-routed) when it decides the stage's goal is met.
- **conditional** transitions fire automatically on a runtime signal rather than the agent's
  choice: `error`, `stuck`, `max_iterations` (the stage's iteration cap is hit), or `always` (an
  unconditional edge).

```toml
[stages.implement.transitions.review]
hint = "Implementation complete, ready for review"

[stages.implement.transitions.reassess]
condition = "stuck"          # a runtime condition, not the agent's choice
```

<a id="graph"></a>

## Stuck detection

`stuck` escapes a stage that is making no progress. Crucially, stuckness is **measured, not
self-reported**. An agent can't loop forever insisting it's almost done:

```toml
[stages.implement.transitions.reassess]
condition = "stuck"
stuck_after_iterations      = 20   # inferences in this stage
stuck_after_same_file_edits = 5    # write/edit calls against one path
stuck_after_tool_calls      = 100
stuck_after_minutes         = 30
```

Any subset applies; the first threshold to trip fires the edge.

> [!TIP]
> When a `stuck` or `error` edge fires, the runtime writes *why* into the target stage's
> [context](/docs/context), so the recovery stage starts out knowing what went wrong instead of
> rediscovering it. The same happens when a stage hits its iteration cap: whatever stage runs
> next is told the work was cut off, not finished. Stuck reasons go to a `stuck_report` region
> when the blueprint declares one; error and iteration-cap notes prefer an `error_report` region.
> Declare them `pinned` (a small budget like 2000 tokens is plenty) so the note survives edge
> transforms; without them, notes land in `conversation`.

## Nudging

When a stage's model replies with plain text before it has made a single tool call, the runtime
normally injects a `[System]` nudge ("You have tools available...") and re-runs the stage, up to
three times. That is the right reflex for a coding stage that stalls, and the wrong one for a stage
whose deliverable *is* text: a planner told to "use your tools" goes hunting for a write tool it
does not have.

Each stage can say what should happen instead:

```toml
[agent.nudge]                # agent-wide default for every stage
max = 2

[stages.plan.nudge]
enabled = false              # this stage's deliverable is text; never nudge

[stages.implement.nudge]
max = 2
text = "You have edit tools. Make the change described in {regions} rather than describing it again."
```

All three keys are optional and cascade independently: a stage block wins over `[agent.nudge]`,
which wins over the `[nudge]` section of your `config.toml`, which falls back to the built-in
defaults. This is a UX knob, not a permission, so a manifest may raise `max` above the global
setting as freely as it lowers it.

The `text` may name `{stage}` (the stage's name) and `{regions}` (the comma-separated names of the
stage's required context regions). The same substitution applies to a required region's
`required_message`, where `{region}` names the region being demanded.

With nothing configured, one stage shape is already exempt: a stage with interaction points
presents its text for review, so it is never nudged for producing exactly that text. Setting
`enabled` explicitly at any level overrides this in either direction.
