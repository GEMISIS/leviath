# Validation notes

## What the checks establish

Run from this directory with `graphql-core==3.2.6`:

| Check | Establishes |
|---|---|
| `check_schema.py` | The schema builds and validates; every element is described; backtick references resolve; filters mirror their outputs, including one arm per union or interface variant; one listing shape with stated page caps; one mutation shape; no orphan types. Prints an informational breaking-change report against `../../schema/leviath.graphql`. |
| `check_kinds.py` | No `kind`-style output field, no enum named like a kind, no enum that repeats a union's or interface's members, no state union beside a status enum. |
| `check_migration.py` | Every coordinate of the published schema has exactly one row in `MIGRATION.md`, every verdict is valid, and every target coordinate exists. |
| `validate_operations.py` | Every operation validates; every operation has examples that coerce; `@oneOf` holds in examples and literals; no page size exceeds its cap; a self-test proves those checks still catch known-bad input. |

They establish nothing about runtime behaviour: resolvers, storage, permissions, paging, event
delivery and jobs all need implementation and integration tests of their own. No operation here is
executed, and the examples use placeholder ids.

## Baseline

`MIGRATION.md` was written against a published schema whose SHA-256 is
`68688952aabb4d9c683d368ae19df556638f3cf531da3ba8c0f4f480e182e628`. Since then the published
schema has gained four coordinates, `AttemptOutcomeInput.finishReason`,
`AttemptOutcomeInput.stoppedFor`, `AttemptOutcomeOutput.finishReason` and
`AttemptOutcomeOutput.stoppedFor`. Against a checkout that has them, `check_migration.py` says the
baseline moved and reports those four as missing. They are not yet in the proposed schema either;
they belong on `AnsweredAttempt` and `AnsweredAttemptFilter`.

## The earlier proposal's operations, ported

The earlier proposal in this PR had 45 operations in seven files. All 45 are ported: 40 fully and
5 in part. None is impossible. The file `scripts-tools.graphql` becomes `extensions-tools.graphql`,
and names that carried the replaced vocabulary are renamed.

| Id | Earlier name | Here | Status |
|---|---|---|---|
| B0 | BlueprintDiscovery | blueprints: BlueprintDiscovery | ported |
| B1 | ValidateBlueprint | blueprints: ValidateBlueprint | ported |
| B2 | BlueprintVersionHistory | blueprints: BlueprintRevisionHistory | ported; an exact revision is read with `node(id:)` |
| B3 | UpdateBlueprintVersion | blueprints: UpdateBlueprintRevision | ported |
| X0 | BlueprintContextDesign | context-architecture: BlueprintContextDesign | ported |
| X1 | FanOutWorkerContracts | context-architecture: FanOutWorkerContracts | in part, see below |
| C0 | PauseRun | control: PauseRun | ported |
| C1 | ResumeRun | control: ResumeRun | ported |
| C2 | CancelRun | control: CancelRun | ported |
| C3 | SendRunMessage | control: SendRunMessage | ported |
| C4 | PauseRunBatch | control: PauseRunBatch | ported; outcomes are types, with `maxAffected` and `dryRun` |
| E0 | WatchRunFamily | events: WatchRunFamily | ported; frames chosen with one Boolean per frame type |
| E1 | WatchMachineEvents | events: WatchMachineEvents | ported |
| E2 | WatchUpdateJob | events: WatchUpdateJob | ported |
| M0 | MachineOrientation | machine: MachineOrientation | ported |
| M1 | ConfigurationHealth | machine: ConfigurationHealth | ported; gateways are model providers |
| M2 | MachineCatalogs | machine: MachineCatalogs | ported |
| M3 | ModelCatalog | machine: ModelCatalog | ported |
| M4 | CheckMachineHealth | machine: CheckMachineHealth | ported; `checkMachine` takes a request (an optional model) |
| M5 | CheckMcpEndpoint | machine: CheckMcpEndpoint | ported |
| M6 | RefreshModelCatalogue | machine: RefreshModelCatalogue | ported |
| M7 | UpdateMachineConfiguration | machine: UpdateMachineConfiguration | ported |
| M8 | ConfigurationVersionHistory | machine: ConfigurationRevisionHistory | ported; revision settings are JSON |
| R0 | SpawnRun | runs: SpawnRun | ported |
| R1 | RunFamily | runs: RunFamily | ported |
| R2 | RunPosition | runs: RunPosition | ported; the stage ledger is a bounded list, not a page |
| R3 | RunBlueprintProvenance | runs: RunBlueprintProvenance | ported |
| R4 | InferenceDiagnosis | runs: InferenceDiagnosis | in part, see below |
| R5 | CurrentRunContext | runs: CurrentRunContext | ported |
| R6 | ContextCausality | runs: ContextCausality | in part, see below |
| R7 | RunDeliverables | runs: RunDeliverables | in part, see below |
| R8 | OpenRunInteractions | runs: OpenRunInteractions | ported through `Run.interactions`, see below |
| R9 | SettleInteraction | runs: SettleInteraction | ported |
| R10 | SettledRunInteractions | runs: SettledRunInteractions | ported |
| R11 | StartRunExport | runs: StartRunExport | ported |
| R12 | RunExportStatus | runs: RunExportStatus | ported |
| T0 | CallableToolsAndSources | extensions-tools: CallableToolsAndSources | ported |
| T1 | UnregisteredScriptCandidates | extensions-tools: UnclaimedExtensionFiles | in part, see below |
| T2 | BlueprintToolTokenResolution | extensions-tools: BlueprintToolTokenResolution | ported |
| T3 | ValidateStageHookDraft | extensions-tools: ValidateStageHookDraft | ported; hook points are an enum |
| T4 | UpsertToolScript | extensions-tools: UpsertToolExtension | ported |
| T5 | HistoricalToolCallResolution | extensions-tools: HistoricalToolCallResolution | ported; the call pins the `ToolRevision` it was shown instead of reading the current catalogue |
| T6 | ScriptToolPolicyPreview | extensions-tools: ApprovalPolicyPreview | ported; the server classifies the tool |
| T7 | ScriptRegistryHealth | extensions-tools: ExtensionCompileHealth | ported |
| T8 | ScriptVersionHistory | extensions-tools: ExtensionRevisionHistory | ported |

