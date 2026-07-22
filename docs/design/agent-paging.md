# Agent paging + deterministic tree persistence

Status: **proposed** (v1). Author: daemon-lifecycle work, 2026-07.

## Problem

The daemon is one `PipelineWorld` hosting every agent. Two lifecycle gaps make
it unfit for a long-running v1 daemon:

1. **Sub-agent trees are lost on restart.** Only a child's `parent_run_id`
   string is persisted. The ECS links (`ParentRef`, `SubAgentChildren`,
   `FanOutWaiting`) are never rebuilt on reload, so a reloaded parent proceeds
   as if its children finished (the `requires_children` gate is a no-op without
   `SubAgentChildren`), and children reload as orphan roots whose results are
   never collected. Fan-out parents never resume their split/merge.

2. **Terminal agents are never reaped.** `WorldHost::by_run_id` / `emitted` /
   `emitted_interactions` and the ECS entities themselves grow monotonically —
   nothing is ever despawned in production. A daemon that runs for days leaks
   every agent it has ever run.

## Direction (from product owner)

- The parent↔child relationship must be **deterministically stored**, not
  reconstructed by heuristic. Restore rebuilds the exact tree from disk.
- An agent should live **in memory only while it is actively being processed or
  viewed**. When it is quiescent (terminal, or idle/waiting with no imminent
  work) its state lives in a file and it is **unloaded**; it is **reloaded on
  demand** when something needs it (a view request, an inbound message, a child
  completing that its parent must observe).

This is demand-paging for agents. The existing persistence (`meta.json` +
`context.json`) and recovery (`recovery::reload_one` = `build_agent` +
`restore_agent`) are already a working "load from disk" path — reused as the
page-in mechanism. Unload is its inverse: flush, despawn, erase host maps.

## Phasing

### Phase A — deterministic tree persistence + exact restore

Make the tree a stored fact and rebuild it precisely on startup recovery.

**Persist** (new `meta.json` fields on `RunMeta`, all `#[serde(default)]` so old
runs load):
- `children: Vec<String>` — the run-ids of this agent's direct sub-agents.
- `depth: usize` — this agent's depth in the tree (0 for a root).
- `max_child_depth: usize` — the sub-agent depth cap carried on
  `SubAgentChildren`.

Populated from live components at snapshot time: `children` from the parent's
`SubAgentChildren` (each child entity → its `RunMetadata.run_id`); `depth` /
`max_child_depth` from the agent's `ParentRef` / `SubAgentChildren`. (Equivalent
alternative: populate `AgentState.spawned_children_ids` — currently a dead
always-empty field — at `spawn_child` / fan-out `start_worker`, and copy it into
`RunMeta.children` in `build_run_meta`.)

**Restore** (two-pass in `recovery`):
1. Reload every persisted agent (existing loop), building `run_id → Entity`.
2. Re-link: for each child with a `parent_run_id` present in the map, insert
   `ParentRef { parent_entity, parent_agent_id, depth }`; for each parent,
   rebuild `SubAgentChildren { children, max_child_depth }` from its persisted
   `children`. A `parent_run_id` / child whose counterpart is absent is logged
   and left unlinked (never silently proceeds as "done").

**Fan-out** (`FanOutWaiting` is not serializable): persist a serializable
`FanOutState { pending, active_run_ids, summaries, failures, merge_stage }` and
rebuild `FanOutWaiting` after run-ids are mapped to entities, so an interrupted
split/merge resumes deterministically. (May land as A2 if A1 ships the
sub-agent-tool tree first.)

Outcome: no orphans, no silent-proceed, no heuristics — restart resumes the
exact tree.

### Phase B — unload terminal agents + reload-on-demand

- When an agent is terminal (`Complete`/`Error`/`Cancelled`), its final snapshot
  is flushed and its terminal state has been emitted once, **unload** it:
  `world.despawn(entity)` and erase its `by_run_id` / `emitted` /
  `emitted_interactions` entries. Disk history is untouched.
- Any host operation that targets a run-id not in `by_run_id` (a dashboard/API
  view, a `send_message`, a `cancel`) first **pages it in** via `reload_one`
  from disk, then proceeds. A miss that isn't on disk is a genuine 404.
- A parent waiting on children must be paged in when a child terminal event
  needs to wake it — the Phase-A tree links tell us which parent to reload.

Outcome: memory is bounded by the number of *active/viewed* agents, not the
total ever run.

### Phase C (fast-follow) — page out idle/waiting agents

Extend unload to non-terminal but quiescent agents (parked on a human
interaction, or a parent blocked on long-running children): flush + despawn, and
page back in on the waking event (interaction response, child completion,
inbound message). This realizes "in memory only while actively processed or
viewed" fully. Deferred from the v1 cut so Phases A/B can land and bound memory
first.

## Invariants / risks

- **Never lose the merge.** A parent must not transition past `requires_children`
  or a fan-out merge unless its children's results are accounted for — enforced
  by rebuilding the gate components before the schedule runs post-recovery.
- **Unload ordering.** A child may only be reaped once no live ancestor still
  queries it (parent terminal, or past its wait). Reaping walks `ParentRef`.
- **Page-in races.** Reload-on-demand must be serialized against the tick loop
  (host owns the world; page-in happens between ticks, like spawn/reload today).
- **Idempotent restore.** Re-linking checks for existing components so a
  double-recovery can't duplicate children.
