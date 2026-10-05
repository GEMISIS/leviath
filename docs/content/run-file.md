---
title: The run file
description: What a run's file holds (spec, steps, state checkpoints), the files it names beside it, how a run resumes from it, and what happens when the machine changed.
group: Concepts
group_order: 2
order: 14
---

# The run file

A run that lives only in memory is lost when the machine restarts, and a run spread over many
files can come back half-restored. Leviath records each run in one file, `run.lvr`, in the run's
directory under `~/.leviath/runs/<run-id>/`. Everything needed to resume the run, or to look at
any point of its history, is in that file or named by it. Its answer, its logs and the files it
stored are kept beside it as plain files, so the run file stays small and they stay easy to open.

```bash
lev run show release-notes-1790848443-51af34ec01cd             # the spec it started from
lev run show release-notes-1790848443-51af34ec01cd --at 4      # its whole state after step 4
lev run show release-notes-1790848443-51af34ec01cd --deltas 3..5   # what steps 3 to 5 changed
```

## What is in it

A run file is a list of frames, written in order and only ever appended to. It holds four kinds of
thing.

| Part | Written | What it holds |
|---|---|---|
| The **spec** | once, first | Everything decided when the run was resolved |
| **Code** | once each | The scripts the spec names, by digest |
| **Deltas** | one per step | What changed in that step, and what happened |
| **State checkpoints** | every so often | The run's whole state, written out in full |

```mermaid
flowchart LR
  H["Header: LVR2 and the format's fingerprint"] --> S["Spec"]
  S --> C["Code"]
  C --> D1["Deltas 1 to k"]
  D1 --> K1["State at step k"]
  K1 --> D2["Deltas k+1 to n"]
  D2 --> K2["State at step n"]
```

### The spec

The **run spec** is the [spawn request](/docs/starting-a-run) after the daemon resolved it against
this machine. It is the run's starting point, and nothing in it is decided again.

