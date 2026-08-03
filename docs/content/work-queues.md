---
title: External work queues
description: How an external work queue should ask whether a run is still going, without leaking slots or cancelling live work.
group: Integrations
group_order: 5
order: 4
---

# Driving Leviath from an external work queue

If something outside Leviath hands out work and tracks which slots are busy, it needs to answer one
question about every run it started: is this still going?

Getting that wrong is expensive in both directions. Decide a healthy run is dead and you cancel real
work. Decide a dead run is healthy and you leak a slot forever. This page covers how to ask, and
three fields that will mislead you if you read them the obvious way.

> [!NOTE]
> **Before this page:** [The daemon](/docs/daemon).
> **In one line:** poll `lev ps --all --json`, act on which list a run appears in, and do nothing at
> all when the daemon is unreachable.

## Three things that are not what they look like

**`updated_at` in `meta.json` is a heartbeat, not progress.** The daemon rewrites a run's metadata
every 30 seconds whether or not the run moved. That is deliberate: a stale timestamp tells you the
daemon stopped, not that the run stopped. So a fresh `updated_at` proves the daemon is alive and
proves nothing about the run.

Read `last_progress_at` instead. It only advances on a new iteration, a new stage, or a change of
status. Two cases where it is absent: runs written before the field existed, and runs whose first
snapshot has not landed yet. A daemon restart resets it, because a reloaded run really is being
re-driven from its saved context.

**`pid` is always 0.** There is no process per run. The daemon hosts every agent in one shared
world, so no run has a process id of its own. The field is still written because it always has
been. You cannot conclude anything from it.

**A finished run leaves the listing eventually.** `lev ps` shows the runs the daemon is holding,
plus any that ended within the last `[limits] finished_retention_secs` (five minutes by default).
After that the row is gone, and a daemon restart forgets it immediately.

So the listing answers "how did this run end" for a few minutes and then stops answering. The record
on disk is permanent. The row in `lev ps` is not.

## The recipe

Poll `lev ps --all --json`. It reports the daemon's live runs, the ones that ended recently enough
for it to still remember, the runs on disk it is not holding, and whether it answered at all:

```json
{
  "daemon_reachable": true,
  "runs": [{ "run_id": "coder-1785568852", "status": "active", "last_progress_at": 1785568852 }],
  "finished": [{ "run_id": "coder-1785568700", "status": "error" }],
  "not_running": [
    { "run_id": "coder-1785568100", "status": "complete", "updated_at": 1785568600, "abandoned": false }
  ]
}
```

For each run your queue thinks is in progress, act on which list it turned up in:

| Where it appears | What it means | What to do |
|---|---|---|
| `runs` | The daemon is driving it | Leave it alone |
| `finished` | It ended just now, and `status` says how | Close the work item |
| `not_running`, terminal `status` | It ended longer ago, or before a restart | Close the work item |
| `not_running`, `abandoned: true` | Disk says running, the daemon is not holding it, and it has not moved in five minutes | `lev cancel <run-id>`, then release the slot |
| Nowhere | The run id was never written, so the spawn failed before creating anything | Release the slot |

In the third row, `updated_at` tells you when it ended, and `status` and `error` tell you how.

`finished` and `not_running` never overlap, so a run appears exactly once and you cannot close the
same work item twice.

## When the daemon does not answer

If `daemon_reachable` is false, act on nothing at all.

A daemon restarting looks exactly like every run dying at once. A reconciler that cannot tell those
two apart will cancel a fleet of perfectly healthy agents. Leviath will not mark anything
`abandoned` while it cannot reach the daemon, and your side should hold off too. Wait for the next
poll.

## Two practical notes

`lev ps --all` reads every run directory, and nothing deletes them, so it gets slower as the runs
directory grows. Poll it less often than plain `lev ps`.

A run can also get stuck in a state no part of the engine can reach, where it has genuinely stopped
but still reports as `running`. `[limits] wedge_timeout_secs` makes the daemon fail those itself, so
they become ordinary finished runs that the table above already handles. See
[the daemon](/docs/daemon#fail-a-wedged-run-instead-of-finding-it-later).
