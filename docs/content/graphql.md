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
* `serverTime` is the daemon's clock when the page was built. Pass it back as `filter.since` to poll
  for what changed.
* `missing` lists ids from an `ids` fetch that name no run here. One dead id never costs you the
  rest of the batch.

Reading one run is the same field:

```graphql
{ runs(ids: ["coder-1788924523-abc123"]) { edges { node { id status error } } missing } }
```

### Filtering

`filter` takes the whole predicate in one place.

| Field | What it selects |
|---|---|
| `status`, `statusIn` | One status, or any of several |
| `query` | A case-insensitive substring. No regex, no operators |
| `queryIn` | Where to search. `META` and `FILES` are free; `CONTEXT`, `LOGS` and `JOURNAL` read files |
| `parent` | One run's direct children, paged |
| `topLevelOnly` | Only runs nobody started |
| `since` | Inclusive lower bound on whichever timestamp `sort` names |
| `sort`, `ascending` | `STARTED_AT`, `UPDATED_AT` or `LAST_PROGRESS_AT`, newest first by default |

`ids` names exactly which runs to return, so it cannot be combined with a filter. Asking for both is
refused rather than resolved one way and silently ignored the other.

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

## Blueprints

Two different questions, and the schema keeps them apart.

* `blueprints` lists what is installed on the machine now.
* `blueprint` on a run is the manifest that run executed, read from the run's
  own copy.

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
are still read from the installed agent directory.

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
edges out of it with their conditions and gates. So is what the agent declares
run-wide: its dependencies, the mime rows it ships, its sandbox, its summarizer,
and what it would like to run unasked.

```graphql
{
  blueprints(exact: ["coder"]) { blueprints {
    dependencies { name kind required remedy }
    stages {
      name mode
      model { models { provider model } allowUserDefault }
      transitions { target condition gate { requireRegions maxAttempts } }
      interactionPoints { name prompt style options }
      fanOut { workerStage maxWorkers onWorkerFailure }
    }
  } }
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

A reference the manifest guarantees resolves is an object, and one that may
dangle is a name. An edge's target stage exists, because a blueprint naming a
stage it does not declare is refused at load. A gate may name a region a later
edit removed, and a stage may name a tool this machine does not have, so those
stay names: resolving them would drop them, and a gate that quietly disappears
reads as a gate nobody wrote.

## Bytes

Bytes never ride a query answer. A run's parts and artifacts come back as
metadata plus a short-lived signed link, and `fileUrl` mints one for any file in
the run's working directory.

```graphql
{
  runs(ids: ["coder-1788924523-abc123"]) { edges { node {
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
  runs(ids: ["coder-1788924523-abc123"]) { edges { node {
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

`contextHistory` is how the run's context window changed over the run. Each point
carries a whole window, so it is paged harder than the run listing is, and the
region contents are their own field: asking for the shape of fifty windows does
not read fifty windows' text.

```graphql
{
  runs(ids: ["coder-1788924523-abc123"]) { edges { node {
    contextHistory(first: 20, descending: true) {
      total
      pageInfo { hasNextPage endCursor }
      edges { node { at stage window { totalTokens maxTokens } } }
    }
  } } }
}
```

## The machine itself

Three catalogues, sized by what you configured rather than by what has piled
up, so they are plain lists with no paging.

```graphql
{
  models { id provider maxContextTokens limitsSource pricing { inputPerMtok } }
  providers { id display enabled signedIn account }
  tools { tools { name source } groups { name description } skipped { path reason } }
}
```

* `models` answers from the catalogue this server keeps, so it costs no
  provider call. Pass `refresh: true` to ask the providers again and wait.
* Two providers can serve the same model id and bill to different places, so
  `provider` is part of each model rather than something you infer.
* `enabled` and `signedIn` are different questions. A provider can be turned on
  with no credential stored, and a credential can outlive the config entry that
  used it.
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
* `tools(agent: "coder")` scopes the inventory to one blueprint's own tools
  directory, which is what an editor offering an `available_tools` list wants.
  `skipped` names scripts that were found and could not be offered, with the
  reason, because a tool an author believes exists and silently is not there is
  the failure worth reporting.

## Starting and steering a run

Every mutation answers with the run as it is afterwards, so you never have to
guess whether the act landed, and you do not need a second request to find out.

```graphql
mutation {
  spawnAgent(input: { blueprint: "coder", task: "fix the parser", workdir: "/work" }) {
    run { id status task }
    warnings
  }
}
```

`warnings` names checks the blueprint declared that your own output shape
retires. Empty when there are none.

Three refusals here are the server's, not the daemon's, and each answers
`FORBIDDEN`: a workdir outside `--workdir-root`, `yolo` on a server started with
`--no-remote-yolo`, and a `callbackUrl` the outbound policy will not allow.

```graphql
mutation { sendMessage(runId: "coder-1788924523-abc123", message: "keep going") { run { status } } }
mutation { pauseAgent(runId: "coder-1788924523-abc123") { run { id status } } }
```

* `pauseAgent`, `resumeAgent` and `cancelAgent` each answer with the run.
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
{ openInteractions { runId request { id kind prompt options tool body } } }
```

Answer with exactly one variant, and which one the request's `kind` decides.

```graphql
mutation { answerInteraction(input: { approval: { requestId: "approve-call_1", approved: false,
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

`createBlueprint`, `updateBlueprint`, `deleteBlueprint` and `validateBlueprint`
are ordinary mutations. A name that is already installed is a `CONFLICT` on
create: replacing somebody's agent is what an edit is for. Uninstalling one
leaves every run that used it intact, because each run holds its own snapshot.

`validateBlueprint` reports rather than fails. A manifest that will not install
comes back `valid: false` with the reasons, because the request to check it
succeeded.

A second group changes the machine rather than a run, and `lev serve` opens it
only with `--allow-admin`: `addMcpServer`, `removeMcpServer`, `putMimeRow` and
`deleteMimeRow`. Adding an MCP server writes a command that Leviath then spawns,
for this run and every future one, which is why it is behind a flag rather than
behind the API token alone.

Without the flag, those fields are invisible to introspection and refused with
`FORBIDDEN` if you name one anyway. The hiding is a courtesy; the refusal is the
boundary. The published schema file documents them either way, because it
describes what the API is rather than what one server will do.

## Live frames

`GET /ws/graphql` streams the same frames `/ws` carries, over
`graphql-transport-ws`. Authenticate with `?token=`, because a browser cannot
put a header on a WebSocket handshake.

```graphql
subscription Watch($run: ID!) {
  events(runId: $run, includeDescendants: true, types: [AGENT_STATUS, LOG, INTERACTION_NEEDED]) {
    __typename
    ... on AgentStatusChanged { runId status stage }
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
not serve. Read it three ways:

* The published file, [`leviath.graphql`](https://leviath.dev/docs/stable/leviath.graphql).
* Introspection, which any GraphQL client tool can read live from your own server.
* `lev serve --print-graphql-schema`, which prints what your build serves.

New fields and types are added; nothing is removed without being marked deprecated first. Check the
`graphql` capability in `GET /api/config` before choosing this transport, the same way you check any
other [feature](/docs/api#feature-detection).