| Field | What it records |
|---|---|
| `graph` | The run graph: stages, edges, layout and declared inputs |
| `inputs` | Every input's checked, typed value |
| `stages` | Per stage: provider, model, window, fallbacks, and each tool's definition |
| `seeded` | What each region held at spawn, from inputs and seeds |
| `code` | The digest of every script the graph names |
| `launch` | The [launch policy](/docs/starting-a-run#launch-policy) the run got |
| `placement` | Its workdir, the run that started it, and its depth in the tree |
| `delivery` | Its webhook and your labels; a signing secret is named by where it is kept, never held |
| `env` | A fingerprint of the providers and MCP servers it used |
| `origin` | Where its graph came from: a blueprint and its digest, or a raw request |

Because tool definitions and seeded contents are stored, a run does not change underneath you
when someone edits the blueprint, a script or an MCP server's tool list later.

### The webhook's secret

A run file never holds the secret its webhook is signed with. When a run is spawned, the daemon
keeps the secret in the **secret store**: `secrets/` in the data root, beside `runs/`. It is one
file per secret, readable by you alone. The spec's `delivery.callback.signed_with` holds only the
file's name, the run's id followed by random hex.

So a copy of `run.lvr` carries no secret, wherever it goes: an upgrade backup, a bug report, another
machine, an agent reading the run's history. The server reads the secret from the store when it
signs the webhook.

The secret is kept as long as its run is, because a finished run's webhook can still be sent:
after a restart, or when the run finishes again. Deleting the run deletes its secret. When the
daemon starts, it removes any secret whose run directory is gone.

### Deltas

Each step of a run writes one **delta**. It names the fields of the run's state that changed, with
their new values, and the **events** that happened in between. Replaying a run's deltas in order
from its start gives its state at any step.

| Event | Recorded when |
|---|---|
| `Attempt`, `Inference`, `Failover` | A model call finished, or moved to another model |
| `ToolStarted`, `Dispatched` | A tool call started, as an execution with its own id |
| `ToolFinished`, `Completed`, `Artifacts` | A tool call ended, with its result and any files |
| `Answered`, `Settled` | A person answered a question |
| `Message` | A message reached the run |
| `ContextCommitted`, `ContextNoted` | The context changed, with its cause |
| `Log` | A line for the run's log |

### State checkpoints

A **checkpoint** is the run's whole state, written out. That is where it is in its graph, what its
pipeline is doing and every region's entries. It also holds open questions, tool calls in flight,
a fan-out in progress, spend, time and child runs. So do the approvals a person granted it for
the run or a stage, and the bytes it has written. It is exactly what
[`lev run show --at`](/docs/inspecting-a-run#from-the-command-line) and `Run.state` return.

The daemon writes one after 64 deltas, or once the deltas since the last one take more than twice
its size, whichever comes first. Reading the current state then means reading the last checkpoint
and replaying a few deltas after it. Each frame ends with its own length, so a reader finds the
last checkpoint by walking back from the end of the file rather than reading all of it.

On a scripted run of 120 tool-calling turns that compacted its context every ten, the file came to
67 KB. Every frame is compressed, and code is stored once however often it is used.

Code is the only content the file holds itself. A resume binds exactly the scripts the run was
resolved against, whatever has become of the files they were read from, so the spec's code travels
with the spec.

### Beside the file

The rest of what a run keeps is in plain files in its directory, and the run file names each of
them rather than holding a copy:

| File | What it holds |
|---|---|
| `final_output` | The answer the run handed back |
| `stages/<n>/output.log` | What the model wrote in stage `n` |
| `stages/<n>/logs.log` | Stage `n`'s tool activity and events |
| `stages/<n>/taint_audit.json` | The taint gate's decisions in stage `n` |
| `blobs/<sha256>` | A file the run was given or a tool stored, named by its digest |

The state's `files` names the answer and each stage's files. Each name is a path relative to the
run's directory and the file's size when the step was written. For a file written whole, the answer
and an audit, it also carries the sha256 of its contents. A log is only appended to, so it may be
longer than its recorded size, but never shorter.

The state's `blobs` lists each stored file once: its digest, its type, its size, its name, the
region it first appeared in, and the tool whose result carried it. A stage that wrote no text has
no `output.log`, and a run with no answer names no `final_output`.

Every reader finds these files through those names: `lev result`, the dashboard, the HTTP API,
GraphQL, `lev rage` and the `run_history` tool. A name that leaves the run's directory is never
followed. A file that is not as the run wrote it (changed, or shorter than recorded) is still read,
and the daemon log says so. A stored file the run names that is missing from `blobs/` is refused by
its digest, and a run that names one is not resumed until it is put back. The error names the file.

On a run given a 2 MB image that handed back a 59 KB answer, `run.lvr` came to 12 KB. With the
image copied in, it came to 2.1 MB.

A crash can cut a frame short. Every frame carries a checksum, so the next open finds a torn frame
at the end and cuts it off. The run resumes from its last complete step. `lev run show` leaves the
torn step out too, and says so.

## Resuming a run

When the daemon starts, it brings back every run that had not finished. For each run it:

1. reads the spec and the last state from the run file;
2. checks that this machine still offers what the spec relied on;
3. places the run in the [engine](/docs/engine) as it was.

Nothing is resolved again. The run carries on with the models, tools, inputs and launch policy it
started with, even if the blueprint or your config changed since.

What the run was doing comes back with it:

| It was | On resume |
|---|---|
| Waiting on a model reply | It asks again |
| Running a batch of tool calls | It carries the batch on; no call that finished or was running is run again |
| Choosing its next stage | It is asked again, among the same edges |
| Running a fan-out | It picks its workers back up by run id |
| Asking a person a question | It asks again, under a new id |
| Stopped at a stage checkpoint | It asks again, under the same id and over the same document |
| Paused | It stays paused, and runs a batch it had in flight once resumed |

Each call in a batch is recorded as done the moment it finishes, so a restart part way through
a batch never runs a finished call again. A call that was still running is not started again
either. A clean stop kills its command part way, and a daemon that died may have left it running,
so either way its result says it was interrupted, and the model checks whether it took effect
before running it again. A fan-out worker that finished while the daemon
was down is read from its own run file, and counts as done or failed as it ended.

A run that has finished stays finished. It reads no more messages, and there is no way yet to send
it on to another stage of its graph.

Child runs come back before the runs that started them, so a parent waiting on its fan-out finds
its workers in place. Among runs at the same depth, those with work to do come first. A run you
cancelled stays stopped through a restart, and comes back paused only when something asks for it.

## When the machine changed

The spec records a fingerprint of each provider's configuration (its kind, base URL and model
list) and each MCP server's tool list. On resume the daemon compares them with this machine. If a
provider is gone or configured differently, or an MCP server offers different tools, the run does
not go ahead on something it was never checked against.

Instead the resume is refused with typed [issues](/docs/starting-a-run#reading-the-problems), the
same kind a spawn gets. Each one is at the place in the spec that depends on what changed. Here a
run was started, then the `openai` provider's base URL was changed and the daemon restarted:

```text
ERROR leviath_cli::daemon::recovery: a run cannot be resumed on this machine as it stands; holding it run_id=release-notes-1790848481-4774f2f3b6fa issues=2 problems with this spawn:
1. stages.gather.provider: changed: provider 'openai' is configured differently from when the run started: the run was started against a different configuration. put 'openai' back the way it was (its kind, base URL and model list), or start a new run. Known: openai
2. stages.write.provider: changed: provider 'openai' is configured differently from when the run started: the run was started against a different configuration. put 'openai' back the way it was (its kind, base URL and model list), or start a new run. Known: openai
```

The daemon writes that to `daemon.log`, and records the issues in the run file as the run's
`held` state. Nothing else about the run changes. It is **held**, not ended: `lev ps` lists it as
paused, its reason reads `machine changed`, and `lev ps --json` and the API carry each problem in
the reason's remedy. A question it was waiting on is not lost; it is asked again when the run
comes back.

| What changed | Code | Path |
|---|---|---|
| A provider is no longer configured | `unavailable` | `stages.<stage>.provider` |
| A provider is configured differently | `changed` | `stages.<stage>.provider` |
| An MCP server is gone or disconnected | `unavailable` | `stages.<stage>.tools` |
| An MCP server's tools changed | `changed` | `stages.<stage>.tools`, naming tools removed, changed and added |
| A script's bytes are missing from the file | `missing` | `code[<n>]` |
| The webhook's secret is gone from the secret store | `unavailable` | `delivery.callback.signed_with` |

Put the provider or server back the way it was, then restart the daemon, and the run carries on
from where it stopped. `lev resume` tries again without a restart, against the providers
`config.toml` names now. A key that only lives in the daemon's environment needs the restart. A
message to a held run is refused with the same reason, and `lev cancel` ends it. If you would
rather not put the machine back, cancel the held run and start a new one: its spec records the
machine as it is now.

## Runs from older versions

Older versions of Leviath kept a run in several files: an LVR1 journal, `meta.json`,
`context.json`, `stages.json`, `fanout.json`, `interactions.json` and a copy of its blueprint.
The daemon converts such a directory the first time it loads it, when it starts or when something
asks for the run:

- the spec is rebuilt from the run's metadata and the blueprint it ran;
- its code is copied in;
- each journal step that maps onto a delta becomes one;
- the state it was last in becomes the last checkpoint, naming the files beside it;
- a webhook secret in its `meta.json` moves to the [secret store](#the-webhooks-secret), and the
  spec names it there.

The old files move into `legacy/` inside the run's directory rather than being deleted, and a
directory that already holds a run file is never converted twice. Three stay where they are,
because a new run keeps them in the same place and form: the stage logs and audits under
`stages/`, the answer in `final_output`, and the stored files under `blobs/`. The new run file names
each of them, so the dashboard, `lev result` and the API read a converted run's logs and answer as
they read a new run's. The whole directory is saved in the home's backup first. See [upgrading from an earlier release](/docs/daemon#upgrading-from-an-earlier-release).
An old run did not record everything a run file holds. Each value the conversion had to fill in goes
to the run's own log, with the value used and why, and `daemon.log` gets one line for the whole pass.

A run whose blueprint is gone, or no longer reads, is converted from what it recorded. Its spec's
origin is `recorded`, and its graph holds the stages it entered, the models they ran on, the edges
it took and its regions. That is enough for `lev ps`, `lev run show`, `lev stages`, `lev timeline`
and `lev result`, but not to run it, so it never resumes.

An old run never recorded a machine fingerprint, so its resume does not compare one. It comes back
on whatever providers this machine has under the same names.

## Reading a run file

`lev run show` reads a run's file directly, so it works for a finished run and with the daemon
stopped.

| Command | Shows |
|---|---|
| `lev run show <run>` | The spec |
| `lev run show <run> --at <seq>` | The whole state after step `seq`; step 0 is the state it started in |
| `lev run show <run> --deltas 3..7` | Steps 3 to 7: `3..` from 3 on, `..7` up to 7, `..` all |
| add `--json` | JSON instead of TOML |

Here is a slice of a delta, the step where a tool call came back:

```toml
[[delta]]
at = 1790848532
seq = 4

[[delta.changes]]
Phase = "ReadyToInfer"

[[delta.events]]

[delta.events.ToolFinished]
call_id = "call_1"
millis = 0

[delta.events.ToolFinished.result]
is_error = false
text = "..."
```

The same data is on the HTTP API, GraphQL, and in the `run_history` tool an agent can call.
[Inspecting a run](/docs/inspecting-a-run) shows each.

The file's frame types have a published JSON Schema, `docs/schema/run-file.schema.json` in the
repository. Its hash is the fingerprint in every run file's header. A build whose frame types
differ refuses a file by name rather than reading it into the wrong shape.
