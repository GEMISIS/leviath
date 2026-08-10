---
title: External work queues
description: Poll lev ps --all --json to tell a live run from a dead one, without leaking slots or cancelling work that is still going.
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

> [!TIP]
> **Building a service rather than a script? Use the [HTTP API](/docs/api), not the CLI.** Shelling
> out to `lev` costs a process per check, gives you no way to filter server-side, and makes you
> poll for something the daemon can push. See [Prefer the API](#prefer-the-api) below.

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
  "runs": [{ "run_id": "coder-1785568852-8b48c1e0f3a2", "status": "Active", "last_progress_at": 1785568852 }],
  "finished": [{ "run_id": "coder-1785568700-2f91d4a07b6c", "status": { "Error": { "message": "HTTP 402 Payment Required" } } }],
  "not_running": [
    { "run_id": "coder-1785568100-c05e7d2891fa", "status": "complete", "updated_at": 1785568600, "abandoned": false }
  ]
}
```

Note the casing. Entries in `runs` and `finished` carry the daemon's own status
(`Active`, `Waiting`, `Complete`, and `Error` as an object with a `message`), while `not_running`
entries come from `meta.json` on disk and use lowercase (`complete`, `error`). Match on both.

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

## Prefer the API

The CLI recipe above is the right shape for a shell script or a CI step. For a long-lived service,
the [HTTP API](/docs/api) is the better surface, and it answers the same questions with less work.

- **`GET /api/runs` is paginated, sortable, and searchable.** Ask for the runs you care about
  instead of listing everything and filtering client-side. Paging is keyset: follow `next_cursor`
  until it comes back null, rather than counting pages.
- **Poll less by asking for less.** `ids=a,b,c` fetches exactly the runs your queue is tracking,
  and `fields=run_id,status,last_progress_at` trims each one to what you read. Ids that no longer
  exist come back under `missing` rather than failing the whole request.
- **`since=` beats deep paging** when you only want what changed since your last check.
- **Or stop polling.** The `/ws` WebSocket pushes status changes as they happen, so your reconciler
  reacts instead of sweeping. Keep the poll as a slow backstop for missed events.
- **Completion can come to you.** A run started with a callback URL fires a signed webhook when it
  finishes, with a stable `delivery_id` to deduplicate on. See [the API guide](/docs/api).

The same `daemon_reachable` rule applies: a request that fails to reach the daemon is not evidence
about any run.

## Two practical notes

`lev ps --all` reads every run directory, and nothing deletes them, so it gets slower as the runs
directory grows. Poll it less often than plain `lev ps`.

A run can also get stuck in a state no part of the engine can reach, where it has genuinely stopped
but still reports as `running`. `[limits] wedge_timeout_secs` makes the daemon fail those itself, so
they become ordinary finished runs that the table above already handles. See
[the daemon](/docs/daemon#fail-a-wedged-run-instead-of-finding-it-later).
