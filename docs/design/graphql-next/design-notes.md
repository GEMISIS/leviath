# Leviath GraphQL: a proposed contract

**Status: a design proposal.** [`leviath.graphql`](leviath.graphql) is a target schema, not what
`lev serve` answers today, and nothing here claims a runtime implementation. The published
[`../../schema/leviath.graphql`](../../schema/leviath.graphql) stays authoritative until the
resolvers make this one true. The checks in this directory are static: the schema builds, follows
the rules below, maps every published coordinate, and validates a corpus of client operations.
They prove nothing about resolvers, storage or permissions.

The proposal comes from reading the published schema cold, as an agent does, with the resolvers
and core types open beside it; where a description and the code disagreed, the code won. The
findings clustered into a few patterns, stated below as rules, followed by the model they produce.

## 1. What goes wrong today

**Names that say how a thing is built, not what it is.** A Rhai file is a `Script`, named after
its format, and its role is a `ScriptKind` from `TOOL` to `PROVIDER`, plus `CANDIDATE`. That last
value is a state, not a role ("nothing has claimed this file yet"): no compiler accepts it, and
GraphQL never emits it. Approval policies are `YoloProfile`, the command-line flag's name.

**The same thing as two nodes that disagree.** A Rhai tool is a `ScriptOutput` of kind `TOOL`
and a `ScriptToolOutput`. The script is named after the file stem; the tool after its `@tool`
annotation, and the two need not match. `scripts` compiles each directory on its own, while
`tools` also skips a script whose name a built-in or the blueprint's own copy already takes, so
a machine script called `shell` reads `compiles: true` in one listing and sits in
`ToolConnection.skipped` in the other. No field leads from one node to the other.

**Several vocabularies for one question.** "Where does this tool come from?" has four answers
that cut the world differently: `ToolOrigin` (`BUILTIN`, `SUBAGENT`, `BLUEPRINT_SCRIPT`,
`GLOBAL_SCRIPT`), `ToolKind` (`BUILTIN`, `SUBAGENT`, `SCRIPT`, `MCP`), `ScriptScope` (`GLOBAL`,
`BLUEPRINT`) and the `@builtin`/`@scripts`/`@mcp` group tokens. `YoloProfile.decide(tool, kind:
ToolKind!, args)` then asks the caller to classify the tool, although the server has the
classifier; a wrong `kind` gets a wrong decision about group rules, with no error.

**States that need a second field to read.** `RunStatus.WAITING_INPUT` covers "stopped until a
person answers" and "healthily waiting on fifty fan-out workers"; only
`waitReason.needsAPerson` tells them apart. `COMPLETE` is not success until a client has read
`flags`, thirteen fields, and applied its own rule. `RunCompletedEvent` fires for
`ERROR` and `CANCELLED` too. `Run.updatedAt` is described as the last state change, but it
advances on the persistence heartbeat, so a dashboard sorted by it shows wedged runs as fresh.

**A `kind` field deciding which nullable fields apply.** `WaitReason.reason` decides whether
`blocker`, `remedy` or `outstanding` means anything. `AttemptOutcome.kind` does the same for the
four failure fields, `McpServer.transport` for seven fields (and `endpoint` is a command or a URL
depending on it), `Region.kind` for eleven of a region's twenty-seven fields, and
`SandboxConfig.kind`, `ConfigError.kind`, `Gateway.kind` and `MimeRow.origin` likewise. Each is an
instruction the reader cannot see in the types.

**Relations spelled as strings, and values that are not true.** `Stage.availableTools` is a list
of strings in which `@all` and `@mcp` are type tags. A stage with no transitions table falls
through to the next stage at run time, but `Stage.transitions` answers `[]`, documented as "a
terminal stage". A percentage-budget region reports `maxTokens: 0`; a compacting region with no
threshold reports 2147483647; `MimeRow.check: ""` means "lift the broader row's check".

