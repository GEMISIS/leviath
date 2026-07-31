---
title: Sub-agents & fan-out
group: Concepts
order: 4
---

# Sub-agents and fan-out

Agents spawn children with different blueprints, all in the same process and over one shared
inference driver.

## Fan-out

A **fan-out** stage splits a task into work items and runs one sub-agent worker per item
concurrently, bounded by `max_workers`, then merges their results back into the parent.

```toml
[stages.fix.fan_out]
worker_agent = "."          # blueprint for each worker
query        = "..."        # how to split the work
max_workers  = 8
merge_stage  = "verify"
on_worker_failure = "continue"
```

Any sub-agent, at any depth, can ask the user a question directly — no fire-and-forget, no routing
through the parent. The [dashboard](/docs/dashboard) and the API's `/api/agents/tree` show the full
sub-agent tree with per-subtree token roll-ups.
