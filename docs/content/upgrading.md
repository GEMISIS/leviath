---
title: Upgrading from 0.6
description: What the first daemon start does to a 0.6 home, what you change yourself, and how to go back.
group: Guides
group_order: 4
order: 9
---

# Upgrading from 0.6

The release after 0.6 records each run in one file, reads blueprints written as `agent.toml`, and
starts every run from one typed spawn request, whichever way it arrives. The daemon brings your
home up to date by itself the first time it starts. What you change is anything outside the home
that still speaks the 0.6 formats: blueprints kept elsewhere, and clients of the API.

The [changelog](https://github.com/GEMISIS/leviath/blob/main/CHANGELOG.md) lists every change.
This page is the order to take them in.

## Before you upgrade

Nothing is required. The daemon saves everything it changes before it changes it, and never
deletes that backup. An unfinished run is converted and resumed, as after any restart, so letting
a run you care about finish first is the simplest path but not a necessary one.

Every way of installing `lev` upgrades a home the same way, `cargo install leviath-cli` included.

## What the first daemon start does

Start the new daemon as you always do, or run any `lev` command that needs it. Before it serves
anything it:

1. saves every installed blueprint and every old run directory under
   `~/.leviath/backups/<version>-<time>/`;
2. turns each installed `agent.leviath` into an `agent.toml`, or replaces it with this release's
   copy of a bundled blueprint;
3. converts each old run directory into a run file, keeping the old files in the run's `legacy/`
   folder;
4. resumes the runs that had not finished.

A command waiting on the daemon shows how far along it is, and the first one after it is ready
prints a summary with any setting an old blueprint held that 0.6 accepted but never read.
[Upgrading from an earlier release](/docs/daemon#upgrading-from-an-earlier-release) covers each
step in detail, with timings for a large home.

## What you change

### Blueprints outside your home

A blueprint in one of your `agent_paths`, or in a project's own folder, is converted where it
lives too. One the daemon never sees, such as a manifest in a repository you only run with
`lev run ./path`, you convert yourself:

```bash
lev blueprint migrate ./path/to/agent.leviath -o ./path/to/agent.toml
```

`lev validate`, `lev add`, `lev pack` and `lev run` say the same when they are handed an
`agent.leviath`. Without `-o`, the converted blueprint is printed, with a note for each setting
it left out.

### HTTP clients

Per-run routes live under `/api/runs/{id}/`, and `POST /api/runs` takes a spawn request:
a blueprint and its inputs by name, or a whole run graph. The 0.6 `/api/agents` routes still
answer, marked deprecated, until 0.8.0. Each answer from one carries a `Deprecation` header, so a
client can find the calls it still has to move. See
[routes kept for older clients](/docs/api#routes-kept-for-older-clients), and
`GET /api/schema/spawn-request` for the request's shape.

### GraphQL clients

`spawnRun` takes one `SpawnRunRequest` with a `source`, typed `inputs`, `launch` and `delivery`,
and answers either the run or every issue with the request. Several 0.6 fields are gone; the
changelog lists each with what replaces it. See [GraphQL](/docs/graphql).

### Programs on the control socket or the embed API

The control socket's spawn operation, the embed `SpawnSpec`, and the `spawn_agent` and `fan_out`
tool arguments all take the new request. `_leviath/spawn` over the
[Agent Client Protocol](/docs/agent-client-protocol) takes it too.

## Going back

The backup is the 0.6 home as it was. [Going back](/docs/daemon#going-back) restores it, and says
what an earlier release does with runs started after the upgrade.