**Questions nobody can ask.** No query lists an MCP server's tools; `checkMcpServer` spawns the
server and answers names as the server spells them, not the `<server>__<tool>` names grants and
approval rules use. `providers` lists only the browser sign-in providers. Whether an update needs
a restart reaches a client only on a subscription frame. A run's blueprint snapshot covers the
manifest but not the hook and tool files it names, and a tool call records its tool only by name
(its `toolDescription` is always null on an execution), so "what exactly did this run execute"
is partly a lookup against the present.

## 2. Design rules

`check_schema.py` and `check_kinds.py` enforce the rules a machine can check. Passing them is
the floor, not the argument.

1. **Plain-English names for what a thing is.** Not its storage, its language or the CLI's word.
   A Rhai file that plugs into a stage is a stage hook; a yolo profile is an `ApprovalPolicy`
   whose description names `--yolo=<name>`. Types are bare nouns (`Run`, not `RunOutput`); inputs
   keep role suffixes (`XFilter`, `XRef`, `VerbNounRequest`).
2. **One concept, one type, and one thing, one node**, however many places it shows up in.
3. **Types, not `kind` fields.** When what a thing is decides which fields it has, each variant
   is a type, behind an interface for the fields they share or in a union when they share none,
   and `__typename` discriminates. No output field is named `kind`, `type`, `category`,
   `variant`, `origin` or `mode`; no enum repeats the names of a union's members. Writes follow:
   an input that takes one of several shapes is `@oneOf`, never a `kind` argument beside a bag of
   optional fields.
4. **A state that carries data is a union of state types, with no status enum beside it** to
   disagree with it. An enum survives only where no value changes the shape.
5. **Filtering by variant uses the types too.** The filter for a union or interface has one field
   per variant, and setting it selects that variant: `runs(filter: { state: { waitingOnPerson:
   {} } })`, or `waitingOnSubAgents: { outstanding: { gt: 3 } }` to add a condition. Subscriptions
   choose frames the same way, with one Boolean per frame type.
6. **A relation is a typed field**, never a name the caller must look up. An authored name stays
   where it is the authored rule (a grant naming a tool this machine lacks), beside the typed
   relation (`ToolByName.matches`).
7. **History is revisions on the object**, and a record of something that happened points at the
   exact revision it used, typed and non-null. See [object-versioning.md](object-versioning.md).
8. **One error rule.** A request that asked for one thing did it or fails whole, as a GraphQL
   error with `extensions.code` (an `ErrorCode`) and, where the code alone is ambiguous,
   `extensions.reason` (an `ErrorReason` naming the rule that refused, never a type: a missing
   thing is `NOT_FOUND` with `extensions.nodeType`). A batch or sweep answers per item, as data.
   A retry of something that already happened succeeds and says so (`alreadyInState`,
   `replayed`); it is never a `CONFLICT`.
9. **One listing shape.** Every collection takes `(filter, orderBy, first, after)` and answers
   `results`, `cursor`, `total`. `first` states its cap in its description, and a `first` over
   the cap is refused, not cut. A list that is small by nature is a plain list whose description
   states the bound.
10. **Writes that agents retry are safe to retry.** A create, spawn or start takes an
    `idempotencyKey`. An update of shared state takes `expectedRevision` (the digest read).
    A sweep by filter requires `maxAffected` and offers `dryRun`, so a filter that happens to
    match everything cannot act on everything.
11. **Everything is pollable.** A tool-calling agent cannot hold a stream, so every frame fact is
    also a query field, and `awaitRun` is a bounded long poll on a state filter.
12. **The server explains itself.** `howToUseThisGraphQLServer` returns the operating guide,
    rendered from source; an agent reads it instead of a copy in its prompt.
13. **Descriptions are the prompt.** Every type, field, argument, input field and enum value is
    described, a backtick name resolves to a real coordinate, and an unknown is `null`, never
    `false`, `0` or `""`.

## 3. The model

### Capabilities: what a run can use

- **Extensions.** `interface Extension` (path, relative path, compile error, revisions) with one
  type per extension point: `ToolExtension`, `StageHook`, `RegionHook`, `OutputValidator`,
  `MimeCheck`, `ProviderExtension`, `DependencyCheck`, `DependencyInstaller`. Ownership is typed:
  `StageHook.blueprint` is non-null, `MimeCheck.blueprint` is null for the operator's own,
  `ProviderExtension` has none. `ExtensionRef` and `ExtensionDraft` are `@oneOf`, so a provider
  inside a blueprint cannot be written. A file nothing names is an `UnclaimedFile`, a state
  rather than a role.
