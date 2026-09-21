---
title: GraphQL API
description: The GraphQL endpoint `lev serve` exposes beside its REST routes, with auth, paging, error codes and the published schema.
group: Reference
group_order: 3
order: 5
---

# GraphQL API (`lev serve`)

`lev serve` answers GraphQL at `POST /graphql`, beside the [REST routes](/docs/api). One request
names exactly the fields it wants, at any depth. A field you do not ask for is never read from disk.

This is not a layer over the REST routes. Both surfaces call the same code inside the server, so
neither can drift from the other, and a GraphQL request costs the same single hop a REST request
costs.

Use it when one screen needs several things at once. A fleet view that lists runs with their spend
and whatever is waiting on a person is one request here. Over REST it is a listing plus one request
per waiting run, with the join done in your client.

REST is not going anywhere. For "cancel this run" or "read this file", a single REST route is still
the simplest thing that works.

## New to GraphQL?

If you know REST, the mapping is short.

| REST | GraphQL |
|---|---|
| Many URLs | One URL, `POST /graphql` |
| The OpenAPI spec | The schema, published as [`leviath.graphql`](https://leviath.dev/docs/stable/leviath.graphql) |
| `GET` | A `query` operation |
| `POST`, `PUT`, `DELETE` | A `mutation` operation |
| `?fields=a,b` on one route | The selection set, on every field |
| `?cursor=` and `next_cursor` | `after:` and `pageInfo.endCursor` |
| A status code per failure | 200 with an `errors` array, each entry carrying a code |

Two habits to bring with you. Ask for the fields you render, because everything else is work the
server skips. Read `errors` on every response, because a 200 can still carry a failure for one
field while the rest of the answer is fine.

## Auth

The same bearer token as the REST routes, from `--token` or `LEVIATH_API_TOKEN`.

```bash
curl -s localhost:3000/graphql \
  -H "Authorization: Bearer $LEVIATH_API_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"query":"{ runs(first: 5) { edges { node { id title status } } } }"}'
```

A missing or wrong token is a plain-text `401`, before any query runs. The in-flight cap and the
request deadline apply here too, so this endpoint can answer `503` or `408` like every other one.
See [limits](/docs/api#limits).

## Reading runs

`runs` is a keyset-paged connection. Ask for a page, render it, then pass `endCursor` back as
`after`.

```graphql
query Fleet($after: Cursor) {
  runs(first: 25, after: $after, filter: { statusIn: [RUNNING, WAITING_INPUT] }) {
    edges {
      cursor
      node {
        id
        title
        status
        ageSecs
        usage { promptTokens completionTokens }
        cost { costUsd costIsExact }
      }
    }
    pageInfo { hasNextPage endCursor }
    total
    scanTruncated
    serverTime
  }
}
```

What the envelope tells you:

* `total` is how many runs matched. It is null when `scanTruncated` is true, because a count taken
  from a partial scan reads as fact and is not one.
* `serverTime` is the daemon's clock when the page was built. Pass it back as
  `filter.updatedAt.gte` to poll for what changed.
* `missing` lists ids from a `filter.ids` fetch that name no run here. One dead id never costs you
  the rest of the batch.

Reading one run is the same field:

```graphql
{ runs(filter: { ids: ["coder-1788924523-abc123"] }) { edges { node { id status error } } missing } }
```

### Filtering

`filter` takes the whole predicate in one place, and it is shaped like the run it selects. Every
field you set has to hold, so one filter object is an `and` of its own fields.

| Field | What it selects |
|---|---|
| `ids` | Exactly these runs. On the filter itself it also says which runs to read |
| `status`, `statusIn` | One status, or any of several |
| `waitReason`, `waitReasonIn` | Why a parked run is parked |
| `title`, `task`, `blueprintName`, `yoloProfileName` | A `StringFilter` on that text |
| `blueprint` | One installed blueprint, by name, with an optional digest pin |
| `stageProvider`, `stageModel` | A `StringFilter` on what some stage of the run ran on |
| `unattended` | A `BooleanFilter` on whether approvals resolve without a person |
| `startedAt`, `updatedAt` | A `TimestampFilter` in unix epoch seconds |
| `ageSecs`, `workingSecs` | An `IntFilter` in seconds |
| `costUsd` | A `DecimalFilter` on what the run has spent |
| `scope` | `ALL`, `TOP_LEVEL` or `SUB_AGENTS` |
| `parent` | One run's direct children |
| `descendantOf` | A whole subtree, at any depth, and not the run itself |
| `query` | A case-insensitive substring. No regex, no operators |
| `queryIn` | Where to search. `META` and `FILES` are free; `CONTEXT`, `LOGS` and `JOURNAL` read files |
| `sort`, `ascending` | `STARTED_AT`, `UPDATED_AT` or `LAST_PROGRESS_AT`, newest first by default |

### The scalar filters

One set, used by every filter input in the schema. `StringFilter` takes `eq`, `ne`, `in`, `notIn`,
`contains`, `startsWith` and `endsWith`. `IntFilter`, `DecimalFilter` and `TimestampFilter` take
`eq`, `ne`, `in`, `notIn`, `lt`, `lte`, `gt` and `gte`. `BooleanFilter` takes `eq` and `ne`.

The exact string comparisons are case-sensitive; `contains`, `startsWith` and `endsWith` ignore
ASCII case, as the run search does. A `Decimal` bound travels as a decimal string, because a cost
figure a JSON parser re-rounds is no longer the figure you sent.

A field the run has no value for satisfies nothing. A run with no title matches no `title` filter
and a run whose spend is unknown matches no `costUsd` filter, whichever comparison you asked for.
Wrap the filter in `not` to find those runs.

```graphql
{
  runs(filter: {
    scope: TOP_LEVEL
    ageSecs: { gt: 3600 }
    costUsd: { gte: "1.00" }
    or: [{ status: ERROR }, { blueprintName: { startsWith: "coder" } }]
    not: { title: { contains: "scratch" } }
  }) { edges { node { id title status cost { costUsd } } } total }
}
```

### Combinators

`and`, `or` and `not` take filters of the same type, so a predicate nests as deep as you need.
`and` is a list every member of which has to match, `or` a list at least one member of which has
to, and `not` one filter that must not. An empty `or` list matches no run, which is what an
alternation with no alternatives selects.

`query`, `queryIn`, `sort` and `ascending` describe the listing rather than a run, so they belong on
the filter you pass and are refused inside a combinator.

The deep search sources read two files per stage per run, so treat them as a "search inside runs"
toggle rather than something every keystroke pays for. When the scan gives up, `scanTruncated` says
so.

### What a run holds

The summary fields come from one stat-cached read, so a listing of fifty costs
fifty stats. Everything below reads a file in the run's directory, and only
when you ask for it.

| Field | What it reads |
|---|---|
| `waitReason` | Why a parked run is parked. Already in memory |
| `flags` | Post-hoc diagnostics. Already in memory |
| `stages` | The per-stage ledger: tokens, spend and region peaks |
| `context` | The live window, with region sizes |
| `context { regions { content } }` | The region text itself. Heavy; select it for the regions you draw |
| `finalOutput` | The answer the run submitted |
| `blueprint` | The manifest the run executed |
| `children` | The run's direct sub-agents, paged |
| `treeStatus` | Depth, descendant count and the subtree token roll-up |
| `logs` | A stage's output or operational log, from the end |

`children` is a connection because a two-hundred-worker fan-out would otherwise
be one unbounded response. Nest it to walk deeper, one level per nesting, and
read `hasNextPage` to see a level that was cut. `runs(filter: { parent: })` is
the same walk from the other direction.

`treeStatus` answers "what did this fan-out cost" without walking it: the
roll-up covers every run below, at any depth. A parent that spent little and
whose fifty workers spent a great deal is not a cheap run.

`logs` reads from the end of a stream. `stageIndex` picks one stage, `allStages`
reads them all in order, and `tail` bounds the bytes, capped at 1 MiB per stream
because `allStages` multiplies it by the stage count.

`waitReason.needsAPerson` is the field a fleet view wants. A run waiting on
workers is healthy and resolves on its own; a run waiting on an answer is a row
somebody has to act on. `NEEDS_SETUP` also carries a `blocker` and a `remedy`,
so a client can offer the right fix rather than parse a sentence.

```graphql
{
  runs(filter: { status: WAITING_INPUT }) {
    edges { node {
      id
      waitReason { reason needsAPerson blocker remedy outstanding }
    } }
  }
}
```

### What each stage ran on

A blueprint picks a model per stage and names a list to fall back through, so
one run can honestly have used three providers. `stages { models }` is where
that is answered, stage by stage, in the order each stage reached them.

```graphql
{
  runs(filter: { stageModel: { contains: "opus" } }) {
    edges { node {
      id
      stages { name entered models { provider model } }
    } }
    total
  }
}
```

There is no run-level answer, on purpose. The one on the run record is the entry
stage's resolution and is never rewritten to follow the stages after it, so it
would be right about one stage and silent about the rest. The list on a stage is
what that stage actually ran on: the first entry is where it started, the last is
where it ended up, and one entry means it never moved.

`models` is null, never an empty list, on a stage that has run no inference. A
stage the run never entered, a stage whose first call is still in flight, and a
stage whose only provider could not be reached all answer null, as does every
stage of a run that finished before Leviath recorded this.

`stageProvider` and `stageModel` keep a run when *any* of its stages matches. So
`stageModel: { ne: "gpt-5.5" }` selects runs with a stage that ran on something
else, which is not the same question as "runs no stage of which ran on
`gpt-5.5`" - wrap the filter in `not` for that one.

## Blueprints

Two different questions, and the schema keeps them apart.

* `blueprints` lists what is installed on the machine now.
* `blueprint` on a run is the manifest that run executed, read from the run's
  own copy.

`blueprints` is keyset-paged on the name, like `runs`: ask for a page, render it, then pass
`endCursor` back as `after`. Its `filter` follows the same convention the run filter does -
`and`, `or` and `not` over `name`, `version` and `description` as `StringFilter`s, `query` as
the shorthand for a case-insensitive prefix on the name, and `names` for exact ones.

```graphql
{
  blueprints(first: 25, filter: {
    version: { startsWith: "1." }
    not: { name: { contains: "scratch" } }
  }) { edges { node { name version } } pageInfo { hasNextPage endCursor } total }
}
```

A name in `names` that is not installed lands in `missing` rather than failing the request.

```graphql
{
  runs(first: 1) {
    edges { node {
      blueprintDigest
      blueprint { id name version source digest }
    } }
  }
}
```

A run copies its manifest into its own directory at spawn and records that
copy's digest. So editing or deleting the installed blueprint never changes
what a finished run says it ran, and a daemon restart resumes a run on the
manifest it started with.

Only the manifest is frozen. Scripts it names, such as hooks and validators,
are still read from the installed blueprint's directory.

`source` says which file a blueprint came from, `SNAPSHOT` or `INSTALLED`. A run
recorded before this feature existed has no copy, so it reads `INSTALLED` and
its `blueprintDigest` is null: what it executed is unknown, which is not the
same as "unchanged".

The id is `<name>@<digest prefix>`, not the bare name. Two revisions of one name
are two different objects, so a client that caches by type and id cannot merge a
run's frozen copy with whatever is installed now.

### What a blueprint declares

The whole manifest is readable, one field at a time: a stage's model block, its
tool routing, its checkpoints, its output shape, its hooks, its fan-out, and the
edges out of it with their conditions and gates. So is what the blueprint declares
run-wide: its dependencies, the mime rows it ships, its sandbox, its summarizer,
and what it would like to run unasked.

```graphql
{
  blueprints(filter: { names: ["coder"] }) { edges { node {
    dependencies { name kind required remedy }
    stages {
      name mode
      model { models { provider model } allowUserDefault }
      transitions {
        target { name } condition
        gate { requireRegions { name } requireRegionNames maxAttempts }
      }
      interactionPoints { name prompt style options }
      fanOut { workerStage { name } maxWorkers onWorkerFailure }
    }
  } } }
}
```

Two rules run through all of it, because a manifest is a document rather than a
database.

A setting the author left out is null, even where the daemon has a default for
it. What was written and what the daemon resolves are different questions, and
`Stage.effective` answers the second: the batch hint, the shell hint, the nudge,
the sandbox and taint tracking, each resolved stage over blueprint over this
machine's config. It is what a run spawned now would get, not a claim about a run
that already started.

A region or a stage the manifest names is served as the object it names, resolved
against the blueprint that wrote the name. An edge's `target` is the `Stage` it
leads to, a gate's `requireRegions` are `Region` objects, and a stage's
`toolRouting.defaultRegion` is the region the results land in.

Every one of those keeps the name beside it, because a name can resolve to
nothing. `targetName`, `requireRegionNames` and `defaultRegionName` carry what
the author wrote, whether or not a declaration was found. Two things put a name
there with nothing to resolve. A later edit can remove the region, which is a
blueprint `lev validate` refuses and the daemon will not spawn. And the runtime
carries `conversation`, `tool_results`, `final_output` and `stage_instructions`
whether a manifest declares them or not, so a stage that resets `conversation` is
naming a region with no declaration to read.

A tool stays a name everywhere. A manifest names tools an inventory cannot
describe: an MCP server's, a group token such as `@builtin`, and any tool this
machine does not have. `tools` is what describes the ones that are here.

## Anything by its id

Some things arrive as an id with no type attached: a run id in a webhook payload, a cache key, a
link somebody pasted into a ticket. `node` takes that id on its own and hands back whatever it
names.

```graphql
{
  node(id: "coder-1788924523-abc123") {
    id
    ... on Run { title status ageSecs }
    ... on Blueprint { name version source }
    ... on UpdateJob { status }
  }
}
```

Everything that implements `Node` promises one thing: the id is unique across the whole API, so no
two nodes anywhere share one. That is what lets `node` work out the type for you, and what makes an
id safe to use as a cache key. Seven types make that promise.

| Type | Its id | Example |
|---|---|---|
| `Run` | The run id, the same one every REST route takes | `coder-1788924523-abc123` |
| `Blueprint` | `<name>@<digest prefix>`, one id per revision | `coder@3f9a1c0d8e77` |
| `Script` | `script:<kind>:<name>`, with `@<blueprint>` on the kind for one blueprint's own | `script:tool@coder:summarise` |
| `McpServer` | `mcpServer:<name>` | `mcpServer:docs` |
| `YoloProfile` | `yoloProfile:<name>` | `yoloProfile:careful` |
| `UpdateJob` | The job id `startUpdate` handed back | `update-1788924523-1` |
| `BulkExport` | The job id `bulkExportRuns` handed back | `export-1788924523-0` |

A name on its own is never an id here. A server, a profile and a script are each unique only within
their own kind, so their ids carry the kind as a tag, and a script's carries the blueprint it belongs
to as well. The two job ids belong to the server that minted them, which keeps its jobs in memory, so
nothing answers to them after a restart.

`Model` is deliberately not a `Node`. A model id is the provider's own, and two providers can both
serve `gpt-5.5` and bill to different places, so `openai` and `codex` would answer to one id. Read
`models` and key on the provider and the id together.

An id that names nothing answers `null` rather than an error. A deleted run, an export that expired
past its hour, a blueprint revision this machine no longer has installed and a typo are the same
answer, and all four mean the same thing: it is not here. A tagged id whose tag this server does not
know answers `null` too, so a client written against a newer build degrades rather than breaking.
What does fail is a read that could not answer at all, such as a config file that will not parse,
and it fails the way the listing it would have come from fails.

One thing `node` does not do is reach inside a run. A blueprint revision only a run's own frozen
copy carries is read through that run, and so is an interaction request, whose id is what an answer
names rather than something unique across the fleet.

## Bytes

Bytes never ride a query answer. A run's parts and artifacts come back as
metadata plus a short-lived signed link, and `fileUrl` mints one for any file in
the run's working directory.

```graphql
{
  runs(filter: { ids: ["coder-1788924523-abc123"] }) { edges { node {
    blobs { sha256 mimeType name size stored url }
    artifacts { name mimeType url }
    fileUrl(path: "out/report.pdf", download: true)
  } } }
}
```

A signed link carries its own permission, so it works in an `<img src>` or a
download link, where a header cannot be set. What it is not is your API token in
a URL:

* It opens one path. A link to one run's blob is not a key to another's.
* It lasts five minutes.
* It opens byte routes only. The same grant pointed at a listing is refused.
* The signing key is random per server process and never written down, so a
  restart invalidates every link it handed out.

Links are relative, so they keep whatever host, scheme and port you reached the
server on. A server guessing its own public URL would guess wrong behind a
proxy.

## A run's files and its history

Two questions about files, and they are not the same one.

```graphql
{
  runs(filter: { ids: ["coder-1788924523-abc123"] }) { edges { node {
    recorded: files { entries { name exists } modifiedFilesTruncated }
    onDisk: files(source: WORKDIR, path: "src") {
      parent
      entries { name isDir size mimeType }
    }
    fileContent(path: "out/report.md") { content nextOffset truncated }
  } } }
}
```

`MODIFIED`, the default, is the run's own record of what it changed. It is free,
because it is already in the run's record, and it is a claim about the run rather
than about the disk: `modifiedFilesTruncated` says when the run hit its tracked
file cap, and a path that has since been deleted is listed with `exists: false`
rather than dropped.

`WORKDIR` is what is there now, one directory level per request. Pass an entry's
own path back to go a level down. That bound is the answer to a repository with a
`node_modules` in it, where one request trying to enumerate everything is no
answer at all.

`fileContent` reads text, at most a megabyte at a time, because the answer
travels inside this one. Pass `nextOffset` back as `offset` for the next window,
and the windows concatenate into the file. For bytes, and for anything that is
not text, mint a `fileUrl` instead: a directory, a path outside the run's working
directory, an offset past the end and a file that is not text each answer with
their own code.

`contextHistory` is snapshots of the run's context window, point by point. Each
point carries a whole window, so it is paged harder than the run listing is, and
the region contents are their own field: asking for the shape of fifty windows
does not read fifty windows' text. For why a region moved rather than what it
then held, see [why a region changed](#why-a-region-changed).

```graphql
{
  runs(filter: { ids: ["coder-1788924523-abc123"] }) { edges { node {
    contextHistory(first: 20, descending: true) {
      total
      pageInfo { hasNextPage endCursor }
      edges { node { at stage window { totalTokens maxTokens } } }
    }
  } } }
}
```

## What a run did

A context window says what a model is looking at now. It does not say what the
run tried. `executions` reads the run's journal instead, so it holds the attempts
the window no longer shows: a call a gate refused, one that failed and was
reissued, one a restart cut off.

```graphql
{
  runs(filter: { ids: ["coder-1788924523-abc123"] }) { edges { node {
    executions(first: 50) {
      total
      pageInfo { hasNextPage endCursor }
      edges { node {
        id callId outcome stageIndex iteration dispatchedAt endedAt
        journalPosition
        call {
          __typename
          toolName
          rawArguments
          ... on ShellCall { args { command } }
          ... on WriteFileCall { args { path append } }
          ... on UntypedToolCall { reason }
        }
      } }
    }
  } } }
}
```

One attempt is one execution. A call the model reissued after a failure is a
second execution with its own `id`, which is why these have ids of their own: a
provider is free to reuse its `callId` across a retry, so that field is
correlation rather than identity.

`outcome` is null for three different reasons, and a client must not flatten
them. The attempt may still be running, it may have ended before this build
recorded outcomes, or it may have ended in a way only the result text describes.
`endedAt` tells the first apart from the other two.

`INDETERMINATE` is its own answer, not a missing one. A daemon that died between
dispatch and completion left a call nobody saw the end of. The resume that
carried the run on records that, because a missing completion is not evidence of
success and not a safe thing to retry silently.

`journalPosition` is where the record that dispatched the attempt sits in the
journal, as a byte offset. It only climbs within a run and it never changes, so
it orders executions and names one for as long as the run exists.

### What an execution is connected to

An execution sits inside a stay in a stage and follows from one trip to a
provider, and both of those are things you can ask for rather than pair up
yourself.

```graphql
{
  runs(filter: { ids: ["coder-1788924523-abc123"] }) { edges { node {
    executions(first: 20) { edges { node {
      id stageIndex iteration
      visit { id ordinal enteredAt leftAt inProgress }
      requestedBy { attempt provider model outcome { kind } }
      contextChanges { cause revisionAfter regions { region tokenDelta } }
      producedArtifacts { name mimeType size url }
    } } }
  } } }
}
```

`visit` is the stay, and it is the key to correlate on. `stageIndex` says where an
execution sat, not which stay it belonged to: a stage entered three times has one
index and three visits, and `iteration` restarts on every entry. A visit's id is
minted when the run enters the stage, so it stays put however long the run loops.

`requestedBy` is the trip to the provider whose answer asked for the call. You
cannot work it out from the timeline: a failover means the answer came from a
different provider than the attempt before it went to.

`contextChanges` is what this execution committed to the window, and it is
independent of `outcome`. A call that succeeded may have committed nothing, and a
call that failed may have committed something before it failed, so neither may be
read off the other. Most executions commit nothing: the `context_*` and `todo_*`
tools are the ones that show up, along with anything that wrote a part into a
region of its own. A lane tool's answer landing in the conversation is committed
by the batch, not by the call, and is not attributed to one.

`producedArtifacts` is the files the call produced, read from the journal as it
produced them rather than from the run's answer. The answer holds only the latest
submission's files and says nothing about which call made them, so a submission a
later one replaced would otherwise be invisible.

Each of these is null or empty where the journal did not record the connection,
and never a guess. `visit` is also null past the ledger's per-stage cap of the
earliest 128 stays, where the stay is real and its detail is not kept; the stage's
own roll-ups in `stages` are the complete figures there.

### Results are their own field

One result can be a whole file, so a page of executions carries none of them.

```graphql
{
  runs(filter: { ids: ["coder-1788924523-abc123"] }) { edges { node {
    executions(first: 1) { edges { node {
      result { text bytes truncated parts }
    } } }
  } } }
}
```

Each `result` reads the one record it needs. `bytes` is the whole result's size
and `truncated` says whether `text` is only its head. `parts` names the stored
parts the result carried, whose bytes come from the run's own parts.

### One tool, one argument shape

Every tool takes exactly one argument shape, so each tool has its own type and a
mismatched pair cannot be built. Ask for `__typename`, then select that type's
`args`.

| Field | What it answers |
|---|---|
| `toolName` | The name the model called, before any alias resolution |
| `toolDescription` | What the tool does, where this build knows the tool |
| `rawArguments` | Exactly what the model sent, untouched |
| `args` | The typed reading of those arguments |

`rawArguments` is on every call, typed or not. The typed view is a convenience
over it and never a replacement, because a debugger that could only show the
tidied version would hide the malformed call that caused the bug.

A call comes back as `UntypedToolCall` in two cases, and `reason` says which.
`NO_TYPE_FOR_THIS_TOOL` is an MCP or script tool, which is ordinary.
`ARGUMENTS_DID_NOT_MATCH` is a built-in whose recorded arguments did not fit its
own schema, which is worth looking at.

An alias is typed as the tool it means. `bash` is `shell`, so it comes back as a
`ShellCall` whose `toolName` is still `bash`.

The same interface answers what a person is being asked to approve:

```graphql
{
  openInteractions { runId request { id kind prompt
    toolCall { __typename ... on ShellCall { args { command } } } } }
}
```

## What a run asked

`executions` says what a run tried. It says nothing about the calls a person
had to approve, or the free-form questions a stage asked along the way: once a
tool has read the answer, a granted call looks exactly like one no policy ever
stopped. `interactions` is the only record that this run stopped and asked
somebody at all.

```graphql
{
  runs(filter: { ids: ["coder-1788924523-abc123"] }) { edges { node {
    interactions(first: 50) {
      total
      pageInfo { hasNextPage endCursor }
      edges { node {
        requestId kind tool prompt stage askedAt settledAt
        settlement { outcome approved scope choice text feedback }
      } }
    }
  } } }
}
```

Every field on `settlement` but `outcome` is null unless `outcome` is
`ANSWERED`. Nobody answered a `TIMED_OUT` ask (the hub answered for them once
the run's interaction timeout ran out), a `CANCELLED` one (the run was
cancelled, or the agent that asked it went away), or a `REFUSED` one, so there
is nothing for any of the rest to carry.

`REFUSED` should never appear. It means a request never opened because another
was already open under the same id, so the run was handed the same neutral
answer a cancellation gives it, which a tool approval reads as not-approved. An
id carries the run that raised it, so two of them meeting is a fault in the
server rather than anything about the call. If you see one, the daemon logged it
as an error at the same moment.

`scope` is the one field worth a note against REST. This spells the widest
grant `RUN`; the REST journal and the answer routes write `session` for the
same scope. `ONCE` and `STAGE` spell the same on both sides.

An unattended run asks nobody, so it has no interactions to list. `--yolo`
answers for the person before the question reaches anyone, and an empty list on
a run that plainly did something dangerous means exactly that: nobody was
asked. What a profile waives is `yoloProfiles`, and what a run was started with
is on the run itself.

## What a run's provider calls took

`usage` and `cost` are per call that worked. A call refused three times and
answered on the fourth is billed once, so the time the run spent being refused is
in neither of them. `inferences` is that half, read from the same journal: one
entry per trip to a provider, in the order the run made them.

```graphql
{
  runs(filter: { ids: ["coder-1788924523-abc123"] }) { edges { node {
    inferences(first: 50) {
      total
      pageInfo { hasNextPage endCursor }
      edges { node {
        stage attempt provider model durationMs backoffMs at
        outcome { kind failureKind transient capacity retry }
        digest { systemHash messages tools maxTokens temperature }
        failover { fromProvider fromModel toProvider toModel reason }
      } }
    }
  } } }
}
```

`outcome.kind` is `SUCCEEDED` or `FAILED`, and the four fields beside it are null
unless it failed. `transient` and `capacity` are how the failure was judged at the
time rather than now: what counts as transient is a policy that moves between
releases. `retry` says what the loop did next. `SAME_MODEL` is the same provider
again after a wait, and the next entry's `backoffMs` says how long that wait
really was. `RENEWED_FILES` is an immediate retry that uploaded the files the
request named afresh, so it spends no wait at all.

`digest` identifies a request without carrying it. Two attempts with the same
digest sent the same thing, which is the question a retry raises: a provider that
kept refusing reads differently from a request that kept changing underneath the
run. `systemHash` is opaque, so compare it and read nothing into the value.

`failover` is the move to a different provider, and it is null on almost every
attempt. A retry against the same provider is the next entry, not a move. Where it
is set, the attempt after it went to `toProvider` and `toModel`, and `reason` says
why the first provider was judged unusable. The journal records a move one tick
after the attempt it follows, because the tick loop decides it rather than the
call. This field puts the pair back together, so your client does not have to.

### What one call sent

`modelInput` is the request itself, per attempt. It is off by default, and
`captureStatus` says which of the four states a record is in before you read
anything else from it.

```graphql
{
  runs(filter: { ids: ["coder-1788924523-abc123"] }) { edges { node {
    inferences(first: 50) { edges { node {
      attempt
      modelInput {
        captureStatus bytes sourceContextDigest
        toolCatalogVersion assemblyVersion
        request
        parameters {
          temperature
          maxOutputTokens { ... on MaxTokensCount { tokens } }
          providerParams
        }
      }
    } } }
  } } }
}
```

`RETAINED` means `request` is there. `NOT_CAPTURED` means no body was ever taken,
which is every run nobody asked to capture. `REDACTED` means a body was taken and
then scrubbed, and `EXPIRED` means it was taken and then aged out. The last two
are a different fact from `NOT_CAPTURED`: something existed and is gone.

`modelInput` itself is null for an attempt whose journal recorded none.

Everything beside `request` is recorded whether capture is on or off, because it
costs nothing and answers what `digest` cannot. `parameters` is what the request
really carried after every override and clamp, so `maxOutputTokens` is an
absolute count rather than the percentage a blueprint may have declared.
`toolCatalogVersion` distinguishes two attempts that offered different tools.
`assemblyVersion` moves when the meaning of an assembled request changes, so an
old captured body stays interpretable.

`sourceContextDigest` names the window the request was assembled from, which is
how a captured request joins to `contextHistory`. It is empty when no body was
taken: folding it walks the whole window, and a run nobody asked to capture does
not pay for that. `bytes` is the size of the captured body, so a client can show
what capture cost even once the body is gone, and zero where none was taken.

`request` is Leviath's own request shape, not one vendor's wire body. The adapter
turns it into the vendor's JSON and never hands that back, so serving the vendor
shape would mean rebuilding it, and a rebuilt prompt is not the request that was
sent.

There is no mapping from context regions to places in the request. Assembly does
not keep one: conversation messages carry no region, one region can become
several system blocks, and the blocks are then reordered by cache tier. A mapping
would have to be inferred after the fact, and an inferred one is not evidence.

Turn capture on for a machine with `[observability] capture_model_input`, or for
one run with `captureModelInput` on `spawnRun`:

```graphql
mutation {
  spawnRun(input: {
    blueprint: { name: "coder" }, task: "fix the parser", workdir: "/work",
    captureModelInput: true
  }) { run { id status } }
}
```

Read the warning in [Observability](/docs/observability#capturing-what-went-to-the-model)
first. A captured request holds whatever the run's context held, including file
contents a tool read and anything somebody pasted, and there is no size cap.

## What changed the window

`contextHistory` serves snapshots of the window. `contextChanges` serves the
changes that moved it. Both read the same journal, and neither answers for the
other. A region that lost its plan looks identical in a snapshot, whether a
compaction took it, a transform cleared it, or the model deleted it.

Each entry is one committed transaction, which may touch several regions. A
compaction summarises one region and empties another; a stage edge clears four; a
resume rebuilds every region there is. All of those are one change.

```graphql
{
  runs(filter: { ids: ["coder-1788924523-abc123"] }) { edges { node {
    contextChanges(first: 50) {
      total
      pageInfo { hasNextPage endCursor }
      edges { node {
        cause at journalPosition
        revisionBefore revisionAfter executionId
        regions {
          region digestBefore digestAfter tokensBefore tokensAfter
          tokenDelta entriesAdded entriesRemoved
        }
      } }
    }
  } } }
}
```

`cause` names a path through the runtime rather than a shape of edit. `SEED`,
`MESSAGE`, `MODEL_REPLY`, `TOOL_RESULT`, `PRODUCED_PART`, `COMPACTION`,
`TRANSFORM`, `CONTEXT_TOOL`, `HOOK`, `FAN_OUT`, `INTERACTION`, `RESUME` and
`FRAMEWORK` are the whole vocabulary. Two paths that both append to the
conversation stay two causes, because which of them ran is the question being
asked.

`revisionBefore` and `revisionAfter` name the window either side of the change.
Pass either to `contextSnapshot` to read exactly that content. `executionId` is
the tool execution that committed it, and is null for every change made outside a
tool call: the model's own reply, a compaction, a transform, a resume, a nudge.

A change carries no content, because the snapshot recorded on the same tick
already holds the text. The per-region digests are what tell you whether you need
to go and read it: `digestBefore` equal to `digestAfter` means that region ended
the transaction holding what it started with. `tokenDelta` is negative where a
region shrank, and `entriesRemoved` counts any eviction the change triggered.

An empty list means the journal holds no change records. A write whose path cannot
name its cause records nothing rather than borrowing the nearest neighbour, so a
gap here reads as a gap rather than as a wrong answer.

## A window by name

A window's revision is a content address: it is derived from what the window
holds, so it names that content for ever. Reading one back is immutable. No later
write can change what a revision means, because a write produces a different
revision, and `contextSnapshot` therefore resolves one to exactly the content it
was minted from - never to whatever the run holds now.

```graphql
{
  runs(filter: { ids: ["coder-1788924523-abc123"] }) { edges { node {
    context { revision totalTokens }
    contextSnapshot(revision: "cw1-4f2a9c8e5b1d7063a4e2f8c19d0b6537") {
      at stage
      window { revision totalTokens regions { name tokens } }
    }
  } } }
}
```

Null means this run never held that window, which is also what a revision from
another run looks like. Two points holding identical contents share a revision -
that is what content addressing means - and the read answers with the first time
the run held it. The stage is not part of the identity: `stage` and `at` say where
and when.

## The machine itself

Three catalogues, sized by what you configured rather than by what has piled
up, so they are plain lists with no paging.

```graphql
{
  models { id provider maxContextTokens limitsSource pricing { inputPerMtok } }
  providers { id display enabled signedIn account }
  tools {
    tools {
      name origin description arguments
      ... on ScriptTool { path blueprint requires }
    }
    groups { name description }
    skipped { path reason }
  }
}
```

* `models` answers from the catalogue this server keeps, so it costs no
  provider call. Pass `refresh: true` to ask the providers again and wait.
* Two providers can serve the same model id and bill to different places, so
  `provider` is part of each model rather than something you infer.
* `enabled` and `signedIn` are different questions. A provider can be turned on
  with no credential stored, and a credential can outlive the config entry that
  used it.
* `Tool` is an interface: `BuiltinTool`, `SubagentTool` and `ScriptTool`. A
  script always has a file, and a built-in never does, so the file is a field
  on the one that has it rather than a null on both. MCP tools are in none of
  them: they depend on a server being reachable rather than on anything
  installed here.
* `config` is the whole server settings view, with every secret left out:
  `configuredProviders` names which providers have a key, never the keys. Read
  `capabilities` there before choosing a code path.
* `doctor` runs the environment checks. A failing check is `ok: false` inside a
  healthy answer, never an error: the request succeeded, and what it found is
  the answer.
* `mcpServers`, `yoloProfiles`, `mime` and `scripts` are what is installed.
  `yoloProfiles` says which file it read and whether that file exists, so "no
  profiles yet" reads differently from "the file is broken".
* `directories(path:)` is the file picker. It is confined to `--workdir-root`
  when the operator set one, which is why `parent` is null at that fence rather
  than leading above it.
* `tools(blueprint: { name: "coder" })` scopes the inventory to one blueprint's
  own tools directory, which is what an editor offering `available_tools` wants.
  `skipped` names scripts that were found and could not be offered, with the
  reason, because a tool an author believes exists and silently is not there is
  the failure worth reporting.

## Starting and steering a run

Every mutation answers with the run as it is afterwards, so you never have to
guess whether the act landed, and you do not need a second request to find out.

```graphql
mutation {
  spawnRun(input: {
    blueprint: { name: "coder" }, task: "fix the parser", workdir: "/work"
  }) {
    run { id status task }
    warnings
  }
}
```

`warnings` names checks the blueprint declared that your own output shape
retires. Empty when there are none.

A blueprint argument is an object rather than a name, so it can carry the
revision you mean. Add the `digest` you read off `Blueprint.digest` and the spawn
is refused with `CONFLICT` if something else is installed under that name. Send
the name alone and the spawn takes whatever is there, which is what a fleet view
starting somebody's current blueprint wants.

Three refusals here are the server's, not the daemon's, and each answers
`FORBIDDEN`: a workdir outside `--workdir-root`, `yolo` on a server started with
`--no-remote-yolo`, and a `callbackUrl` the outbound policy will not allow.

```graphql
mutation { sendMessage(runId: "coder-1788924523-abc123", message: "keep going") { run { status } } }
```

```graphql
mutation { pauseRun(runId: "coder-1788924523-abc123") { run { id status } } }
```

* `pauseRun`, `resumeRun` and `cancelRun` each answer with the run.
* A run that has already finished answers `CONFLICT`. That is the difference
  between "you stopped it" and "it was over before you asked".
* A run that does not take messages says that, rather than reading as missing:
  a stage can declare `accepts_messages = false`.

## Deleting records

```graphql
mutation { deleteRuns(before: 1788000000) { deleted skipped { id reason } } }
```

Takes exactly one of `ids` or `before`. Neither is a client that failed to build
its query, and both is two predicates for one act, so each is refused.

Deleting a run takes its sub-agents with it: their records only mean anything
under the run that started them. A run that is still going is skipped with its
reason rather than removed, so read `skipped` when `deleted` is shorter than you
expected. Partial success is the normal outcome, not a failure.

Deleting a record is not editing a run, so a finished run is fair game here even
though the lifecycle mutations refuse it.

## Exporting everything

Paging five thousand runs is a hundred requests. An export is one:

```graphql
mutation {
  bulkExportRuns(filter: { statusIn: [COMPLETE] }, fields: ["run_id", "status", "cost_usd"]) {
    id status downloadUrl
  }
}
```

It answers before the file exists, which is what makes it a job rather than a
very large response. The request returns in milliseconds however large the store
is. Poll it, and fetch it when it is ready:

```graphql
{ bulkExport(id: "export-1789865498-0") { status written error downloadUrl } }
```

`written` counts the runs on disk so far, so a progress bar has something to
read. `downloadUrl` is null until `status` is `complete`, because there is
nothing to fetch before then. It is then a signed link, the same kind the byte
fields mint, so a download button can use it directly.

The filter is the same `RunFilter` the `runs` connection takes. `fields` narrows
each row to the top-level run fields you name, and a name no run carries is
`BAD_USER_INPUT` rather than a column quietly missing from the file.

What comes back is JSONL: one JSON object per line, not one array. A reader can
start on it before the writer has finished, and neither side ever holds the whole
store in memory.

```
{"run_id":"run-a","status":"complete","cost_usd":0.0142}
{"run_id":"run-b","status":"complete","cost_usd":0.0071}
```

A file is kept for one hour, then removed along with its job record. Neither
outlives the other, so an expired id and one that was never started both answer
null. Ask again: an export is cheap, and the store has moved on anyway.

## Answering a prompt

`openInteractions` is the approval inbox: every open ask, each naming the run it
is parked on. The daemon holds these in memory, so it is one read rather than a
walk of the run store.

```graphql
{ openInteractions { runId request { id kind prompt options body toolCall { toolName } } } }
```

Answer with exactly one variant, and which one the request's `kind` decides.

```graphql
mutation { answerInteraction(input: { approval: {
  requestId: "coder-1788924523-abc123-approve-call_1", approved: false,
  feedback: "read the file instead" } }) { requestId accepted } }
```

| Variant | For a request of kind |
|---|---|
| `choice` | `MULTIPLE_CHOICE` |
| `text` | `FREE_TEXT` or `EDIT_TEXT` |
| `approval` | `CONFIRM` or `TOOL_APPROVAL` |

Two things worth knowing. `feedback` is what the model reads instead of the
call, so it goes with a denial and is refused beside an approval. And the first
answer wins: a second answer to the same request comes back `accepted: false`
rather than as an error, because two people clicking one prompt is ordinary.

## Blueprint writes and admin

`createBlueprint`, `updateBlueprint` and `deleteBlueprint` are ordinary
mutations. A name that is already installed is a `CONFLICT` on create:
replacing somebody's blueprint is what an edit is for. Uninstalling one leaves
every run that used it intact, because each run holds its own snapshot.

The four checks are queries, not mutations: `validateBlueprint`,
`validateScript`, `validateConfigKey` and `testYoloProfile` take text and give
a verdict, writing nothing, dialling nothing and running nothing. A form
usually calls one just before a write, which is where it sits on the screen,
not what it does. None of them is gated, and a read-only client can use them.

`validateBlueprint` reports rather than fails. A manifest that will not install
comes back `valid: false` with the reasons, because the request to check it
succeeded. It takes the text as `content`, and a `name` beside it checks the
text as that installed blueprint, so its own scripts resolve.

```graphql
{
  validateBlueprint(content: "[agent]\nname = \"coder\"\n", name: "coder") {
    valid
    errors
    warnings
  }
}
```

A second group changes the machine rather than a run, and `lev serve` opens it
only with `--allow-admin`: `addMcpServer`, `removeMcpServer`, `putMimeRow` and
`deleteMimeRow`. Adding an MCP server writes a command that Leviath then spawns,
for this run and every future one, which is why it is behind a flag rather than
behind the API token alone.

Without the flag, those fields are invisible to introspection and refused with
`FORBIDDEN` if you name one anyway. The hiding is a courtesy; the refusal is the
boundary. The published schema file documents them either way, because it
describes what the API is rather than what one server will do.

## The machine's own state, and changing it

`daemon` says who is on the other end of the control socket, `update` says what
an upgrade would do, and `updateJob(id:)` follows one that is running.

```graphql
{
  daemon { reachable version build pid restarts restartAdvised }
  update {
    version installMethod channel latest updateAvailable
    binary { __typename ... on UpgradeByCommand { shell } }
    blueprints { name version change preselected }
  }
}
```

Planning never reaches the network. The "is there anything newer" half is
whatever the last check found, and asking starts another for whoever asks next
rather than waiting on one, so a page can ask every time it opens.

`journal` says whether the daemon is still recording what its runs do:

```graphql
{
  journal {
    healthy appendsAttempted appendsFailed snapshotsFailed queueDepth
    lastError { runId path message at }
  }
}
```

Worth asking on any page that shows runs as healthy. A daemon whose journal is
refusing writes serves every other field here exactly as it did before, and a run
whose journal record cannot be written is failed rather than carried on. `healthy`
stays false for the life of the daemon once a write has been lost, because a
record that went missing does not come back. The field is null when the daemon
cannot be reached, since this reading exists nowhere else; `daemon.reachable` says
whether that is why.

Behind `--allow-admin`, the mutations that change the machine rather than a run:

| Mutation | What it changes |
|---|---|
| `updateConfig` | The config file, a field at a time |
| `putScript`, `deleteScript` | A Rhai script every run then executes |
| `putMimeRow`, `deleteMimeRow` | One row of the mime registry |
| `addMcpServer`, `removeMcpServer` | A server the daemon spawns |
| `runDoctorLive` | Nothing. It asks a provider and the daemon, which costs seconds |
| `makeDirectory` | One directory, so a picker can offer "New Folder" |
| `startUpdate` | Runs a package manager and rewrites the agents directory |
| `providerSignIn`, `providerSignOut`, `checkProvider` | A subscription's stored sign-in |
| `testMcpServer`, `loginMcpServer` | Nothing. They connect to a server and report |
| `probeModels` | Nothing. It asks an endpoint what it serves |
| `putYoloProfiles` | The profiles file, whole |

`updateConfig` is a partial edit with three states per setting, which is why the
keys take `null` rather than an empty string:

```graphql
mutation {
  updateConfig(input: { overrideModel: null, providerOrder: ["openai", "anthropic"] }) {
    defaultProvider overrideModel providerOrder
  }
}
```

A field left out leaves the setting alone. `null` clears it. A value sets it. An
empty string is refused rather than read as a clear, because a form that posts its
empty box should be told rather than obeyed. Every refusal happens before anything
is written, so a request that is going to fail leaves the file as it was.

A sign-in answers as soon as there is a URL to go to, because what happens after
that is the person's business:

```graphql
mutation { providerSignIn(provider: "anthropic") { authorizeUrl alreadyWaiting } }
```

The browser has to be on the serving host. The flow listens on a loopback port
there, so a browser anywhere else cannot finish it, and one sign-in runs at a time
because a second could not bind that port. Asking again while one is waiting
answers the same URL with `alreadyWaiting: true` rather than refusing, since that
URL is what the client needs either way. Read `providers` to see whether it landed.

`putYoloProfiles` takes the whole file rather than one profile. The file is the
unit: `--yolo=<name>` names a profile inside it, and the profiles refer to each
other, so writing one at a time would let a save leave the set inconsistent. It is
parsed before it is written, so a file that would not load is refused rather than
saved and found at the next spawn.

`startUpdate` answers before the work is done, because the work is a download and
an install. Poll `updateJob(id:)` or watch the live frames. One update runs at a
time.

## Live frames

`GET /ws/graphql` streams the same frames `/ws` carries, over
`graphql-transport-ws`. Authenticate with `?token=`, because a browser cannot
put a header on a WebSocket handshake.

```graphql
subscription Watch($run: ID!) {
  events(runId: $run, includeDescendants: true, types: [RUN_STATUS_CHANGED, LOG, INTERACTION_NEEDED]) {
    __typename
    ... on RunStatusChanged { runId status stage }
    ... on LogLine { runId line }
    ... on InteractionNeeded { runId request { id kind prompt options } }
    ... on EventsDropped { count }
  }
}
```

What this buys over `/ws`:

* `types` and the run scope are applied on the server, before a frame is
  serialized. A console watching one run of five thousand is handed one run's
  frames.
* `includeDescendants` adds the sub-agents of those runs as they spawn. Without
  it, a fan-out means re-querying the tree and re-subscribing while it grows,
  and the frames in between are lost.
* `EventsDropped` says you fell behind, and by how many frames. The broadcast is
  bounded, and a listener that cannot keep up is skipped past rather than
  allowed to hold up the daemon. Treat it as the cue to re-read whatever you
  render.

Two delivery rules match `/ws` exactly. `DaemonLinkChanged` reaches every
subscription, scoped or not, because it explains why a run's frames stopped.
The machine's other frames, such as config health and update progress, reach
unscoped subscriptions only.

Delivery is at-most-once. A dropped connection delivers nothing until you
reconnect, so re-read the state you render when you do.

## Failures

A failure inside a field is a 200 with an `errors` entry. Each entry carries the `path` in your
query that produced it, so a page of fifty runs where one record will not read still returns the
other forty-nine.

Branch on `extensions.code`, never on the message text.

| Code | Means | REST answers |
|---|---|---|
| `BAD_USER_INPUT` | The request is wrong. Retrying it unchanged fails the same way | `400` |
| `NOT_FOUND` | Nothing by that name, or nothing in the state the request needs | `404` |
| `CONFLICT` | It exists, and its state refuses the change | `409` |
| `DAEMON_UNAVAILABLE` | The daemon could not be reached. Get it back, then retry | `503` |
| `DAEMON_INCOMPATIBLE` | The daemon was updated under a running server. Restart `lev serve` | `502` |
| `INTERNAL` | Something failed that you did nothing wrong to cause | `500` |

`extensions.httpStatus` carries that same number, so a client that already knows the REST
vocabulary needs no second table.

## Limits

A query is checked before any of it runs.

| Limit | Value | Why |
|---|---|---|
| Depth | 12 | A run's children are runs, so nesting has no natural end |
| Complexity | 10000 | Depth does not bound breadth: fifty runs each asking for fifty children is shallow and large |
| `first` | 200 | The server's page-size cap, the same one `GET /api/runs` uses |

A `first` over the cap is refused rather than quietly cut down. A client builds its query in code,
and silently getting 200 of the 500 rows it asked for shows up much later as missing data.

## The schema

The schema is generated from the server's own types, so it cannot describe something the server does
not serve. It is the only one of Leviath's published schemas that is: the OpenAPI spec, the
blueprint schema and the config schema are written by hand and held to the code by tests. Read it
three ways:

* The published file, [`leviath.graphql`](https://leviath.dev/docs/stable/leviath.graphql). It is
  committed, so no command is needed to read it, and a test refuses a build whose schema has moved
  away from it. Each channel publishes its own copy.
* Introspection, which any GraphQL client tool can read live from your own server.
* `lev serve --print-graphql-schema`, which prints what your build serves and exits.

New fields and types are added; nothing is removed without being marked deprecated first. Check the
`graphql` capability in `GET /api/config` before choosing this transport, the same way you check any
other [feature](/docs/api#feature-detection).
