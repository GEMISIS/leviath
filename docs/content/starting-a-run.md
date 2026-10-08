---
title: Starting a run
description: The one spawn request every front door takes, its typed inputs, launch policy and delivery, and how to check it and read its problems.
group: Concepts
group_order: 2
order: 2
---

# Starting a run

A run that starts with a typo in its inputs fails late, after it has spent tokens, and often for a
reason that is hard to trace. Leviath checks a run before it starts. Every way of starting one
sends the same typed request, and every problem in it comes back at once, each with the place it
is about.

That request is a **spawn request**. The CLI, the [HTTP API](/docs/api), [GraphQL](/docs/graphql),
the [Agent Client Protocol](/docs/agent-client-protocol), the [dashboard](/docs/dashboard), an
[embedding program](/docs/embedding) and an agent's own [`spawn_agent`](/docs/sub-agents) tool all
build one. A [blueprint](/docs/blueprint-format) only fills part of it in.

```bash
lev run release-notes --input since=v0.6.0 --input audience=developers
```

That line names an installed blueprint and gives it two inputs. The rest of this page is what the
request holds and how the daemon reads it.

## One request, every front door

Here is the same request written four ways. It names the `release-notes` blueprint (the complete
file is on [Writing a blueprint](/docs/blueprint-format#a-complete-blueprint)), gives it two inputs,
runs it unattended with no child runs, and labels it with a ticket.

As a file for `lev run --request`, in TOML:

```toml
[source.blueprint]
name = "release-notes"

[inputs]
since = "v0.6.0"
audience = "developers"

[launch]
unattended = "all"
max_depth = 0

[delivery.metadata]
ticket = "REL-12"
```

As JSON, the body of `POST /api/runs` (or of `lev run --request request.json`):

```json
{
  "source": { "blueprint": { "name": "release-notes" } },
  "inputs": { "since": "v0.6.0", "audience": "developers" },
  "launch": { "unattended": "all", "max_depth": 0 },
  "delivery": { "metadata": { "ticket": "REL-12" } }
}
```

As GraphQL, where each input is a name and a typed value:

```graphql
mutation {
  spawnRun(request: {
    source: { blueprint: { name: "release-notes" } }
    inputs: [
      { name: "since", value: { text: "v0.6.0" } }
      { name: "audience", value: { text: "developers" } }
    ]
    launch: { unattended: { all: true }, maxDepth: 0 }
    delivery: { metadata: [{ key: "ticket", value: "REL-12" }] }
  }) {
    ... on SpawnedOutput { runId }
    ... on SpawnRejectedOutput { issues { path code message expected got hint known } }
  }
}
```

On the command line, where each flag lands on the same request:

```bash
lev run release-notes --input since=v0.6.0 --audience developers --yolo --max-depth 0
```

`--input name=value` gives any input, and `--<name> value` is its short form. The CLI has no flag
for `metadata`; put it in a `--request` file and give the other flags beside it.

A request names only what it needs. Everything left out takes its default, and a key the request
format does not have is refused rather than ignored. `lev schema spawn-request` prints the request's
JSON Schema, and `GET /api/schema/spawn-request` serves the same document.

| Field | What it holds |
|---|---|
| `source` | What to run: a blueprint, or a whole graph. |
| `inputs` | Values for the inputs the graph declares, by name. |
| `attachments` | Files: for `file` inputs, or placed straight into a region. |
| `model` | A model for every stage that allows it, as `{ provider, model }`. |
| `output` | The shape of the final answer, over the graph's own. |
| `workdir` | Where the run's tools work. |
| `launch` | How much the run is trusted with. |
| `delivery` | A webhook to call at the end, and your own labels. |

## Blueprint or raw graph

`source` says what the run is. It takes one of these forms.

| Form | JSON | Who may send it |
|---|---|---|
| An installed blueprint | `{"blueprint": {"name": "release-notes"}}` | Anyone |
| A blueprint pinned to one revision | `{"blueprint": {"name": "release-notes", "digest": "3f9a..."}}` | Anyone |
| A blueprint directory that is not installed | `{"blueprint_file": "/home/ana/agents/release-notes"}` | Callers on this machine |
| A whole graph | `{"raw": { "stages": [...], "layout": {...}, ... }}` | Anyone allowed to spawn |

A blueprint fills in the run's graph: its stages, edges, regions and declared inputs. A caller
changes that graph only through those inputs, so a blueprint means the same thing every time it is
run. The digest pins the exact file you read. If something else is installed under that name, the
spawn is refused.

A raw graph is the same structure a blueprint's `[graph]` table holds, sent whole. Use it when a
program or an agent writes the run itself. Its stages can seed regions from shell commands and
connect MCP servers, so the `spawn_agent` tool needs the `spawn_raw_graph` permission to send one.
That permission asks a person by default.

`lev run ./path/to/dir` sends a `blueprint_file` source. The HTTP API, GraphQL and the Agent Client
Protocol refuse that form, because a remote caller should not choose files on the daemon's disk.
They also refuse a raw graph whose fan-out names its worker by directory.

## Inputs

A graph declares the values a run takes. Each declaration has a name, a type, whether it is
required, an optional default, and where its value goes. Here are the three inputs the
`release-notes` blueprint declares:

```toml
[[graph.inputs]]
name = "since"
type = "text"
required = true
description = "The tag or commit to start from."
binds = [{ region = "brief", template = "Changes since {since}, for {audience}. At most {max_items} items." }]

[[graph.inputs]]
name = "audience"
type = { kind = "choice", options = ["users", "developers"] }
default = { choice = "users" }

[[graph.inputs]]
name = "max_items"
type = { kind = "int", min = 1, max = 50 }
default = { int = 20 }
```

The request supplies plain values: text, numbers, booleans, lists and tables. The daemon reads
each one through its declared type. `"developers"` becomes the choice `developers`, `"1h30m"` a
duration of 5400 seconds, and `"twenty"` for an `int` becomes a problem with its path. From there
on, nothing in the run handles an untyped value.

Most blueprints declare `task`, a `text` input bound to a region named `task`. That is a
convention, not a rule. `--task` on the command line is shorthand for the `task` input. A
blueprint without one refuses it:

```text
Error: 1 problem with this run:
  inputs.task: unknown: release-notes takes no task. give it its inputs with --input <name>=<value>. Known: since, audience, max_items
```

A run whose graph takes a `task` is refused when every input bound to a region is blank and
nothing is attached. It would start with nothing to do.

To see what a blueprint takes before you start it:

| Where | How |
|---|---|
| CLI | `lev validate <dir>` lists each input and its type |
| HTTP | `GET /api/blueprints/{name}/inputs` |
| GraphQL | `Blueprint.inputs` |
| Inside a run | the [`describe_blueprint`](/docs/inspecting-a-run#from-inside-a-run) tool |

### Input types

The set of types is closed. A value fits one of these or it is refused.

| Type | Declared as | A value | Read as |
|---|---|---|---|
| `text` | `"text"`, or `{ kind = "text", multiline = true, min_len = 1, max_len = 200 }` | `"ship it"` | the text |
| `bool` | `"bool"` | `true` | true or false |
| `int` | `{ kind = "int", min = 1, max = 5 }` | `3` | a whole number in range |
| `float` | `{ kind = "float", min = 0, max = 1 }` | `0.2` | a number in range; a whole number counts |
| `choice` | `{ kind = "choice", options = ["markdown", "json"] }` | `"json"` | one of the options |
| `list` | `{ kind = "list", item = "text", max = 5 }` | `["api", "cli"]` | each item read as `item` |
| `record` | `{ kind = "record", fields = [...] }` | `{ name = "Ana" }` | each field read as declared |
| `file` | `{ kind = "file", accepts = ["image/png"] }` | `"hero.png"` | the name of an attachment |
| `path` | `{ kind = "path", names = "dir", must_exist = true }` | `"src"` | a path inside the workdir |
| `model` | `"model"` | `"openai/gpt-5.5"` | a model, with or without its provider |
| `blueprint` | `"blueprint"` | `"researcher"` | a blueprint name, or `name@digest` |
| `duration` | `"duration"` | `"1h30m"` | seconds, from `90s`, `5m`, `2h`, `1d` or sums |
| `url` | `"url"` | `"https://leviath.dev"` | an http or https URL |

A `record` declares its fields the way a graph declares inputs, each with a name and a type. A
`path` names `"file"`, `"dir"` or `"any"`, and with `must_exist` the path must be there when the run
is resolved. A `path` cannot climb out of the workdir with `..`. A `model` can also be written as a
table, `{ provider = "openai", model = "gpt-5.5" }`.

A default is written tagged with its type, because a default is a checked value rather than a raw
one: `default = { int = 20 }`, `default = { text = "none" }`, `default = { bool = false }`. The
daemon checks a default like any supplied value, and `lev validate` reports one that does not fit.

On the command line every value starts as text, and the CLI reads it by the input's type first:

| Declared type | On the command line |
|---|---|
| `text` | the text, or `@file` for a file's text |
| `bool` | `true` or `false` (also `yes`/`no`, `on`/`off`, `1`/`0`) |
| `int`, `float` | a number |
| `list` | `a,b,c`, or a JSON array |
| `record` | a JSON object |
| `file` | `@path` or a bare path; the file is attached for you |
| anything else | the text, which the daemon checks |

```bash
lev run reviewer --input diff=@change.patch --input tags=api,cli --input photo=@hero.png
```

### Where an input goes

A declaration's `binds` says where its value lands. These are the only places a caller can reach
inside a blueprint's graph.

| Slot | Written | Takes |
|---|---|---|
| A region, as text | `{ region = "task" }` | any type |
| A region, through a template | `{ region = "brief", template = "Since {since}." }` | any type |
| A stage's model | `{ stage_model = "plan" }` | a `model` input |
| A stage's iteration cap | `{ stage_max_iterations = "plan" }` | an `int` of at least 1 |
| A fan-out's worker cap | `{ fan_out_max_workers = "split" }` | an `int` of at least 1 |
| The output format | `"output_format"` | a `text` or `choice` input |
| The output instructions | `"output_instructions"` | a `text` input |

A template names any declared input in braces, so one region can hold several inputs. Write `{{`
and `}}` for a literal brace. An input with no value renders as nothing.

In a region, a list reads one item per line after `- `, and a record one `name: value` per line. A
`file` input lands as a typed [part](/docs/mime) in its region, with its name beside it. An input
with no `binds` of its own is still checked and recorded in the run's spec, and a template can
still name it.

A region marked `required` that an input or a seed fills must be filled at spawn, or the spawn is
refused at that region's path with its `required_message`. A required region nothing fills at
spawn is the run's own to fill, and its stage cannot be left until it is.

### Attachments

`attachments` carries files. Each has a `name`, optional `mime_type` (sniffed from the bytes when
left out), and `data` as base64 in JSON or TOML. A `file` input names one by its `name`. An
attachment with a `region` goes straight into that region instead, as a typed part.

```json
{
  "source": { "blueprint": { "name": "reviewer" } },
  "inputs": { "diff": "--- a/ui/button.tsx\n+++ b/ui/button.tsx\n...", "criteria": "compare with the mockup" },
  "attachments": [{ "name": "mockup.png", "region": "screenshots", "data": "iVBORw0KGgo..." }]
}
```

The bundled `reviewer` keeps screenshots in a region of their own, which takes only images. A
`file` input would name the attachment instead, as `"inputs": { "photo": "mockup.png" }`, and the
attachment would carry no `region`. A file whose type the input or the region does not accept is
refused before the run starts.

Over HTTP you can send the files as `multipart/form-data` instead of base64, one field per file.
See [Attaching files](/docs/api#attaching-files). On the command line, a `file` input's `@path` and
`--attach path:region` both build attachments for you.

## Launch policy

`launch` is what the run is trusted with. A caller asks for it, and the daemon decides what the run
gets.

| Field | Default | What it does |
|---|---|---|
| `unattended` | `"off"` | `"off"`, `"all"`, or `{ profile = "careful" }`: which prompts go ahead without a person |
| `allow` | `[]` | Tools approved for this run without asking |
| `max_depth` | the graph's `max_child_depth`, then the operator's | How deep the run's tree of child runs may grow |
| `seed_commands` | `true` | Whether region seeds that run a shell command may run |
| `capture_model_input` | `false` | Record every request sent to the model |

`unattended = "all"` is what `--yolo` sends, and `{ profile = "careful" }` is `--yolo=careful`. A
profile names a section of `yolo.toml`, described in [Yolo profiles](/docs/yolo).

The operator's settings bound every request. Seed commands run only when the operator allows them,
whoever asks. A server started with `--no-remote-yolo` refuses `unattended` and `allow` from the
network, as an issue at that path.

### A child never gets more than its parent

A run started by another run, through `spawn_agent` or a fan-out, asks with its own `launch`. The
daemon narrows that request against its parent's policy:

| Field | The child gets |
|---|---|
| `unattended` | the less trusting of the two; a profile is less trusting than `all` |
| `allow` | only tools both the child asked for and the parent has |
| `max_depth` | at most one less than the parent's |
| `seed_commands` | only when both allow them |
| `capture_model_input` | what the child asked for |

A parent whose depth allowance is used up cannot start a child at all. The refusal is an issue at
`launch.max_depth` that says so. No chain of runs can widen its own trust, however it is
written.

## Delivery

`delivery` says who hears about the run when it ends, and carries your own labels.

```toml
[delivery.callback]
url = "https://example.com/hooks/leviath"
secret = "whsec_..."

[delivery.metadata]
ticket = "REL-12"
team = "platform"
```

`callback` POSTs to `url` when the run finishes. With a `secret`, the body is signed with
HMAC-SHA256 so the receiver can check it. [Spawning with a signed
webhook](/docs/api#spawning-with-a-signed-webhook) shows how to verify it. The secret never
reaches a log line.

The POST is sent by [`lev serve`](/docs/api), whichever front door started the run. A run started
with `lev run --request` or over ACP gets its callback only when a `lev serve` is running on the
same machine as the run finishes. With no server running, nothing is sent and the run says nothing
about it.

`metadata` is a table of text labels: a ticket, a tenant, a batch. Nothing in the run reads them.
They come back unchanged with the run, and the run search looks through them.

## Check before you start

Every front door can resolve a request the whole way without starting it. The daemon reads the
blueprint, checks every input, picks each stage's model and tools, and decides the launch policy,
then stops. You get either a summary of the run it would start, or every problem it found.

| Where | How |
|---|---|
| CLI | `lev run ... --check` (add `--json` for the summary as JSON) |
| HTTP | `POST /api/runs/validate`: 200 with a summary, or 422 with issues |
| GraphQL | `validateSpawn(request:)`, answering `SpawnSummaryOutput` or `SpawnRejectedOutput` |
| ACP | the `_leviath/validate_spawn` extension method |
| Embedding | `world.validate(request)` |
| Inside a run | the `validate_spawn` tool, which takes `spawn_agent`'s arguments |

```text
$ lev run --request request.toml --check
release-notes would run (blueprint release-notes@a4adce5110b52cc26e95bb6ef879985f2253229b85fcaa3b02842ab287dae61a, version 0.1.0)
  starts in: gather
  workdir:   /home/ana/project
  unattended: yes
  max depth: 0
  inputs:
    audience = developers
    max_items = 20
    since = "v0.6.0"
  stages:
    gather  anthropic/claude-sonnet-5-5  tools: read_file, shell, context_append
    write  openai/gpt-5.5  no tools
```

`max_items` was never sent, so it shows its default. A check costs no tokens and starts nothing,
so run it whenever a request is new. A run that may never finish is reported in a banner above
the summary, naming the stages it cannot get out of. See [a run has to be able to
end](/docs/stages#a-run-has-to-be-able-to-end).

## Reading the problems

A refused request comes back as a list of **issues**, never one at a time. Each one has:

| Field | What it says |
|---|---|
| `path` | Where in the request it is: `inputs.max_items`, `source.raw.stages.plan.model` |
| `code` | What kind of problem it is (table below) |
| `message` | One sentence saying what is wrong |
| `expected` | What would have been accepted, when that is one thing |
| `got` | What arrived instead |
| `hint` | How to fix it, when there is more to say |
| `known` | The valid choices: declared inputs, options, stage names |

The CLI prints one issue per line:

```text
$ lev run release-notes --input since=v0.6.0 --input audiance=developers --input max_items=twenty --check
Error: 2 problems with this run:
  inputs.audiance: unknown: --input names no input this run takes. Known: since, audience, max_items
  inputs.max_items: wrong type: this is not a whole number (expected an integer 1 to 50; got "twenty")
```

The CLI reads what it can before it asks the daemon, so these two came from the command line
itself. The daemon's own checks, such as a choice that is not one of the options, come back in the
same form.

HTTP answers 422 with the same issues as JSON. `at` is the path as text, and `path` is the same
path one step at a time:

```json
{"issues": [{
  "at": "inputs.max_items",
  "path": [{"Field": "inputs"}, {"Key": "max_items"}],
  "code": "out_of_range",
  "message": "0 is out of range",
  "expected": "an integer 1 to 50",
  "got": "the integer 0",
  "hint": null,
  "known": []
}]}
```

GraphQL returns them as `SpawnRejectedOutput.issues`, with the code as an enum such as
`OUT_OF_RANGE`. A refused request is an answer, not a GraphQL error. An agent gets them as a
numbered list in its tool result. The Agent Client Protocol puts them in the `data` of an
`invalid params` error.

| Code | Means |
|---|---|
| `missing` | Something required was not given. |
| `unknown` | A key or name nothing declares. |
| `wrong_type` | A value of the wrong type. |
| `out_of_range` | A number, list or text outside its declared bounds. |
| `invalid` | A value without the shape its type needs: a bad URL, a path leaving the workdir. |
| `duplicate` | The same name declared twice. |
| `dangling` | A reference to a stage, region, edge, input or attachment that is not there. |
| `conflict` | Two settings that cannot both hold. |
| `not_allowed` | Something this caller may not ask for. |
| `unresolvable` | Something this machine cannot provide: a blueprint, a model, an MCP server. |
| `unavailable` | A resumed run names something this machine no longer has. |
| `changed` | A resumed run names something that changed since it started. |
| `may_never_finish` | A warning, never a refusal: stages the run can reach and never leave. |

`unavailable` and `changed` only come from a resume. `may_never_finish` only comes in a spawn's
or a check's `warnings`, beside a run that starts. [The run file](/docs/run-file#when-the-machine-changed)
shows one.

Read `path` first. It names the field to change, and `known` usually holds the value you meant.
Fix every issue in the list and send the request again. Nothing about it changes between tries.
