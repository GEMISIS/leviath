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
* `tools(agent: "coder")` scopes the inventory to one blueprint's own tools
  directory, which is what an editor offering an `available_tools` list wants.
  `skipped` names scripts that were found and could not be offered, with the
  reason, because a tool an author believes exists and silently is not there is
  the failure worth reporting.

## Moving a run

Mutations return the run as it is afterwards, so you never have to guess whether the act landed, and
you do not need a second request to find out.

```graphql
mutation { pauseAgent(runId: "coder-1788924523-abc123") { run { id status } } }
```

* `pauseAgent`, `resumeAgent` and `cancelAgent` each answer with the run.
* A run that has already finished answers `CONFLICT`. That is the difference between "you stopped
  it" and "it was over before you asked".
* A run the daemon does not know answers `NOT_FOUND`, naming both things that can mean.

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
