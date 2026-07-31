---
title: Multi-stage workflows
group: Concepts
order: 2
---

# Multi-stage workflows

A blueprint is a **graph of stages**. Each stage has its own model, tools, and context layout. Run
them linearly, or branch with conditional transitions, LLM-driven routing, and error recovery.
Check the graph with `lev validate`.

```toml
[stages.implement.transitions.review]
hint = "Implementation complete, ready for review"

[stages.implement.transitions.reassess]
condition = "stuck"          # a runtime condition, not the agent's choice
```

## Transitions

- **hint** transitions are chosen by the agent (LLM-routed).
- **conditional** transitions fire automatically on a runtime signal: `error`, or `stuck`.

## Stuck detection {#graph}

`stuck` escapes a stage that is making no progress. Stuckness is *measured*, not self-reported:

```toml
[stages.implement.transitions.reassess]
condition = "stuck"
stuck_after_iterations      = 20   # inferences in this stage
stuck_after_same_file_edits = 5    # write/edit calls against one path
stuck_after_tool_calls      = 100
stuck_after_minutes         = 30
```

Any subset applies; the first threshold to trip fires the edge, and the runtime writes *why* into
the target stage's context so the next stage knows what happened.