- **Tools.** `interface Tool implements Node & Versioned`, typed by source: `BuiltinTool` (with
  `aliases`, so `bash` and `shell` are one tool), `CustomTool`, `McpTool`, `SubagentTool`.
  `CustomTool.extension` and `ToolExtension.tool` link the two halves of a Rhai tool;
  `ToolExtension.nameTakenBy` says why a file offers nothing.
- **Selectors.** An authored tool list entry is a `ToolSelector`: `ToolByName`,
  `ToolsOfMcpServer`, `ToolsMatching`, one type per group token (`AllBuiltinTools`,
  `AllCustomTools`, ...) and `MachineCustomTools`. The same union selects what a
  `ToolPermissionRule` applies to, in stages and in approval policies alike.
- **MCP servers.** `StdioMcpServer`, `HttpMcpServer`, `UnresolvedMcpServer`, each with
  `toolListing: McpToolsListed | McpToolListFailed`; `tools` lists MCP tools under the names
  grants use.
- **Model providers.** One `interface ModelProvider` over seven types, from `ApiKeyProvider` to
  `ScriptProvider`, in one complete listing; `Model.provider` is the relation.
- **Approval policies.** `ApprovalPolicy`, built-in default included; `decide` classifies the
  tool itself.
- **Mime types.** `MimeType` is what a type resolves to; `interface MimeTypeRule` has one type
  per layer that wrote a rule (built-in, provider, operator, blueprint), and `liftsCheck`
  replaces the empty-string convention.

### Runs

- **State.** `Run.state: RunState!`, a union of thirteen types, each carrying what a caller acts
  on: `WaitingOnPerson { questions }`, `WaitingOnSubAgents { outstanding, unfinished,
  questionsBelow }`, `NeedsSetup { blocker, remedy, provider }`, `Lost { claimed, lastMovedAt }`,
  and `Completed`, `CompletedWithProblems`, `Failed`, `Cancelled` behind `interface Finished`.
  `WaitingUnexplained` and `UnrecognizedState` let old records and newer daemons read honestly.
  The daemon already holds most of this as data-carrying Rust enums; only the API shape changes.
- **Problems.** `Run.problems: [RunProblem!]!` turns the flags into eleven typed findings, so
  `Completed` means a clean record. `isHosted` and `lastMovedAt` replace the heartbeat-driven
  timestamp.
