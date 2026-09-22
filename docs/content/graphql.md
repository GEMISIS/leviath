---
title: GraphQL API
description: The GraphQL endpoint `lev serve` answers beside its REST routes, with its filter grammar, paging, mutations, live frames and published schema.
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
| `?cursor=` and `next_cursor` | `after:` and the connection's `cursor` |
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
  -d '{"query":"{ runs(first: 5) { results { id title status } } }"}'
```

A missing or wrong token is a plain-text `401`, before any query runs. The in-flight cap and the
request deadline apply here too, so this endpoint can answer `503` or `408` like every other one.
See [limits](/docs/api#limits).

## What a type's name tells you

Every type in this schema carries a suffix, and the suffix says what the type is for. Read it and
you know whether a name belongs in a selection set, in a filter or in a mutation argument, without
opening the schema.

| Suffix | Kind | Reach for it when |
|---|---|---|
| `XOutput` | object | You are reading. `RunOutput`, `StageOutput` |
| `XInput` | input | You are filtering. It mirrors `XOutput` field for field |
| `XListInput` | input | The field is a list. Holds `some`, `every` and `none` |
| `XFilter` | input | The field is a scalar or an enum. `StringFilter`, `RunStatusFilter` |
| `XOrder` | input | You are sorting. One key and a direction |
| `XConnection` | object | You asked for a page. `results`, `cursor`, `total` |
| `XRef` | input | You are pointing at something that already exists. `BlueprintRef` |
| `VerbNounRequest` | input | It is the single argument of the mutation `verbNoun` |
| `VerbNounResult` | object | It is what `verbNoun` answers with, carrying the entity |
| `XWrite` | input | It is a write shape nested inside a request. `McpServerWrite` |
| `XOptions` | input | It is a read-side argument that is not a filter. `RunSearchOptions` |
| `XEvent` | object | It is a subscription frame, named in the past tense |

One query uses four of them at once. `blueprints` takes a `BlueprintInput` filter and a
`BlueprintOrder` sort key, and answers with a `BlueprintConnection` of `BlueprintOutput`:

```graphql
{
  blueprints(
    filter: { regions: { some: { kind: { eq: COMPACTING } } } }
    orderBy: [{ field: NAME, direction: ASC }]
    first: 10
  ) {
    results { name version description }
    cursor
    total
  }
}
```

Enums, unions and interfaces carry no suffix. `RunStatus`, `MimeTokenRule` and `Node` are
vocabulary and abstraction rather than a place in the grammar.

## One shape for every listing

Every collection in this schema takes the same four arguments and answers with the same three
fields.

```graphql
query FirstPage {
  runs(first: 25) {
    results { id title status startedAt }
    cursor
    total
  }
}
```

`results` holds at most `first` items. `cursor` names where you got to, and it is null when that
page was the last one. Pass it back as `after` for the next page:

```graphql
query NextPage($after: Cursor) {
  runs(first: 25, after: $after) {
    results { id title status }
    cursor
  }
}
```

There are no edges, no `pageInfo` and no per-item cursor. A connection may add a field of its own
beside the three, and two do: `RunConnection.highlights` carries search snippets, and
`ToolConnection.skipped` carries the scripts a walk found and could not offer.

A cursor belongs to the filter, the sort and the search it was minted under. Send it back with a
different predicate and the server refuses it rather than resuming somewhere that means nothing.
Change the filter and start again from the first page.

`total` is resolved only when you select it. For a filter answered from memory it is free. For one
that has to open files it walks the whole store, so ask for it on the first page and carry the
number, rather than asking again on every page.

Page sizes are capped per listing, and a `first` over the cap is refused rather than quietly cut
down. A client builds its query in code, and silently getting 200 of the 500 rows it asked for
shows up much later as missing data.

| `first` cap | Listings |
|---|---|
| `500` | `models`, `providers`, `tools` |
| `200` | `runs`, `blueprints`, `scripts`, `mcpServers`, `mimeRows`, `yoloProfiles`, `updateJobs`, `openInteractions`, and every listing on a run except the two below |
| `1000` | `files` on a run, where a row is a name and a size |
| `100` | `contextHistory` on a run, where a point carries a whole context window |

Separately, `runs(filter: { id: { in: [...] } })` names at most 200 runs at once. That is not a page
size: it is how many records the filter may ask for by name, and going over it is refused.

## Filters

A filter mirrors the thing it selects. `RunInput` has a field wherever `RunOutput` has one, under
the same name, wrapped in whatever compares that field's type. Set the fields that have to hold,
and one filter object is an `and` of its own fields.

### The scalar comparators

One set, shared by every filter input in the schema.

| Filter | Operators |
|---|---|
| `IDFilter` | `eq` `ne` `in` `notIn` `isNull` |
| `StringFilter` | `eq` `ne` `in` `notIn` `contains` `startsWith` `endsWith` `isNull` |
| `IntFilter`, `BigIntFilter`, `FloatFilter`, `DecimalFilter`, `TimestampFilter` | `eq` `ne` `in` `notIn` `lt` `lte` `gt` `gte` `isNull` |
| `BooleanFilter` | `eq` `ne` `isNull` |
| `JSONFilter` | `eq` `ne` `isNull`, on the whole value |
| `StringListFilter` | `has` `hasEvery` `hasSome` `isEmpty` `isNull` |
| One per enum, such as `RunStatusFilter` | `eq` `ne` `in` `notIn` `isNull` |

```graphql
{
  runs(filter: {
    startedAt: { gte: 1758326400, lt: 1758412800 }
    cost: { costUsd: { gte: "1.00" } }
    title: { contains: "parser" }
    status: { in: [COMPLETE, COMPLETE_INTERACTIVE] }
  }) {
    results { id title status cost { costUsd costIsExact } }
    total
  }
}
```

The exact string comparisons are case-sensitive. `contains`, `startsWith` and `endsWith` ignore
ASCII case, as the run search does. A `Decimal` bound travels as a string, because a cost figure a
JSON parser re-rounds is no longer the figure you sent. A `Timestamp` is unix epoch seconds, as a
number.

### Absence, and the combinators

A field with no value satisfies no comparison. A run with no title matches no `title` filter
whichever operator you asked for, so "has no title" is its own question, and `isNull` is how you
ask it. Every filter in the schema carries it.

`and`, `or` and `not` compose filters of the same type at any depth. `and` is a list every member
of which has to match, `or` a list at least one member of which has to, and `not` one filter that
must not. An empty `or` list matches nothing, which is what an alternation with no alternatives
selects.

```graphql
{
  topLevel: runs(filter: { parentId: { isNull: true } }, first: 25) {
    results { id title }
  }
  troubled: runs(filter: {
    status: { in: [ERROR, CANCELLED] }
    or: [{ blueprintName: { startsWith: "coder" } }, { yoloProfileName: { eq: "careful" } }]
    not: { title: { contains: "scratch" } }
  }, first: 25) {
    results { id title status error }
  }
}
```

### Asking about a list

Where the thing you are filtering holds a list, the mirror is an `XListInput` with three
quantifiers. `some` keeps it when at least one member matches, `every` when they all do, and `none`
when no member does.

```graphql
{
  usedAnthropic: runs(filter: { stageModels: { some: { provider: { eq: "anthropic" } } } }) {
    results { id stageModels { provider model } }
  }
  allStagesDone: runs(filter: { stages: { every: { status: { eq: COMPLETE } } } }) {
    results { id title }
  }
  noImages: runs(filter: { blobs: { none: { mimeType: { startsWith: "image/" } } } }) {
    results { id }
  }
}
```

`every` holds over an empty list, which is what "every member matches" means when there are no
members. Reach for `none` when you mean "and there is at least nothing of this kind".

### Asking about a union

A union's mirror carries one field per variant. Setting one asks about that variant, and a value
of any other variant never matches it.

```graphql
{
  mimeRows(filter: { tokens: { perPixel: { pixelsPerToken: { gt: 700 } } } }, first: 25) {
    results {
      mimeType
      tokens {
        __typename
        ... on PerPixelOutput { pixelsPerToken max }
        ... on FixedOutput { tokens }
      }
    }
  }
}
```

### Through a relation

Where a field of the output is another object, the filter field is that object's own filter. So a
predicate follows the same path a selection set does, and you never have to flatten a question into
a column name.

```graphql
{
  byRegionKind: blueprints(filter: { regions: { some: { kind: { eq: CHECKLIST } } } }) {
    results { name version }
  }
  byStageStatus: runs(filter: { stages: { some: { status: { eq: ERROR } } } }) {
    results { id title }
  }
  byParent: runs(filter: { parent: { blueprintName: { eq: "coder" } } }) {
    results { id parentId }
  }
}
```

A whole subtree is a filter on `ancestorIds`, which every run carries root first:
`ancestorIds: { has: "<id>" }`. `children` walks one level per nesting, and this is the flat read
of every level at once.

### What a file-backed filter costs

Most of `RunInput` is answered from an index the server keeps in memory, so a predicate over
status, spend, timing and blueprint name opens nothing. Some fields live in the run's own
directory: `context`, `finalOutput`, `stages`, `blobs`, `artifacts` and the search scopes below.

```graphql
{
  runs(filter: {
    status: { eq: COMPLETE }
    context: { totalTokens: { gt: 100000 } }
    finalOutput: { content: { contains: "TODO" } }
  }, first: 25) {
    results { id title context { totalTokens maxTokens } }
    cursor
  }
}
```

The walk is lazy and it is ordered. Cheap fields are tested first and drop what they can with no
file opened. What is left is sorted, the cursor is seeked past, and only then are files read, in
order, until the page is full. There is no scan cap and no truncation flag, so a selective filter
over a large store may take a while and will answer completely.

Nothing before the cursor is read twice. Page two opens no file belonging to a run page one already
returned, which is what makes paging through a file-backed filter cost the same per page as the
first one did.

### Searching the text

`search` is free text over a run listing, and it sits beside the filter rather than inside it. A
listing has one search, and nesting one in a combinator would be asking for a second.

```graphql
{
  runs(search: { query: "connection timeout", in: [META, LOGS] }, first: 10) {
    results { id title status }
    highlights { runId field snippet stageIndex }
  }
}
```

`META` and `FILES` are answered from the index and cost nothing. `CONTEXT`, `LOGS` and `JOURNAL`
read files, so they join the lazy walk above. Matching is case-insensitive substring: no regex and
no boolean operators, because the filter's own combinators say that better.

## Ordering

`orderBy` is a list of keys, each a field and a direction, applied in the order written.

```graphql
{
  runs(
    orderBy: [{ field: LAST_PROGRESS_AT, direction: DESC }, { field: TITLE, direction: ASC }]
    first: 25
  ) {
    results { id title lastProgressAt }
    cursor
  }
}
```

Only fields that hold still are sortable, which is why `RunOrderField` names `TITLE`, `STARTED_AT`,
`UPDATED_AT` and `LAST_PROGRESS_AT` and not `ageSecs`. An age moves with the clock, and a sort key
that moves under a cursor makes a page skip or repeat.

Each listing declares its own keys, so `BlueprintOrderField` is `NAME` and `VERSION`, and a
journal-backed listing such as `executions` orders by `JOURNAL_POSITION`. Leave `orderBy` out and
each listing uses the one that suits it.

## Looking one thing up

Every `Node` type has a singular root field beside its listing, and a lookup answers null for "not
here" rather than failing.

```graphql
query Lookups($ids: [ID!]!) {
  run(id: "coder-1788924523-abc123") { id title status error }
  blueprint(name: "coder") { id name version digest source }
  mcpServer(name: "docs") { id name transport endpoint }
  script(ref: { kind: TOOL, name: "summarise", blueprintName: "coder" }) { id path compiles }
  node(id: "coder-1788924523-abc123") {
    id
    ... on RunOutput { title status ageSecs }
    ... on BlueprintOutput { name version }
    ... on UpdateJobOutput { status }
  }
  nodes(ids: $ids) {
    id
    ... on RunOutput { status updatedAt }
    ... on ScriptOutput { kind name compiles }
    ... on ModelOutput { modelId providerName }
  }
}
```

`node` is for a client holding an id and no type: a webhook payload, a cache key, a link somebody
pasted into a ticket. `nodes(ids:)` is the same thing in bulk, one answer per id, in the order you
asked, and null where an id names nothing.

Nine types implement `Node`, and an id is unique across all of them, which is what lets `node` work
the type out and what makes an id safe as a cache key.

| Type | Its id | Example |
|---|---|---|
| `RunOutput` | The run id, the same one every REST route takes | `coder-1788924523-abc123` |
| `BlueprintOutput` | `<name>@<digest prefix>`, one id per revision | `coder@3f9a1c0d8e77` |
| `ScriptOutput` | `script:<kind>:<name>`, with `@<blueprint>` on the kind where it has one | `script:tool@coder:summarise` |
| `McpServerOutput` | `mcpServer:<name>` | `mcpServer:docs` |
| `YoloProfileOutput` | `yoloProfile:<name>` | `yoloProfile:careful` |
| `ModelOutput` | `model:<provider>/<model id>` | `model:openai/gpt-5.5` |
| `ProviderOutput` | `provider:<name>` | `provider:anthropic` |
| `UpdateJobOutput` | The job id `startUpdate` handed back | `update-1788924523-1` |
| `RunExportOutput` | The job id `startRunExport` handed back | `export-1788924523-0` |

A name on its own is never an id. A server, a profile and a script are each unique only within
their own kind, so their ids carry the kind as a tag. A model's carries the provider too, because a
model id is the provider's own and two providers can both serve `gpt-5.5` while billing to
different places.

An id that names nothing answers null rather than an error. A deleted run, an export past its hour,
a blueprint revision this machine no longer has and a typo are the same answer, and all four mean
the same thing. A tagged id whose tag this server does not know answers null too, so a client
written against a newer build degrades rather than breaking. What does fail is a read that could
not answer at all, such as a config file that will not parse.

## Reading a run

The summary fields come from one stat-cached read, so a listing of fifty costs fifty stats.
Everything else reads a file in the run's directory, and only when you ask for it.

```graphql
{
  run(id: "coder-1788924523-abc123") {
    id title status task ageSecs workingSecs iteration toolCallCount
    usage { promptTokens completionTokens cachedTokens }
    cost { costUsd costPricedUsd costIsExact unpricedCalls }
    currentStage { name index of }
    waitReason { reason needsAPerson blocker remedy outstanding }
    flags { emptyOutput producedOutput maxIterationsHit modifiedFileCount }
    metadata { key value }
  }
}
```

`waitReason.needsAPerson` is the field a fleet view wants. A run waiting on its own workers is
healthy and resolves on its own. A run waiting on an answer is a row somebody has to act on.
`NEEDS_SETUP` also carries a `blocker` and a `remedy`, so a client can offer the right fix rather
than parse a sentence.

### What each stage ran on

A blueprint picks a model per stage and names a list to fall back through, so one run can honestly
have used three providers. The stage ledger is where that is answered, stage by stage.

```graphql
{
  run(id: "coder-1788924523-abc123") {
    stageModels { provider model }
    stages(first: 20, orderBy: [{ field: INDEX, direction: ASC }]) {
      total
      results {
        name index status entered visitCount
        usage { promptTokens completionTokens }
        cost { costUsd }
        models { provider model }
        regionPeaks { region tokens }
      }
    }
  }
}
```

`models` on a stage is null, never an empty list, where the stage has run no inference. A stage the
run never entered, one whose first call is still in flight, and one whose only provider could not be
reached all answer null.

`stageModels` on the run is the flat roll-up of the same thing, and it is what the filter reads:
`stageModels: { some: { model: { contains: "opus" } } }` keeps a run where any stage ran on one.
That is not the same question as "no stage ran on anything else", which is `none` or a `not`.

### The sub-agent tree

```graphql
{
  run(id: "coder-1788924523-abc123") {
    treeStatus { depth descendantCount rollup { promptTokens completionTokens } }
    children(first: 25, orderBy: [{ field: STARTED_AT, direction: ASC }]) {
      results { id title status cost { costUsd } }
      cursor
      total
    }
  }
  subtree: runs(filter: { ancestorIds: { has: "coder-1788924523-abc123" } }, first: 50) {
    results { id ancestorIds status }
    total
  }
}
```

`children` is the run listing with this run preset as the parent, so it takes the same filter, the
same sort keys and the same cursors. `treeStatus` answers "what did this fan-out cost" without
walking it: the roll-up covers every run below, at any depth. A parent that spent little and whose
fifty workers spent a great deal is not a cheap run.

### What a run did

A context window says what a model is looking at now. It does not say what the run tried.
`executions` reads the run's journal instead, so it holds the attempts the window no longer shows:
a call a gate refused, one that failed and was reissued, one a restart cut off.

```graphql
{
  run(id: "coder-1788924523-abc123") {
    executions(first: 50) {
      total
      cursor
      results {
        id callId outcome stageIndex iteration dispatchedAt endedAt journalPosition
        call {
          __typename
          toolName
          rawArguments
          ... on ShellCallOutput { args { command } }
          ... on WriteFileCallOutput { args { path append } }
          ... on UntypedToolCallOutput { reason }
        }
      }
    }
  }
}
```

One attempt is one execution. A call the model reissued after a failure is a second execution with
its own `id`, which is why these have ids of their own. A provider is free to reuse its `callId`
across a retry, so that field is correlation rather than identity.

`outcome` is null for three different reasons, and a client must not flatten them. The attempt may
still be running, it may have ended before this build recorded outcomes, or it may have ended in a
way only the result text describes. `endedAt` tells the first apart from the other two.
`INDETERMINATE` is its own answer: a daemon that died between dispatch and completion left a call
nobody saw the end of, and the resume that carried the run on records that.

`journalPosition` is where the record that dispatched the attempt sits in the journal, as a byte
offset. It only climbs within a run and it never changes, so it orders executions and names one for
as long as the run exists.

Every tool takes exactly one argument shape, so each tool has its own type and a mismatched pair
cannot be built. `rawArguments` is on every call, typed or not. The typed view is a convenience
over it and never a replacement, because a debugger that could only show the tidied version hides
the malformed call that caused the bug. An alias is typed as the tool it means, so `bash` comes
back as a `ShellCallOutput` whose `toolName` is still `bash`.

A call is an `UntypedToolCallOutput` for two reasons, and `reason` says which.
`NO_TYPE_FOR_THIS_TOOL` is an MCP or script tool, which is ordinary.
`ARGUMENTS_DID_NOT_MATCH` is a built-in whose recorded arguments did not fit its own schema, which
is worth looking at.

### What an execution is connected to

An execution sits inside a stay in a stage and follows from one trip to a provider. Both are things
you can ask for rather than pair up yourself.

```graphql
{
  run(id: "coder-1788924523-abc123") {
    executions(first: 20, filter: { outcome: { eq: FAILED } }) {
      results {
        id
        visit { id ordinal enteredAt leftAt inProgress }
        requestedBy { attempt provider model outcome { kind } }
        contextChanges { cause revisionAfter regions { region tokenDelta } }
        producedArtifacts { name mimeType size url }
        result { text bytes truncated parts }
      }
    }
  }
}
```

`visit` is the stay, and it is the key to correlate on. `stageIndex` says where an execution sat,
not which stay it belonged to: a stage entered three times has one index and three visits, and
`iteration` restarts on every entry.

`requestedBy` is the trip to the provider whose answer asked for the call. You cannot work it out
from the timeline, because a failover means the answer came from a different provider than the
attempt before it went to.

`contextChanges` is what this execution committed to the window, and it is independent of
`outcome`. A call that succeeded may have committed nothing, and a call that failed may have
committed something before it failed. Most executions commit nothing: the `context_*` and `todo_*`
tools are the ones that show up, along with anything that wrote a part into a region of its own.

`result` is its own field because one result can be a whole file, so a page of executions carries
none of them. `bytes` is the whole result's size and `truncated` says whether `text` is only its
head. `parts` names the stored parts the result carried.

Each of these is null or empty where the journal did not record the connection, and never a guess.
`visit` is also null past the ledger's per-stage cap of the earliest 128 stays, where the stay is
real and its detail is not kept.

### What a run asked

`executions` says what a run tried. It says nothing about the calls a person had to approve, or the
free-form questions a stage asked along the way. Once a tool has read the answer, a granted call
looks exactly like one no policy ever stopped, so `interactions` is the only record that this run
stopped and asked somebody at all.

```graphql
{
  run(id: "coder-1788924523-abc123") {
    openInteraction { id kind prompt options }
    interactions(first: 50) {
      total
      cursor
      results {
        id kind prompt body stageName isRequired askedAt settledAt
        toolName
        settlement { outcome approved scope choice text feedback }
      }
    }
  }
}
```

A question is written down when it settles, so the one a run is parked on right now is not on this
list yet. `openInteraction` carries that one while it is open, and the approval inbox across every
run is `openInteractions`.

Every field on `settlement` but `outcome` is null unless `outcome` is `ANSWERED`. Nobody answered a
`TIMED_OUT` ask, a `CANCELLED` one or a `REFUSED` one, so there is nothing for the rest to carry.
`REFUSED` should never appear: it means a request never opened because another was already open
under the same id, which is a fault in the server rather than anything about the call.

`scope` is the one field worth a note against REST. This spells the widest grant `RUN`, where the
REST journal and answer routes write `session`. `ONCE` and `STAGE` spell the same on both sides.
A grant is keyed on what was approved, not on the tool alone: for a shell call that is the program
and its first literal argument, so `RUN` on `touch a.txt` does not cover `touch b.txt`, and the
second one asks again.

An unattended run asks nobody, so it has no interactions to list. An empty list on a run that
plainly did something dangerous means exactly that.

### What a run's provider calls took

`usage` and `cost` are per call that worked. A call refused three times and answered on the fourth
is billed once, so the time the run spent being refused is in neither of them. `inferences` is that
half, read from the same journal: one entry per trip to a provider, in the order the run made them.

```graphql
{
  run(id: "coder-1788924523-abc123") {
    inferences(first: 50) {
      total
      cursor
      results {
        stage attempt provider model durationMs backoffMs at
        outcome { kind failureKind transient capacity retry }
        digest { systemHash messages tools maxTokens temperature }
        failover { fromProvider fromModel toProvider toModel reason }
      }
    }
  }
}
```

`outcome.kind` is `SUCCEEDED` or `FAILED`, and the four fields beside it are null unless it failed.
`transient` and `capacity` are how the failure was judged at the time rather than now, because what
counts as transient is a policy that moves between releases. `retry` says what the loop did next.
`SAME_MODEL` is the same provider again after a wait, and the next entry's `backoffMs` says how
long that wait really was. `RENEWED_FILES` is an immediate retry that uploaded the request's files
afresh, so it spends no wait at all.

`digest` identifies a request without carrying it. Two attempts with the same digest sent the same
thing, which is the question a retry raises: a provider that kept refusing reads differently from a
request that kept changing underneath the run. `systemHash` is opaque, so compare it and read
nothing into the value.

`failover` is the move to a different provider, and it is null on almost every attempt. A retry
against the same provider is the next entry, not a move. Where it is set, the attempt after it went
to `toProvider` and `toModel`, and `reason` says why the first provider was judged unusable.

### What one call sent

`modelInput` is the request itself, per attempt. It is off by default, and `captureStatus` says
which of the four states a record is in before you read anything else from it.

```graphql
{
  run(id: "coder-1788924523-abc123") {
    inferences(first: 50) {
      results {
        attempt
        modelInput {
          captureStatus bytes sourceContextDigest
          toolCatalogVersion assemblyVersion
          request
          parameters {
            temperature
            maxOutputTokens { ... on MaxTokensCountOutput { tokens } }
            providerParams
          }
        }
      }
    }
  }
}
```

`RETAINED` means `request` is there. `NOT_CAPTURED` means no body was ever taken, which is every
run nobody asked to capture. `REDACTED` means a body was taken and then scrubbed, and `EXPIRED`
means it was taken and then aged out. The last two are a different fact from `NOT_CAPTURED`:
something existed and is gone. `modelInput` itself is null for an attempt whose journal recorded
none.

Everything beside `request` is recorded whether capture is on or off, because it costs nothing and
answers what `digest` cannot. `parameters` is what the request really carried after every override
and clamp, so `maxOutputTokens` is an absolute count rather than the percentage a blueprint may
have declared. `toolCatalogVersion` distinguishes two attempts that offered different tools.
`assemblyVersion` moves when the meaning of an assembled request changes, so an old captured body
stays interpretable.

`sourceContextDigest` names the window the request was assembled from, which is how a captured
request joins to `contextHistory`. It is empty when no body was taken, because folding it walks the
whole window. `bytes` is the size of the captured body, so a client can show what capture cost even
once the body is gone.

`request` is Leviath's own request shape, not one vendor's wire body. The adapter turns it into the
vendor's JSON and never hands that back, so serving the vendor shape would mean rebuilding it, and
a rebuilt prompt is not the request that was sent.

There is no mapping from context regions to places in the request. Assembly does not keep one:
conversation messages carry no region, one region can become several system blocks, and the blocks
are then reordered by cache tier. A mapping would have to be inferred after the fact, and an
inferred one is not evidence.

Turn capture on for a machine with `[observability] capture_model_input`, or for one run with
`captureModelInput` on `spawnRun`. Read the warning in
[Observability](/docs/observability#capturing-what-went-to-the-model) first. A captured request
holds whatever the run's context held, including file contents a tool read and anything somebody
pasted, and there is no size cap.

### Why a region changed

`contextHistory` serves snapshots of the window. `contextChanges` serves the changes that moved it.
Both read the same journal, and neither answers for the other. A region that lost its plan looks
identical in a snapshot, whether a compaction took it, a transform cleared it, or the model deleted
it.

```graphql
{
  run(id: "coder-1788924523-abc123") {
    contextChanges(first: 50) {
      total
      cursor
      results {
        cause at journalPosition
        revisionBefore revisionAfter executionId
        regions {
          region digestBefore digestAfter tokensBefore tokensAfter
          tokenDelta entriesAdded entriesRemoved
        }
      }
    }
  }
}
```

Each entry is one committed transaction, which may touch several regions. A compaction summarises
one region and empties another, a stage edge clears four, a resume rebuilds every region there is.
All of those are one change.

`cause` names a path through the runtime rather than a shape of edit. `SEED`, `MESSAGE`,
`MODEL_REPLY`, `TOOL_RESULT`, `PRODUCED_PART`, `COMPACTION`, `TRANSFORM`, `CONTEXT_TOOL`, `HOOK`,
`FAN_OUT`, `INTERACTION`, `RESUME` and `FRAMEWORK` are the whole vocabulary. Two paths that both
append to the conversation stay two causes, because which of them ran is the question being asked.

A change carries no content, because the snapshot recorded on the same tick already holds the text.
The per-region digests are what tell you whether to go and read it: `digestBefore` equal to
`digestAfter` means that region ended the transaction holding what it started with. `tokenDelta` is
negative where a region shrank, and `entriesRemoved` counts any eviction the change triggered.

An empty list means the journal holds no change records. A write whose path cannot name its cause
records nothing rather than borrowing the nearest neighbour, so a gap here reads as a gap.

### A window, now or by name

```graphql
{
  run(id: "coder-1788924523-abc123") {
    context { revision totalTokens maxTokens stageName regions { name kind tokens } }
    contextHistory(first: 20, orderBy: [{ field: SEQUENCE, direction: DESC }]) {
      total
      cursor
      results { at stage window { revision totalTokens maxTokens } }
    }
    contextSnapshot(revision: "cw1-4f2a9c8e5b1d7063a4e2f8c19d0b6537") {
      at stage
      window { revision totalTokens regions { name tokens } }
    }
  }
}
```

Each point of `contextHistory` carries a whole window, so it is paged harder than the run listing
is. Ask for the regions you draw rather than every point's every region, because
`regions { content }` is the text itself.

A revision is a content address, derived from what the window holds, so it names that content for
ever. `contextSnapshot` resolves one to exactly the content it was minted from, never to whatever
the run holds now. Null means this run never held that window, which is also what a revision from
another run looks like. Two points holding identical contents share a revision, and the read
answers with the first time the run held it.

### Files, logs and bytes

```graphql
{
  run(id: "coder-1788924523-abc123") {
    recorded: files(first: 50) { results { name exists } isModifiedFilesTruncated }
    onDisk: files(source: WORKDIR, path: "src", first: 50) {
      path parent workdir isTruncated
      results { name isDir size mimeType }
    }
    fileContent(path: "out/report.md") { content offset nextOffset truncated }
    current: logs(tailBytes: 4096)
    everyStage: logs(stage: { all: true }, stream: OPERATIONAL, tailBytes: 4096)
    blobs(first: 20, filter: { mimeType: { startsWith: "image/" } }) {
      results { sha256 mimeType name size width height tokens regions stored url }
    }
    artifacts(first: 20) { results { name mimeType size sha256 url } }
    fileUrl(path: "out/report.pdf", download: true)
    blobUrl(sha256: "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")
  }
}
```

`MODIFIED`, the default, is the run's own record of what it changed. It is free, because it is
already in the run's record, and it is a claim about the run rather than about the disk.
`isModifiedFilesTruncated` says when the run hit its tracked file cap, and a path since deleted is
listed with `exists: false` rather than dropped.

`WORKDIR` is what is there now, one directory level per request. Pass an entry's own path back to
go a level down. That bound is the answer to a repository with a `node_modules` in it, where one
request trying to enumerate everything is no answer at all.

`fileContent` reads text, at most a megabyte at a time, because the answer travels inside this one.
Pass `nextOffset` back as `offset` for the next window, and the windows concatenate into the file.

`logs` reads from the end of a stream. `stage` takes exactly one of `index` and `all`, and leaving
it out means the stage the run is on now. `stream` picks `OUTPUT` or `OPERATIONAL`, and `tailBytes`
bounds the read, under the server's own cap because naming every stage multiplies it by the stage
count.

Bytes never ride a query answer, which is why `blobs` and `artifacts` above carry metadata and a
`url` rather than the bytes. `fileUrl` mints the same kind of link for any file in the working
directory, and `blobUrl` for a hash a client already holds.

A signed link carries its own permission, so it works in an `<img src>` or a download link, where a
header cannot be set. What it is not is your API token in a URL. It opens one path, it lasts five
minutes, and it opens byte routes only. The signing key is random per server process and never
written down, so a restart invalidates every link it handed out. Links are relative, so they keep
whatever host, scheme and port you reached the server on.

## Blueprints and the manifest

Two different questions, and the schema keeps them apart. `blueprints` lists what is installed on
the machine now. `blueprint` on a run is the manifest that run executed, read from the run's own
copy.

The whole manifest is readable, one field at a time. That covers a stage's model block, its tool
routing, its checkpoints, its output shape, its hooks and its fan-out. It covers the edges out of a
stage too, with their conditions and gates.

```graphql
{
  blueprint(name: "coder") {
    id name version description digest source maxChildDepth toolRescan
    dependencies { name kind required remedy }
    regions { name kind maxTokens budgetPercent volatility admission }
    stages {
      name mode maxIterations availableTools
      model { allowUserDefault requestTimeoutSecs models { provider model } }
      transitions {
        targetName condition
        target { name }
        gate { requireRegionNames maxAttempts requireRegions { name } }
        stuck { afterIterations afterMinutes afterToolCalls }
      }
      interactionPoints { name prompt style options required }
      fanOut { maxWorkers maxItems onWorkerFailure workerStageName workerStage { name } }
      effective { includesBatchHint shellHintEligible tracksTaint }
    }
  }
  run(id: "coder-1788924523-abc123") {
    blueprintDigest
    blueprint { id name version source digest }
  }
}
```

A run copies its manifest into its own directory at spawn and records that copy's digest. So
editing or deleting the installed blueprint never changes what a finished run says it ran, and a
daemon restart resumes a run on the manifest it started with. Only the manifest is frozen. Scripts
it names, such as hooks and validators, are still read from the installed blueprint's directory.

`source` says which file a blueprint came from, `SNAPSHOT` or `INSTALLED`. A run recorded before
snapshots existed has no copy, so it reads `INSTALLED` and its `blueprintDigest` is null. What it
executed is unknown, which is not the same as "unchanged".

The id is `<name>@<digest prefix>`, not the bare name. Two revisions of one name are two different
objects, so a client that caches by type and id cannot merge a run's frozen copy with whatever is
installed now.

Two rules run through the manifest, because it is a document rather than a database.

A setting the author left out is null, even where the daemon has a default for it. What was written
and what the daemon resolves are different questions. `StageOutput.effective` answers the second:
the batch hint, the shell hint, the nudge, the sandbox and taint tracking, each resolved stage over
blueprint over this machine's config.

A region or a stage the manifest names is served as the object it names, with the written name kept
beside it. `targetName`, `requireRegionNames` and `defaultRegionName` carry what the author wrote,
whether or not a declaration was found. Two things put a name there with nothing to resolve. A
later edit can remove the region, which `lev validate` refuses and the daemon will not spawn. And
the runtime carries `conversation`, `tool_results`, `final_output` and `stage_instructions` whether
a manifest declares them or not.

A blueprint's own tools and scripts are fields on the blueprint, because the scope changes which
directory is walked.

```graphql
{
  blueprint(name: "coder") {
    tools(first: 50) {
      results {
        name origin description
        ... on ScriptToolOutput { path blueprint requires }
      }
      skipped { path reason }
      total
    }
    scripts(first: 50) {
      results { id kind name scope path relativePath isDeclared compiles compileError }
    }
  }
}
```

`Tool` is an interface with two members, `BuiltinToolOutput` and `ScriptToolOutput`. A script
always has a file and a built-in never does, so the file is a field on the one that has it rather
than a null on both. Sub-agent tools are built-ins with `origin: SUBAGENT`, because they carry the
same four fields and a type of their own would say nothing a client could act on.

`skipped` names scripts that were found and could not be offered, with the reason, because a tool
an author believes exists and silently is not there is the failure worth reporting.

A tool stays a name inside a manifest. `available_tools` may name an MCP server's tool, a group
token such as `@builtin`, and any tool this machine does not have, so the inventory describes what
is here rather than what was written.

## Mutations

Every mutation has the same shape. One argument named `request`, and a result that carries what
changed.

```graphql
mutation {
  pauseRun(request: { id: "coder-1788924523-abc123" }) {
    run { id status }
    warnings
  }
}
```

| Rule | What it means |
|---|---|
| Verbs | `create`, `update`, `delete`, `upsert`, `start`, `check`, `signIn`, and plain imperatives for run actions |
| Result | It carries the entity under its own name. A delete carries `deletedId` or `deletedIds` |
| Booleans | No mutation answers a bare `Boolean`. A result carries the thing instead |
| Failure | A refusal is a GraphQL error with `extensions.code`, never a field inside a result |
| Exclusive input | Spelled with `@oneOf`, so the schema refuses a second variant rather than the server |
| Sweeps | A destructive bulk mutation takes `filter` and refuses an empty one |

`checkMachine` is the one field with no `request`, because it has nothing to say: the checks are
the checks.

`spawnRun` is the widest request, and every part of it is optional but the blueprint and the task.

```graphql
mutation Spawn($task: String!) {
  spawnRun(request: {
    blueprint: { name: "coder", digest: "3f9a1c0d8e77" }
    task: $task
    workdir: "/work"
    model: "claude-sonnet-5"
    maxDepth: 3
    yolo: { profileName: "careful" }
    captureModelInput: true
    regions: [{ region: { name: "brief" }, text: "ship the parser fix" }]
    metadata: [{ key: "ticket", value: "LEV-412" }]
    attachments: [{ path: "spec.pdf", deliver: NATIVE, caption: "the spec" }]
    callback: { url: "https://example.invalid/hooks/leviath", secret: "shared-secret" }
    output: { format: "markdown", instructions: "one page, no preamble" }
  }) {
    run { id status task }
    warnings
  }
}
```

The blueprint argument is a `BlueprintRef` rather than a name, so it can carry the revision you
mean. Add the `digest` you read off `BlueprintOutput.digest` and the spawn is refused with
`CONFLICT` if something else is installed under that name. Send the name alone and the spawn takes
whatever is there.

`yolo` is `@oneOf`: exactly one of `everything` and `profileName`. `callback` puts the secret
inside the object that carries the URL, so a secret with nowhere to go cannot be written.
`attachments` name files inside the working directory, and `deliver` says whether each goes to the
model natively, as text, or as a stand-in.

`warnings` names checks the blueprint declared that this request's own output shape retires. Three
refusals here are the server's rather than the daemon's, and each answers `FORBIDDEN`: a workdir
outside `--workdir-root`, an unattended run on a `--no-remote-yolo` server, and a callback URL the
outbound policy will not allow.

### Acting on many runs at once

`pauseRuns`, `resumeRuns`, `cancelRuns` and `deleteRuns` take a filter, the same `RunInput` the
listing takes. A run the act does not apply to is reported under `skipped` rather than failing the
sweep.

```graphql
mutation {
  cancelRuns(request: {
    filter: { status: { eq: WAITING_INPUT }, startedAt: { lt: 1758326400 } }
  }) {
    runs { id status }
    skipped { id reason message }
  }
  deleteRuns(request: { filter: { updatedAt: { lt: 1758326400 } }, force: false }) {
    deletedIds
    skipped { id reason message }
  }
}
```

`SkipReason` is `STILL_RUNNING`, `RECORD_UNREADABLE`, `ALREADY_FINISHED` or `OTHER`, with a
`message` beside it. Partial success is the normal outcome here, not a failure.

`runs` carries each run as the act left it, waited for the same way the single-run acts wait. The
daemon moves a run in its world and the record is written a moment later, so the sweep reads the
records back once every act has gone out. A run that ends by itself while the sweep is reaching it
is `ALREADY_FINISHED` under `skipped` and never under `runs`, so nothing this sweep did overwrites
what the run says about itself.

An empty filter names every run on this machine, and every one of these four refuses it with
`BAD_USER_INPUT`. Deleting everything is a thing to ask for outright, not something a client falls
into by sending a filter it forgot to fill in.

Deleting a run takes its sub-agents with it, because their records only mean anything under the run
that started them. Deleting a record is not editing a run, so a finished run is fair game here even
though the lifecycle mutations refuse it.

### Answering a prompt

`openInteractions` is the approval inbox: every open ask, each carrying the run it is parked on.
The daemon holds these in memory, so it is one read rather than a walk of the run store.

```graphql
{
  openInteractions(first: 50, orderBy: [{ field: SEQUENCE, direction: ASC }]) {
    results {
      id kind prompt body options isRequired
      run { id title status }
      toolCall {
        __typename
        toolName
        ... on ShellCallOutput { args { command } }
      }
    }
    total
  }
}
```

The answer is `@oneOf`, so exactly one variant goes in, and which one the request's `kind` decides.

```graphql
mutation {
  answerInteraction(request: {
    interactionId: "coder-1788924523-abc123-approve-1"
    answer: { deny: { feedback: "read the file instead" } }
  }) {
    interactionId
    outcome
  }
}
```

| Variant | For a request of kind |
|---|---|
| `choice` | `MULTIPLE_CHOICE`, zero-based |
| `text` | `FREE_TEXT` or `EDIT_TEXT` |
| `approve` | `CONFIRM` or `TOOL_APPROVAL`, with a `scope` |
| `deny` | `CONFIRM` or `TOOL_APPROVAL`, with optional `feedback` |

`feedback` is what the model reads instead of the call, which is why it sits on the denial rather
than beside an approval. The first answer wins, and a second one comes back `ALREADY_SETTLED`
rather than as an error, because two people clicking one prompt is ordinary.

### Blueprint writes

```graphql
mutation {
  updateBlueprint(request: {
    blueprint: { name: "coder", digest: "3f9a1c0d8e77" }
    manifest: "[agent]\nname = \"coder\"\nentry_stage = \"analyze\"\n"
  }) {
    blueprint { id name version digest }
  }
}
```

A name that is already installed is a `CONFLICT` on `createBlueprint`, because replacing somebody's
blueprint is what an edit is for. The `digest` on the reference pins the revision being replaced,
so two clients editing one blueprint cannot silently overwrite each other. `deleteBlueprint`
answers with `deletedId` and leaves every run that used it intact, because each run holds its own
snapshot.

The checks are queries rather than mutations. `validateBlueprint`, `validateScript` and
`validateProviderKey` take text and give a verdict, writing nothing and dialling nothing. A form
usually calls one just before a write, which is where it sits on the screen, not what it does.

```graphql
{
  validateBlueprint(manifest: "[agent]\nname = \"coder\"\n", as: { name: "coder" }) {
    valid errors warnings
  }
  validateScript(kind: TOOL, content: "fn run(args) { args.text }") { valid error }
  validateProviderKey(provider: "openai", key: "sk-not-a-real-key") { valid message }
}
```

A manifest that will not install comes back `valid: false` with the reasons, because the request to
check it succeeded. Passing `as` checks the text as that installed blueprint, so its own scripts
resolve.

### Exports

An export is started here and fetched over REST, because the filter already lives here. See
[exporting the whole store](/docs/api#exporting-the-whole-store) for `startRunExport`,
`runExport(id:)` and the JSONL the file holds.

## Config and admin

Everything `updateConfig` writes reads back under the same name, so a settings screen renders what
it saves. A key is the one exception, and it reads back as `hasKey` on its provider.

```graphql
{
  config {
    routing { defaultProvider providerOrder overrideModel fallbackModel }
    providers { id name auth isEnabled hasKey baseUrl region }
    gateways { name kind baseUrl hasApiKey headerNames models unknownKeys }
    allowsFileUploads blueprintPaths mcpServerCount
    server {
      apiVersion capabilities isAdminEnabled
      limits { maxPageSize maxUploadBytes requestTimeoutSecs maxConcurrentRequests }
    }
    health { savedAt error { kind path message line column } }
    yoloFile { path exists error }
  }
  yoloProfiles(first: 20) {
    results {
      id name default questions checkpoints gate
      toolRules { allow ask deny }
      shellRules { allow { command args } ask { command } deny { command } }
    }
    total
  }
}
```

Read `config.server.capabilities` before choosing a code path. A 404 also means "no such run", so
discovering a feature by being refused costs a round trip and tells you less.

A write is a partial edit in five parts, and what none of them mentions is left alone.

```graphql
mutation {
  updateConfig(request: {
    set: { providerOrder: ["openai", "anthropic"], allowsFileUploads: true }
    clear: [OVERRIDE_MODEL]
    providers: [{ provider: "openai", key: "sk-not-a-real-key", isEnabled: true }]
    upsertGateways: [{
      name: "local"
      kind: OPENAI_COMPATIBLE
      baseUrl: "http://127.0.0.1:1234/v1"
      models: ["qwen3-coder"]
    }]
    deleteGateways: ["old-proxy"]
  }) {
    config {
      routing { providerOrder overrideModel fallbackModel }
      gateways { name kind models }
      providers { id hasKey isEnabled }
    }
  }
}
```

`set` names the settings to change, and a field left out of it is untouched. `clear` is the third
state, spelled in the schema as `ConfigClearable` rather than hidden in a null. Taking
`overrideModel` back to nothing is a value you send, not an absence the server has to guess at.
Every refusal happens before anything is written, so a request that is going to fail leaves the
file as it was.

The admin group is mounted only with `--allow-admin`. Without it those fields are invisible to
introspection and refused with `FORBIDDEN` if you name one anyway. The hiding is a courtesy and the
refusal is the boundary. The published schema documents them either way, because it describes what
the API is rather than what one server will do.

```graphql
mutation {
  createMcpServer(request: {
    server: {
      name: "docs"
      transport: { stdio: { command: "npx", args: ["-y", "@acme/docs-mcp"] } }
      env: [{ key: "DOCS_TOKEN", value: "t0ken" }]
    }
  }) {
    mcpServer { id name transport endpoint command args envNames auth configError }
  }
  upsertMimeRow(request: {
    row: {
      mimeType: "application/x-acme-scene"
      family: "model"
      extensions: ["scene"]
      magic: "41434D45"
      tokens: { fixed: 2000 }
    }
  }) {
    isNew
    mimeRow {
      mimeType origin blueprintName family extensions magic check
      tokens { __typename ... on FixedOutput { tokens } }
    }
  }
  upsertScript(request: {
    script: { kind: TOOL, name: "summarise", blueprintName: "coder" }
    content: "fn run(args) { args.text }"
  }) {
    script { id kind name scope path relativePath compiles compileError }
  }
}
```

Each of those answers with the row as the server now holds it, so a client never has to guess what
its write became. Secrets are the exception and they read back as names: `envNames` and
`headerNames` on a server, `hasApiKey` on a gateway, `hasKey` on a provider.

`McpTransportWrite` is `@oneOf`, so a server is stdio or HTTP and cannot be half of each. A server
whose config will not parse reads back with `transport: null` and a `configError`, which is a
different answer from "no such server".

Adding an MCP server writes a command Leviath then spawns, for this run and every future one.
Writing a script writes code a run then executes. That is why the group is behind a flag rather
than behind the API token alone.

Anything that dials out is a mutation, so queries stay side-effect free. `refreshModels`,
`checkMachine`, `checkProvider`, `checkMcpServer`, `checkEndpoint`, `signInProvider` and
`signInMcpServer` each cost seconds and a request to somebody else.

```graphql
mutation {
  signInProvider(request: { provider: "anthropic" }) {
    authorizeUrl isAlreadyWaiting
    provider { name signedIn account plan expiresAt }
  }
  checkEndpoint(request: { baseUrl: "http://127.0.0.1:1234/v1" }) { modelIds }
  refreshModels(request: { provider: "openai" }) { models { id modelId maxContextTokens } }
}
```

A sign-in answers as soon as there is a URL to go to, because what happens after that is the
person's business. The browser has to be on the serving host, since the flow listens on a loopback
port there, and one sign-in runs at a time because a second could not bind that port. Asking again
while one is waiting answers the same URL with `isAlreadyWaiting: true`.

`yoloProfiles` is a listing like any other, and `yoloProfile(name:)` reads one. A profile carries
its rules rather than a count of them: `toolRules` and `shellRules` each hold the `allow`, `ask`
and `deny` lists as written. Where the file is and whether it loads lives on
`config.yoloFile`, so "no profiles yet" reads differently from "the file is broken" without that
status being repeated on the listing.

`decide` on a profile answers what it would do with one call, running nothing. It is the same code
path `lev yolo test` takes, so the command and the API cannot disagree about a call.

```graphql
{
  yoloProfile(name: "careful") {
    decide(tool: "shell", kind: BUILTIN, args: { command: "curl https://example.com" }) {
      tool configured policy reason
    }
  }
}
```

`upsertYoloProfile` writes one profile, whole. A profile is a grant of permissions, so an edit
that left half of a previous list behind would describe rules nobody wrote. Only that one table of
`yolo.toml` is touched, so comments and formatting around it survive; the comments inside the
table being written do not. The whole document is checked before anything reaches the disk.

```graphql
mutation {
  upsertYoloProfile(request: { profile: {
    name: "builder"
    default: ASK
    questions: ASK
    toolRules: { allow: ["@builtin"], ask: ["web_fetch"] }
    shellRules: { allow: [{ command: "cargo *" }], deny: [{ command: "curl" }] }
  } }) {
    isNew
    yoloProfile { id name toolRules { allow ask } shellRules { deny { command } } }
  }
}
```

`deleteYoloProfile(request: { name })` takes one out and answers with the id it had. A name the
file has no table for is a miss rather than a silent success. Whatever sat above the table it
removed stays in the file, so deleting the first profile does not take the file's own header.

Both of these read the file as it stands before they touch it. A `yolo.toml` that will not parse,
or that another profile has made unloadable, is `UNPROCESSABLE`: your request is fine and the file
cannot answer. `BAD_USER_INPUT` is what the profile you sent earns, such as a reserved name or a
shell rule that will not compile.

## The machine itself

```graphql
{
  models(filter: { providerName: { eq: "anthropic" } }, first: 50) {
    results { id modelId providerName maxContextTokens limitsSource pricing { inputPerMtok } }
    total
  }
  providers(first: 50) { results { id name display enabled signedIn account } }
  toolGroups { name description }
  doctor { ok isLive checks { name ok detail } }
  directory(path: "/work", includeHidden: false) { path parent home cwd entries }
  daemon {
    reachable version build pid restarts restartAdvised
    journal { healthy appendsAttempted appendsFailed queueDepth lastError { runId message at } }
  }
  updatePlan {
    version installMethod channel latest updateAvailable checkedAt
    binary { __typename ... on UpgradeByCommandOutput { shell commands } }
    blueprints { name version change preselected }
  }
  serverTime
}
```

`models` answers from the catalogue this server keeps, so it costs no provider call.
`refreshModels` is the mutation that goes and asks. Two providers can serve the same model id and
bill to different places, so the provider is part of each model rather than something you infer.

`providers` lists the ones a person signs in to through a browser, which is what `signInProvider`
and `signOutProvider` act on. A provider that takes an API key is never signed in to, so it is not
here: `config { providers { ... } }` is where every provider this build knows is listed.

`enabled` and `signedIn` are different questions. A provider can be turned on with no credential
stored, and a credential can outlive the config entry that used it.

`doctor` runs the environment checks that read config alone. A failing check is `ok: false` inside
a healthy answer, never an error: the request succeeded, and what it found is the answer.
`checkMachine` is the mutation that also dials a provider and the daemon.

`daemon` is answered from what this server already knows, so it works while the daemon is down,
which is the point of asking. `reachable: false` does not mean requests fail, it means the live
frames have stopped. `journal` is the one field there that needs the daemon, and it costs a control
call only where it is selected. A daemon whose journal is refusing writes serves every other field
exactly as before, and `healthy` stays false for the life of the daemon once a write has been lost.

`updatePlan` never reaches the network. The "is there anything newer" half is whatever the last
check found, and asking starts another for whoever asks next rather than waiting on one, so a page
can ask every time it opens. `startUpdate` runs it, `updateJob(id:)` follows one job and
`updateJobs` lists them.

`serverTime` is the daemon's clock in unix epoch seconds. Every duration a run reports is measured
against it, so a client drawing its own clocks should draw them against this rather than the
browser's.

## Live frames

`GET /ws/graphql` carries subscriptions over `graphql-transport-ws`. Authenticate with `?token=`,
because a browser cannot put a header on a WebSocket handshake.

Three streams rather than one, because they answer three different questions: what runs are doing,
what the machine is doing, and how one update is going.

```graphql
subscription Watch($filter: RunInput) {
  runEvents(
    filter: $filter
    includeDescendants: true
    types: [RUN_STATUS_CHANGED, LOG_LINE_WRITTEN, INTERACTION_OPENED]
  ) {
    __typename
    ... on SubscriptionOpenedEvent { seq at serverInstance daemon { reachable version } }
    ... on RunStatusChangedEvent {
      seq at runId status stage iteration
      waitReason { reason needsAPerson }
    }
    ... on LogLineWrittenEvent { seq at runId line }
    ... on InteractionOpenedEvent {
      seq at runId
      interaction { id kind prompt options toolName toolCall { __typename } }
    }
    ... on DaemonLinkChangedEvent { seq at connected restarted restartAdvised }
    ... on EventsDroppedEvent { seq at count }
  }
}
```

The run filter is the listing's own filter, so "every failed run of this blueprint" is the same
words here as in `runs`. It is resolved to a set of runs when the subscription starts, and after
that the set only grows. A run that spawns is checked once its record exists, a run outside the set
is checked again on each status change, and `includeDescendants` puts sub-agents in scope as they
spawn. A run that stops matching keeps sending, because losing the frame that says a run finished
is worse than one extra row.

Only the filter's in-memory half decides those later checks. A condition that would have to open a
file reads as "not matching" for now, and is asked again on that run's next frame.

Every frame carries `seq` and `at`, and both come from `Event`, which every frame implements, the
two transport ones included. `RunEvent` adds `runId`, `agentId` and the `run` itself, and only the
frames about a run implement it, so one fragment reaches the fields every domain frame shares and
another tells domain from transport.

The first frame of every subscription is a `SubscriptionOpenedEvent`. It says which process is
numbering the stream and whether the daemon behind it is reachable, so a client knows the stream is
live without a second request. Two subscriptions reporting different `serverInstance` values were
served by different processes, so a reconnecting client re-reads rather than resumes.

`seq` is that process's own numbering of every frame it sends, not a count of the ones this
subscription received. Two subscriptions open at once see one frame under one number. A
subscription that asked for three frame types sees the numbers of those three and nothing between,
so gaps are the ordinary shape of a filtered stream and mean nothing on their own.

Delivery is at-most-once, and a gap is announced rather than inferred. `EventsDroppedEvent` says
you fell behind, and `count` is how many frames went past unread. It is counted off the server's
numbering, so it includes frames your filter would have dropped anyway: an upper bound, never an
under-count. The broadcast is bounded, and a listener that cannot keep up is skipped past rather
than allowed to hold up the daemon. Treat it as the cue to re-read whatever you render.
`DaemonLinkChangedEvent` arrives whatever `types` says and whatever the scope is, because a run's
frames stopping looks exactly like a quiet run without it.

The machine's own frames are their own stream, so a settings screen subscribes without seeing a
fleet's worth of log lines.

```graphql
subscription Machine {
  machineEvents(types: [DAEMON_LINK_CHANGED, CONFIG_HEALTH_CHANGED]) {
    __typename
    ... on DaemonLinkChangedEvent { seq at connected restarted restartAdvised }
    ... on ConfigHealthChangedEvent { seq at healthy path error { kind message line } }
    ... on UpdateStepChangedEvent { seq at jobId step status detail }
    ... on SubscriptionOpenedEvent { seq at serverInstance }
    ... on EventsDroppedEvent { seq at count }
  }
}
```

One update has a stream of its own, narrowed to that job, because a console watching an install is
watching one install.

```graphql
subscription Installing($id: ID!) {
  updateJobEvents(id: $id) {
    __typename
    ... on UpdateStepChangedEvent { seq at step status detail }
    ... on UpdateFinishedEvent { seq at status restartRequired job { id status } }
    ... on SubscriptionOpenedEvent { seq at serverInstance }
  }
}
```

The frame that says the job finished is the last one it produces. The stream itself stays open
until the client closes it.

There is no replay. A stream is how a client stays current, not how it reconstructs the past, and
for that it reads the run.

## Failures

A failure inside a field is a 200 with an `errors` entry. Each entry carries the `path` in your
query that produced it, so a page of fifty runs where one record will not read still returns the
other forty-nine.

Branch on `extensions.code`, never on the message text. Every failure carries one, a query refused
before any resolver ran included: those are `BAD_USER_INPUT`.

| Code | Means | REST answers |
|---|---|---|
| `BAD_USER_INPUT` | The request is wrong as written. Sending it again unchanged fails the same way | `400` |
| `FORBIDDEN` | This server is configured to refuse it, such as a workdir outside `--workdir-root` | `403` |
| `NOT_FOUND` | Nothing by that name, or nothing in the state the act needs | `404` |
| `CONFLICT` | It exists and its state refuses the change | `409` |
| `PAYLOAD_TOO_LARGE` | An attachment is over this server's `max_upload_bytes` | `413` |
| `UNPROCESSABLE` | Well formed, and something on disk will not answer, such as a `yolo.toml` a profile you never touched has broken | `422` |
| `UPSTREAM` | Something this server depends on answered badly. Retrying may well work | `502` |
| `DAEMON_INCOMPATIBLE` | The daemon was updated under a running server. Restart `lev serve` | `502` |
| `DAEMON_UNAVAILABLE` | The daemon could not be reached. Get it back, then retry | `503` |
| `INTERNAL` | Something failed that you did nothing wrong to cause | `500` |

`extensions.httpStatus` carries that same number, so a client that already knows the REST
vocabulary needs no second table.

Nothing inside a result is a failure. A bulk sweep reports what it did not touch under `skipped`, a
run that ended by itself while the sweep was reaching it reports itself as `ALREADY_FINISHED`, and
a check that found something wrong is a report rather than an error. Those are outcomes, and a
refusal is an error.

## Limits

A query is checked before any of it runs.

| Limit | Value | Why |
|---|---|---|
| Selection depth | 12 | A run's children are runs, so nesting has no natural end |
| Complexity | 10000 | Depth does not bound breadth: fifty runs each asking for fifty children is shallow and large |
| Filter depth | 16 | `and`, `or` and relations nest, and a predicate is walked before anything is read |
| Filter values | 512 | One filter holding thousands of values is a query to split, not a page to serve |
| `first` | Per listing | The page caps above, refused rather than clamped |

The two filter limits are counted in the same walk that builds the cursor, so a filter that is over
them is refused before a single run is touched.

Complexity counts the rows a query asks for. A listing field costs its `first` times what one row
of it costs, so `runs(first: 200) { results { children(first: 200) { results { id } } } }` is
counted as the forty thousand records it asks for and refused. The same query with `first: 20` at
both levels costs four hundred and runs.

Every refusal in this section is a `BAD_USER_INPUT` with `httpStatus` `400`. So is every refusal a
query earns before a resolver runs: an unknown field, an unknown argument, a misspelled enum value,
a `@oneOf` input with two members set, or a document that will not parse.

The keys of every object come back in the order you selected them, from the root down and through
every fragment, as the spec asks. Fields are still resolved together rather than one after another,
so a field that reads the disk costs the same whether it is first or last in the query.

## The schema

The schema is generated from the server's own types, so it cannot describe something the server
does not serve. It is the only one of Leviath's published schemas that is: the OpenAPI spec, the
blueprint schema and the config schema are written by hand and held to the code by tests. Read it
three ways:

* The published file, [`leviath.graphql`](https://leviath.dev/docs/stable/leviath.graphql). It is
  committed, so no command is needed to read it, and a test refuses a build whose schema has moved
  away from it. Each channel publishes its own copy.
* Introspection, which any GraphQL client tool can read live from your own server.
* `lev serve --print-graphql-schema`, which prints what your build serves and exits.

Every type, field, argument and enum value in it carries a description. A client tool that renders
documentation renders the whole thing, and an agent reading the SDL cold has the same answers a
reader of this page does.

New fields and types are added; nothing is removed without being marked deprecated first. Check the
`graphql` capability in `GET /api/config` before choosing this transport, the same way you check any
other [feature](/docs/api#feature-detection).