Sixteen operations are new, for journeys the earlier schema could not serve: B4 CreateBlueprint,
C5 CancelRunsPreview, E3 UpdateJobStatus (the polling twin of E2), M9 McpToolInventory, M10
AgentGuide, R13 RunsWaitingOnPerson, R14 ParentsWaitingOnSubAgents, R15 RunsWithProblems, R16
RunsOfBlueprintRevision, R17 AwaitRunDecision, R18 StageLogTail, R19 OpenApprovalInbox, T9
CustomToolAndItsFile, and in `history.graphql` H0 RecentChanges, H1 EditsByAuthor and H2
RevisionById.

## What this schema cannot express, or only in part

These are findings about the proposed schema, not about the operations.

- **R4, finish reasons.** The provider's finish reason (`finishReason`, `stoppedFor`) has no field
  on `AnsweredAttempt`; see "Baseline".
- **R6, grouping windows.** `contextSnapshots` lists every recorded window in order, and each
  carries its `digest`, but the schema does not group repeated observations under one unique
  window. A client groups by digest. Nothing reports how complete a run's causal context history
  is; older journals simply have fewer typed links.
- **R7, artifact provenance.** `Artifact` has no id and no link to the call that produced it. The
  reverse link exists (`ToolExecution.artifacts`), so the operation selects the calls instead.
- **R8, one run's open interactions.** `InteractionFilter` has no `run` field, although
  `Interaction.run` exists, so the machine-wide `openInteractions` cannot be narrowed to one run or
  by a run's properties. The operation reads `Run.interactions` with `settlement: { isNull: true }`.
- **T1, unclaimed file content.** `UnclaimedFile` has a path but no content, and only files in a
  blueprint's directory are listed. Reading an unclaimed file before claiming it needs another
  route.
- **X1, malformed fan-out.** A fan-out stage whose worker does not resolve cannot be served as a
  stage, by design: its manifest does not load, so it is a `BrokenBlueprint` whose problems carry
  the rule, the stage and a remedy. The operation reads those.
- **History coverage (B0, B2, M8, T8).** The earlier operations read `historyCoverage`. This
  schema has no such field; see the open questions in [object-versioning.md](object-versioning.md).
- **Exports.** `RunExportField` lacks the two route-derived keys the export writes, `age_secs`
  and `working_secs` (recorded by the earlier proposal).

## Findings from writing the operations

- **Same field name, different nullability, across variants.** GraphQL refuses one selection that
  reads such a field from two variants, so a client must alias one of them. The operations here
  alias `ModelProvider` `baseUrl` (nullable on `ApiKeyProvider`, not on
  `OpenAiCompatibleProvider`), `Extension` `blueprint` (nullable on `MimeCheck` and
  `ToolExtension`), `Interaction` `document` (non-null only on `DocumentEdit`) and `RunState`
  `stage` (non-null only on `Running`). Others exist (`McpServer.configError`, several
  `ToolArguments` members). Aligning them is an open question.
- **An empty-string sentinel.** `UpsertExtensionRequest.expectedRevision: ""` means "only if no
  file exists yet". One example uses it; the design rules say an unknown is null, never `""`.
- **`Decimal` is described as exact.** The earlier proposal records that the daemon's decimals are
  f64 values in their shortest round-tripping form; the description should say so.

## Root fields not used by an operation

`validate_operations.py` prints the root fields no operation selects. Most are writes that mirror
one that is exercised (`deleteBlueprint`, `updateMcpServer`, `resumeRuns`, ...), deprecated
aliases (`doctor`, `yoloProfile`), or single-item reads whose listing is exercised (`mcpServer`,
`modelProvider`, `model`, `mimeType`).