- **Where it ran.** `RunStage` (this run's row for a declared `Stage`), `StageVisit` (one stay)
  and `Turn` (one ask of the model and the calls its answer requested) replace stage names and
  indexes in eleven places.
- **Model calls.** `interface ModelAttempt` with `AnsweredAttempt` (usage, cost, requested
  calls) and `FailedAttempt` (failure, transient, at capacity, next step).
- **Tool calls.** One `ToolCall` whose `arguments` is a union of per-tool argument types,
  replacing thirty-six wrapper types; `ToolCall.tool` is the `ToolRevision` the model was shown.
  `ToolExecution` gains `run`, `turn`, `approval`, `spawnedRuns` and a `state` union.
- **Interactions.** Six types (`ToolApproval`, `TaintGateApproval`, `TextQuestion`,
  `ChoiceQuestion`, `ConfirmQuestion`, `DocumentEdit`), each settled only by the settlements its
  question allows, so a choice cannot settle as `Approved`. Open and settled asks are one list.
- **Context.** `ContextSnapshot` is one content-addressed window; `ContextChange` is typed by who
  changed it (tool, model, message, interaction, runtime), with `before` and `after`.
- **Output.** `Answer` owns its `artifacts`; logs are read per stage, from an offset.

### Blueprints

A `Blueprint` is the installed, named thing; its content is a `BlueprintRevision`: the manifest,
every file it names, and the graph resolved from them. A manifest that loads is a `Blueprint`, so
every stage and region reference in it is a non-null typed relation and the twenty-one `…Name`
twin fields go. One that does not load is a `BrokenBlueprint` with typed problems, instead of
vanishing from the listing. Stages are `AutonomousStage`, `OutputStage`, `InteractionPointStage`
and `FanOutStage`; regions are one type per kind, plus `RuntimeRegion` for the four the runtime
supplies; transitions are one type per condition, plus `FallThroughEdge` for the stage that
declares none. Budgets are served as written (`TokenCount | WindowShareBudget`). Settings the
parser reads and the schema never served (`transition_region`, a fan-out's `items_region`,
`hooks.on_terminal`, region content rules) are fields. The filter over manifest structure,
ninety input types for a catalogue of tens, shrinks to name, id and revision.

### The machine and jobs

Each fact lives on one root. `server` is this `lev serve` process; `daemon` is the daemon behind
it, with its two connections (`control` and `events`) reported separately instead of one boolean;
`config` is the file, with `problem: FileProblem` typed by the step that failed and a revision.
Three words mean three things: `validate…` judges text you send and is a query; `check…`
contacts the real thing (it may dial, spawn or bill) and is a mutation whose finding is data,
`passed: false`, not an `UPSTREAM` error; `diagnostics` is the offline half of `checkMachine`.
`UpdateJob` and `RunExport` implement `interface Job` with a `JobState` union and clocks; their
ids are tagged (`updateJob:…`, `runExport:…`); `runExports` lists exports; a download link says
when it expires.

### Events

The three subscriptions stay. Frames carry the same objects queries answer with (`Daemon`,
`Config`, `UpdateJob`, `Run`), and `RunFinishedEvent` replaces the misnamed completion frame.

## 4. What breaks

This is a breaking release, deliberately, and `check_schema.py` reports its size against the
published schema: 705 breaking and 66 dangerous changes. Most type removals are renames: GraphQL
cannot alias a type, so `RunOutput` becoming `Run` breaks every fragment on it once. Beyond
renames:

- `Timestamp` (epoch seconds, 45 output fields) becomes `DateTime` (ISO-8601 UTC).
- Kind and status enums become types: clients branch on `__typename` and filter with variant
  arms. `RunStatus`, `ScriptKind`, `ToolOrigin`, `WaitReasonKind` and their kin go.
- Sweeps require `maxAffected`. `updateConfig` takes typed provider writes. `RunCompletedEvent`
  becomes `RunFinishedEvent`. Job ids gain tags. Checks report a failure as data. Blueprint
  digests change, because they now cover every file a manifest names. `CreateBlueprintRequest`
  takes its name from the manifest.

What softens it: 141 renamed output fields keep their old name as a `@deprecated` alias for one
release, and [MIGRATION.md](MIGRATION.md) gives every published coordinate (4,109 of them) a row:
1,363 kept, 459 renamed, 721 reshaped, 212 merged, 1,354 dropped, most of the last being filter
and order plumbing that follows its output.

## 5. What the runtime needs

**Reshaping only**, where the data is already in the process and the GraphQL types change:
`RunState` from the status, the data-carrying wait reason, the error and the flags; `RunProblem`
from `RunFlags`; the extension interface from the script registry; `ModelAttempt` from the
attempt outcome; `ToolCall` from the same argument parse; interactions from their kind and
request-id prefix; one model provider list merged from the places providers live today;
`Sandbox`, `FileProblem` and `Job` from Rust enums the projection currently flattens; subtree
cost summed the way subtree tokens already are.

**New data**, which the daemon or the journal must start keeping:

- a content-addressed revision store (number, time, author, reason) for approval policies,
  extensions, MCP servers, providers, config and operator mime rules;
- per run: the tool definitions offered, a snapshot of every file the blueprint names, and the
  provider revision per attempt;
- on journal records: the attempt id on usage records; visit and turn on attempts; the whole
  interaction request and when it opened; taint levels on a gate; the interaction and spawned
  runs on an execution; the attempt or interaction behind a context change; a run's end time;
- MCP tool listings with each tool's description and schema; per-layer mime rules; keyed
  providers' base URLs and header names; job start and finish times.

Every new-data field is nullable, or empty for old records, and says so; none of it changes what a
run does.

**Derive work** in `leviath-graphql-derive`: the suffix legend in `names.rs`; `#[mirror]` on
interfaces, writing one arm per implementer as it already does for unions; deprecated aliases
for fields, arguments and input fields, carried into mirrored filters; `#[filter(skip)]` on
manifest structure; generated filter descriptions that match the text in this file.

## 6. Relation to the code-first schema

Leviath's schema is generated from the server's types, and that stays true. This SDL is the
target the derive must regenerate: the change is done when the docs task writes
`docs/schema/leviath.graphql` equal to this file, or to the version review agrees. It is not a
second source of truth, and once the generated schema catches up this directory has done its job.

## 7. Runtime facts from the earlier proposal

The earlier proposal in this PR recorded source-grounded facts about the runtime. These are the
ones this model must respect, with where it stands on each:

- **`Decimal` is the daemon's f64 in its shortest round-tripping form**, not arbitrary precision.
  This schema's `Decimal` description says "exact decimal"; that sentence needs correcting.
- **Event `seq` is a server-wide counter shared by all streams**: filtering leaves gaps,
  delivery is live and at most once, and only `EventsDroppedEvent` signals loss. The description
  of `Event.seq` agrees.
- **A settled interaction may not say whether the ask was required**, because journals may not
  keep it. `Interaction.isRequired` is nullable and deprecated here.
- **Never infer what a run executed from an installed fallback.** Here `Run.blueprint` is
  non-null and, for a run recorded before snapshots, serves the installed revision marked by
  `blueprintSource: INSTALLED_FALLBACK`. A client must check it; see the open questions.
- **A current stage position must survive a missing definition.** `RunStage.definition` is
  non-null here, which holds only while every run's revision loads.
- **Configuration history keeps no secret values.** A secret-only write still advances the
  revision, though the redacted settings look unchanged; and a write must notice an on-disk change
  since the revision read and refuse, rather than overwrite an invalid external edit.
- **The export also writes two route-derived keys**, `age_secs` and `working_secs`, which this
  schema's `RunExportField` does not list.
- Opaque cursors, page caps, export expiry, update-job retention and `extensions.httpStatus`
  are kept.

## 8. Open questions for maintainers

1. One breaking release, or a side-by-side endpoint for a migration window?
2. `DateTime`, or keep `Timestamp` and take one fewer break?
3. Type growth: 654 named types become 798, mostly interface fields each implementer repeats.
   The alternative for stages and regions is one type with a `mode` union, one hop deeper.
4. Should `lev serve` judge `Lost` (the rule `lev ps` applies) and own the clean-or-flagged
   verdict?
5. Serve only manifests that load, and refuse API installs that don't?
6. `mode = "interactive"` has no runtime effect. Deprecate, implement or remove it? Here it
   loads as `AutonomousStage` with a warning.
7. Runs from before blueprint snapshots whose blueprint is gone cannot satisfy a non-null
   `Run.blueprint`. Synthesize a revision, or make the field nullable?
8. An extension's id embeds its type, so re-purposing a file changes its identity and starts a
   new history. The earlier proposal kept source identity across registration. Which is right?
9. The earlier proposal reports how complete a history is (`historyCoverage`). This schema has no
   such signal; a gap shows only as an edit with no author. Add one?
10. Config, provider and MCP server revisions hold `settings: JSON`. Type them?
11. Built-in tool revisions change with each build, and `Model` is not versioned, so a price
    change is not pinned per attempt. Scope either?
12. `UpsertExtensionRequest.expectedRevision: ""` means "only if no file exists": a sentinel the
    rules above forbid. A separate field instead?
13. Several variants share a field name with different nullability (`baseUrl` on model
    providers, `blueprint` on extensions, `document` on interactions, `stage` on run states), which
    forces aliases in clients. Align them?
14. The published schema gained `finishReason` and `stoppedFor` on attempt outcomes after
    `MIGRATION.md` was written. They belong on `AnsweredAttempt`.
15. "Turn" or "iteration"? The API says turn; manifests say `max_iterations`.
