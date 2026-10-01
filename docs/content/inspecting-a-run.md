---
title: Inspecting a run
description: Read any run's spec, its state at any step, its steps and its graph, from the CLI, REST, GraphQL, or an agent's own tools.
group: Reference
group_order: 3
order: 17
---

# Inspecting a run

When a run does something you did not expect, the question is usually "what did it know at that
moment?" A log line rarely answers that. Every run keeps its whole history in its [run
file](/docs/run-file), so you can read what it started from, what it held at any step, and which
edges it took.

```bash
lev run show release-notes-1790848532-2c61ba4e7682 --at 3
```

That prints the run's whole state after its third step: where it was, what each region held, and
what it was waiting on. The same four views are open to every front door.

| View | What it is | CLI | REST | GraphQL |
|---|---|---|---|---|
| Spec | What the run was resolved to | `lev run show <run>` | `GET /api/runs/{id}/spec` | `Run.spec` |
| State | Its whole state now, or after a step | `--at <seq>` | `GET /api/runs/{id}/state?at=` | `Run.state(at:)` |
| Deltas | What each step changed, and what happened | `--deltas 3..7` | `GET /api/runs/{id}/deltas?from=&to=` | `Run.deltas(from:, to:)` |
| Graph | Stages and edges, with how often each was used | the dashboard's stage explorer | `GET /api/runs/{id}/graph` | `Run.graph` |

A **step** is one numbered delta in the run file. Step 0 is the state the run started in. Each
later step is one change the daemon recorded, so the steps of a run are a complete, ordered history
of it.

## From the command line

`lev run show` reads the run file and nothing else, so it works for a finished run, for a run that
failed, and with the daemon stopped. Its output is TOML, or JSON with `--json`.

```text
$ lev run show release-notes-1790848532-2c61ba4e7682 --at 4
[state]
accepts_messages = true
children = []
inbox = []
interactions = []
phase = "ReadyToInfer"
seq = 4
status = "Active"

[[state.context.regions]]
current_tokens = 14
max_tokens = 164
name = "brief"

[[state.context.regions.entries]]
kind = "Text"
text = "Changes since v0.6.0, for developers. At most 20 items."
tokens = 14
...
```

| Flag | Shows |
|---|---|
| none | The spec: the graph, the checked inputs, each stage's model and tools |
| `--at <seq>` | The whole state after step `seq` |
| `--deltas <from>..<to>` | Those steps, both ends included: `3..`, `..7` and `..` work too |
| `--json` | JSON instead of TOML |

To list a run's steps first, `lev run show <run> --deltas ..` prints all of them. `lev ps` and
`lev ps --all` give the run ids. The [dashboard](/docs/dashboard)'s stage explorer draws a run's
graph.

## Over REST

The [HTTP API](/docs/api) serves the same reads under `/api/runs/{id}`. They answer from the run
file, so they work for any run on disk.

| Route | Answers |
|---|---|
| `GET /api/runs/{id}/spec` | The run spec, as JSON |
| `GET /api/runs/{id}/state` | The state now: the daemon's own view while it holds the run, the file's last step otherwise |
| `GET /api/runs/{id}/state?at=3` | The state after step 3 |
| `GET /api/runs/{id}/deltas?from=4&to=5` | Steps 4 and 5; leave either end out for the first or last |
| `GET /api/runs/{id}/graph` | The stages with their visit counts, and the edges with how often each was taken |
| `GET /api/runs/{id}/children` | The runs this one started, one level down |

```bash
curl -s -H "Authorization: Bearer $LEVIATH_API_TOKEN" \
  http://127.0.0.1:3000/api/runs/release-notes-1790848532-2c61ba4e7682/graph
```

The answer has two lists. Each node is a stage, with `visits` (how often the run entered it) and
`current` (whether the run is there now). Each edge has its `from`, `to`, `name` and `condition`,
and `taken`, the number of times the run file records the run taking it.

## Over GraphQL

On [GraphQL](/docs/graphql), the same reads are fields of `Run`, so one query can take exactly the
parts you need.

```graphql
query {
  run(id: "release-notes-1790848532-2c61ba4e7682") {
    spec {
      inputs { name }
      stages { stage provider model }
      launch { maxDepth }
    }
    state(at: 3) {
      seq
      status
      cursor { stage iteration }
      phase { kind }
    }
    deltas(from: 4, to: 5) {
      seq
      events { __typename }
    }
    graph {
      nodes { stage visits current }
      edges { from to name condition taken }
    }
  }
}
```

`state` with no `at` is the run as it is now. `deltas` returns at most 200 steps per call. Each
step's `changes` and `events` are unions, one type per kind of change and event, so a client asks
only for the ones it handles. `spec`, `state` and `graph` are null for a run with no run file.

`Blueprint.inputs` lists what a blueprint takes before you start one, and `validateSpawn` resolves a
request without starting it. [Starting a run](/docs/starting-a-run#check-before-you-start) covers
both.

## From inside a run

An agent can look before it spawns and read what its children did. Four tools do this, and none of
them starts anything.

| Tool | Takes | Returns |
|---|---|---|
| `describe_blueprint` | `blueprint`: a name, or `{name, digest}` | Its stages, its declared inputs, and a `spawn_agent` call that would start it |
| `spawn_schema` | `part` (optional) | The spawn request's JSON Schema, one named part at a time |
| `validate_spawn` | exactly `spawn_agent`'s arguments | The run it would start, or every problem as a numbered list |
| `run_history` | `run_id`, `view`, `at` | A run in the caller's own tree: its summary, state or edges |

`describe_blueprint` answers with JSON the model can copy from. Its `spawn_with` field is a
`spawn_agent` call with every declared input as a placeholder:

```json
{
  "blueprint": "probe@fec796f35c707cada2ccaddd6150af491f892a1c28966004c8344d632a3316a5",
  "description": "One-stage agent for driving a real daemon against the mock provider",
  "entry_stage": "main",
  "inputs": [
    {
      "binds": [{ "region": "task" }],
      "default": null,
      "description": null,
      "name": "task",
      "required": true,
      "type": { "kind": "text", "multiline": true }
    }
  ],
  "spawn_with": {
    "inputs": { "task": "<task>" },
    "source": { "blueprint": "probe" }
  },
  "stages": [
    { "description": "Do whatever the mock provider asks, then finish", "mode": "autonomous", "name": "main" }
  ],
  "title": "probe",
  "version": "0.1.0"
}
```

`spawn_schema` with no `part` returns the request's top level and the names of its parts. Ask for
`RunGraph`, `StageDef`, `RegionDef`, `InputDecl` or `InputType` by name to see one. The whole
schema runs to some hundred kilobytes, far more than a model needs at once.

`run_history`'s `view` picks what comes back:

| `view` | Returns |
|---|---|
| `"summary"` (the default) | Status, stage, phase, spend, children, the final output, and the last edge taken |
| `"state"` | The whole state, now or at step `at`, as the TOML `lev run show --at` prints |
| `"transitions"` | Each edge the run took: step, from, to, edge and why |

A run can read itself, the runs it started, and theirs. Any other run is refused, so a run cannot
read the work of a run it has no part in. A person reads any run with `lev run show`.

The four tools are [sub-agent tools](/docs/sub-agents), granted with `@subagent` or by name.
