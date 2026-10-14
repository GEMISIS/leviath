# Leviath GraphQL: v1 to v2 migration

Every published v1 coordinate (type, field, argument, input field, enum value, union
member, root field) has one row here: 4109 rows under 654 v1 types. `check_migration.py`
fails when a v1 coordinate is missing, appears twice, or a coordinate in the v2 column
does not exist in `leviath.graphql`.

**Verdicts** (1363 kept, 459 renamed, 721 reshaped, 212 merged, 1354 dropped):

- **kept**: same name and type. The v2 column says where it lives; "on `T`" flags a field
  that moved to another type than the one in the heading (usually a revision).
- **renamed**: same value, new name. Where a v1 output field survives on the same type
  under a new name, the old name stays for one release as a `@deprecated(reason: "Use …")`
  alias (the why column says so). A renamed type has no alias: GraphQL cannot alias a type.
- **reshaped**: the value is still there in another shape (a type instead of an enum value,
  a relation instead of an id, a union instead of a flag, a typed field instead of a string).
  "now `T`" gives the new type.
- **merged**: folded into another coordinate that already carries it.
- **dropped**: gone; the why column says why, and what to read instead when there is something.

**Rules that explain most rows:**

- Suffixes: `XOutput` is `X`, a filter `XInput` is `XFilter`, `XListInput` is `XListFilter`,
  `Timestamp` is `DateTime` (ISO-8601 instead of epoch seconds).
- A kind, status or origin enum whose values name the variants became types: select with
  `__typename` or `... on`, filter with the variant arms of the interface's filter.
- Plumbing follows its output: a filter field, order field or connection maps wherever the
  field or type it mirrors went, and goes when that went.
- The 36 per-tool call wrappers (`ShellCall`, `ReadFileCall`, …) are one `ToolCall`, typed by
  its `arguments` union; the `…Args` objects are the `…Arguments` members of that union.
- History: every changeable object is `Versioned`; what v1 read as a digest or a revision string
  is a `Revision` object, and writes send `expectedRevision`.

### `AnswerInteractionRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `AnswerInteractionRequest` | `AnswerInteractionRequest` | kept |  |
| `AnswerInteractionRequest.interactionId` | `AnswerInteractionRequest.interactionId` | kept |  |
| `AnswerInteractionRequest.answer` | `AnswerInteractionRequest.answer` | kept |  |

### `AnswerInteractionResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `AnswerInteractionResult` | `AnswerInteractionResult` | kept |  |
| `AnswerInteractionResult.interactionId` | `AnswerInteractionResult.interaction` (`Interaction.id`) | merged | the interaction is returned whole |
| `AnswerInteractionResult.outcome` | `AnswerInteractionResult.alreadyInState` | merged | duplicated `alreadyInState`; any other settlement is a `CONFLICT` |

### `AnswerOutcome`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `AnswerOutcome` | — | dropped | duplicated `alreadyInState`; any other outcome is a `CONFLICT` |
| `AnswerOutcome.ACCEPTED` | `AnswerInteractionResult.alreadyInState` false | dropped | this answer settled it |
| `AnswerOutcome.ALREADY_SETTLED` | `ErrorCode.CONFLICT` with `ErrorReason.INTERACTION_SETTLED` | dropped | a different settlement is refused, never a quiet success |

### `ApprovalScope`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ApprovalScope` | `ApprovalScope` | kept |  |
| `ApprovalScope.ONCE` | `ApprovalScope.ONCE` | kept |  |
| `ApprovalScope.STAGE` | `ApprovalScope.STAGE` | kept |  |
| `ApprovalScope.RUN` | `ApprovalScope.RUN` | kept |  |

### `ApprovalScopeFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ApprovalScopeFilter` | `ApprovalScopeFilter` | kept |  |
| `ApprovalScopeFilter.eq` | `ApprovalScopeFilter.eq` | kept |  |
| `ApprovalScopeFilter.ne` | `ApprovalScopeFilter.ne` | kept |  |
| `ApprovalScopeFilter.in` | `ApprovalScopeFilter.in` | kept |  |
| `ApprovalScopeFilter.notIn` | `ApprovalScopeFilter.notIn` | kept |  |
| `ApprovalScopeFilter.isNull` | `ApprovalScopeFilter.isNull` | kept |  |

### `ApproveWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ApproveWrite` | `ApproveWrite` | kept |  |
| `ApproveWrite.scope` | `ApproveWrite.scope` | kept |  |

### `ArtifactByPathOutput` → `ArtifactArgument`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ArtifactByPathOutput` | `ArtifactArgument` | merged | one type for an artifact the model named |
| `ArtifactByPathOutput.path` | `ArtifactArgument.path` | merged |  |

### `ArtifactConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ArtifactConnection` | — | dropped | a plain list in v2, bounded by what holds it |
| `ArtifactConnection.results` | — | dropped | follows its connection |
| `ArtifactConnection.cursor` | — | dropped | follows its connection |
| `ArtifactConnection.total` | — | dropped | follows its connection |

### `ArtifactDescribedOutput` → `ArtifactArgument`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ArtifactDescribedOutput` | `ArtifactArgument` | merged | one type for an artifact the model named |
| `ArtifactDescribedOutput.path` | `ArtifactArgument.path` | merged |  |
| `ArtifactDescribedOutput.name` | `ArtifactArgument.name` | merged |  |
| `ArtifactDescribedOutput.mimeType` | `ArtifactArgument.mimeType` | merged |  |

### `ArtifactInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ArtifactInput` | — | dropped | nothing filters on `Artifact` in v2 |
| `ArtifactInput.name` | — | dropped | follows `Artifact.name`, which v2 does not filter on |
| `ArtifactInput.mimeType` | — | dropped | follows `Artifact.mimeType`, which v2 does not filter on |
| `ArtifactInput.size` | — | dropped | follows `Artifact.size`, which v2 does not filter on |
| `ArtifactInput.sha256` | — | dropped | follows `Artifact.sha256`, which v2 does not filter on |
| `ArtifactInput.path` | — | dropped | follows `Artifact.path`, which v2 does not filter on |
| `ArtifactInput.and` | — | dropped | follows its filter |
| `ArtifactInput.or` | — | dropped | follows its filter |
| `ArtifactInput.not` | — | dropped | follows its filter |
| `ArtifactInput.isNull` | — | dropped | follows its filter |

### `ArtifactListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ArtifactListInput` | — | dropped | no v2 listing filters a list of `Artifact` (now `Artifact`) |
| `ArtifactListInput.some` | — | dropped | follows its list filter |
| `ArtifactListInput.every` | — | dropped | follows its list filter |
| `ArtifactListInput.none` | — | dropped | follows its list filter |
| `ArtifactListInput.isNull` | — | dropped | follows its list filter |

### `ArtifactOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ArtifactOrder` | — | dropped | the listing it sorted went or became a plain list |
| `ArtifactOrder.field` | — | dropped | the listing it sorted went or became a plain list |
| `ArtifactOrder.direction` | — | dropped | the listing it sorted went or became a plain list |

### `ArtifactOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ArtifactOrderField` | — | dropped | the listing it sorted went or became a plain list |
| `ArtifactOrderField.NAME` | — | dropped | not a sort key in v2 |

### `ArtifactOutput` → `Artifact`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ArtifactOutput` | `Artifact` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `ArtifactOutput.name` | `Artifact.name` | kept |  |
| `ArtifactOutput.mimeType` | `Artifact.mimeType` | kept |  |
| `ArtifactOutput.size` | `Artifact.size` | kept |  |
| `ArtifactOutput.sha256` | `Artifact.sha256` | kept |  |
| `ArtifactOutput.path` | `Artifact.path` | kept |  |
| `ArtifactOutput.url` | `Artifact.url` | kept |  |

### `AskUserChoiceArgsOutput` → `ChoiceQuestionArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `AskUserChoiceArgsOutput` | `ChoiceQuestionArguments` | renamed | argument types are named `XArguments` |
| `AskUserChoiceArgsOutput.prompt` | `ChoiceQuestionArguments.prompt` | kept |  |
| `AskUserChoiceArgsOutput.options` | `ChoiceQuestionArguments.options` | kept |  |

### `AskUserChoiceCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `AskUserChoiceCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `AskUserChoiceCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `AskUserChoiceCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `AskUserChoiceCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `AskUserChoiceCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `ChoiceQuestionArguments` for `ask_user_choice` |

### `AskUserConfirmArgsOutput` → `QuestionArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `AskUserConfirmArgsOutput` | `QuestionArguments` | merged | `ask_user_confirm` and `ask_user_text` take the same arguments; `ToolCall.toolName` says which |
| `AskUserConfirmArgsOutput.prompt` | `QuestionArguments.prompt` | kept |  |

### `AskUserConfirmCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `AskUserConfirmCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `AskUserConfirmCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `AskUserConfirmCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `AskUserConfirmCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `AskUserConfirmCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `QuestionArguments` for `ask_user_confirm` |

### `AskUserTextArgsOutput` → `QuestionArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `AskUserTextArgsOutput` | `QuestionArguments` | merged | same arguments as `ask_user_confirm` |
| `AskUserTextArgsOutput.prompt` | `QuestionArguments.prompt` | kept |  |

### `AskUserTextCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `AskUserTextCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `AskUserTextCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `AskUserTextCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `AskUserTextCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `AskUserTextCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `QuestionArguments` for `ask_user_text` |

### `AttachmentWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `AttachmentWrite` | `AttachmentWrite` | kept |  |
| `AttachmentWrite.path` | `AttachmentWrite.path` | kept |  |
| `AttachmentWrite.region` | `AttachmentWrite.region` | kept |  |
| `AttachmentWrite.name` | `AttachmentWrite.name` | kept |  |
| `AttachmentWrite.mimeType` | `AttachmentWrite.mimeType` | kept |  |
| `AttachmentWrite.deliver` | `AttachmentWrite.delivery` | renamed | typed as `Delivery` |
| `AttachmentWrite.caption` | `AttachmentWrite.caption` | kept |  |

### `AttemptOutcomeInput` → `FailedAttemptFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `AttemptOutcomeInput` | `FailedAttemptFilter` | renamed | follows `FailedAttempt` |
| `AttemptOutcomeInput.kind` | — | dropped | follows `AttemptOutcome.kind`, which went |
| `AttemptOutcomeInput.failureKind` | `FailedAttemptFilter.failure` | renamed | follows `FailedAttempt.failure` |
| `AttemptOutcomeInput.transient` | `FailedAttemptFilter.transient` | renamed | follows `FailedAttempt.transient` |
| `AttemptOutcomeInput.capacity` | `FailedAttemptFilter.atCapacity` | renamed | follows `FailedAttempt.atCapacity` |
| `AttemptOutcomeInput.retry` | `FailedAttemptFilter.next` | renamed | follows `FailedAttempt.next` |
| `AttemptOutcomeInput.and` | `FailedAttemptFilter.and` | reshaped | now `[FailedAttemptFilter]` |
| `AttemptOutcomeInput.or` | `FailedAttemptFilter.or` | reshaped | now `[FailedAttemptFilter]` |
| `AttemptOutcomeInput.not` | `FailedAttemptFilter.not` | reshaped | now `FailedAttemptFilter` |
| `AttemptOutcomeInput.isNull` | `FailedAttemptFilter.isNull` | kept |  |
| `AttemptOutcomeInput.finishReason` | `AnsweredAttemptFilter.ending` | reshaped | the recognised reason is `EndedForFilter.reason`; newer than this repo's copy of v1 |
| `AttemptOutcomeInput.stoppedFor` | `AnsweredAttemptFilter.ending` | reshaped | the provider's words are `EndedForUnrecognizedReasonFilter.providerReason`; newer than this repo's copy of v1 |

### `AttemptOutcomeKind`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `AttemptOutcomeKind` | — | dropped | the `ModelAttempt` implementer is the outcome |
| `AttemptOutcomeKind.SUCCEEDED` | `AnsweredAttempt` | reshaped | a type per outcome |
| `AttemptOutcomeKind.FAILED` | `FailedAttempt` | reshaped | a type per outcome |

### `AttemptOutcomeKindFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `AttemptOutcomeKindFilter` | — | dropped | `AttemptOutcomeKind` went |
| `AttemptOutcomeKindFilter.eq` | — | dropped | follows `AttemptOutcomeKind` |
| `AttemptOutcomeKindFilter.ne` | — | dropped | follows `AttemptOutcomeKind` |
| `AttemptOutcomeKindFilter.in` | — | dropped | follows `AttemptOutcomeKind` |
| `AttemptOutcomeKindFilter.notIn` | — | dropped | follows `AttemptOutcomeKind` |
| `AttemptOutcomeKindFilter.isNull` | — | dropped | follows its filter |

### `AttemptOutcomeOutput` → `FailedAttempt`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `AttemptOutcomeOutput` | `FailedAttempt` | merged | folded into the attempt types |
| `AttemptOutcomeOutput.kind` | `__typename` | dropped | the attempt type is the outcome: `AnsweredAttempt`, `FailedAttempt` |
| `AttemptOutcomeOutput.failureKind` | `FailedAttempt.failure` | renamed | on the failed attempt; old name kept one release, deprecated |
| `AttemptOutcomeOutput.transient` | `FailedAttempt.transient` | merged | on the failed attempt |
| `AttemptOutcomeOutput.capacity` | `FailedAttempt.atCapacity` | renamed | on the failed attempt; old name kept one release, deprecated |
| `AttemptOutcomeOutput.retry` | `FailedAttempt.next` | renamed | what the retry loop did next; old name kept one release, deprecated |
| `AttemptOutcomeOutput.finishReason` | `AnsweredAttempt.ending` | reshaped | a union: `EndedFor.reason` (a `FinishReason`) or `EndedForUnrecognizedReason.providerReason`; newer than this repo's copy of v1 |
| `AttemptOutcomeOutput.stoppedFor` | `EndedForUnrecognizedReason.providerReason` | reshaped | only exists when the reason is not recognised, so it lives on that variant; newer than this repo's copy of v1 |

### `BigInt`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BigInt` | `BigInt` | kept |  |

### `BigIntFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BigIntFilter` | `BigIntFilter` | kept |  |
| `BigIntFilter.eq` | `BigIntFilter.eq` | kept |  |
| `BigIntFilter.ne` | `BigIntFilter.ne` | kept |  |
| `BigIntFilter.in` | `BigIntFilter.in` | kept |  |
| `BigIntFilter.notIn` | `BigIntFilter.notIn` | kept |  |
| `BigIntFilter.lt` | `BigIntFilter.lt` | kept |  |
| `BigIntFilter.lte` | `BigIntFilter.lte` | kept |  |
| `BigIntFilter.gt` | `BigIntFilter.gt` | kept |  |
| `BigIntFilter.gte` | `BigIntFilter.gte` | kept |  |
| `BigIntFilter.isNull` | `BigIntFilter.isNull` | kept |  |

### `BinaryUpgrade`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BinaryUpgrade` | `BinaryUpgrade` | kept |  |
| `BinaryUpgrade.UpgradeByCommandOutput` | `BinaryUpgrade.UpgradeByCommand` | kept | suffix rule |
| `BinaryUpgrade.UpgradeByAdviceOutput` | `BinaryUpgrade.UpgradeByAdvice` | kept | suffix rule |

### `BlobEntryConnection` → `StoredPartConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlobEntryConnection` | `StoredPartConnection` | renamed | follows `StoredPart` |
| `BlobEntryConnection.results` | `StoredPartConnection.results` | renamed |  |
| `BlobEntryConnection.cursor` | `StoredPartConnection.cursor` | renamed |  |
| `BlobEntryConnection.total` | `StoredPartConnection.total` | renamed |  |

### `BlobEntryInput` → `StoredPartFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlobEntryInput` | `StoredPartFilter` | renamed | follows `StoredPart` |
| `BlobEntryInput.sha256` | `StoredPartFilter.sha256` | renamed | follows `StoredPart.sha256` |
| `BlobEntryInput.mimeType` | `StoredPartFilter.mimeType` | renamed | follows `StoredPart.mimeType` |
| `BlobEntryInput.name` | `StoredPartFilter.name` | renamed | follows `StoredPart.name` |
| `BlobEntryInput.size` | `StoredPartFilter.size` | renamed | follows `StoredPart.size` |
| `BlobEntryInput.width` | `StoredPartFilter.width` | renamed | follows `StoredPart.width` |
| `BlobEntryInput.height` | `StoredPartFilter.height` | renamed | follows `StoredPart.height` |
| `BlobEntryInput.durationMs` | `StoredPartFilter.durationMs` | renamed | follows `StoredPart.durationMs` |
| `BlobEntryInput.tokens` | `StoredPartFilter.tokens` | renamed | follows `StoredPart.tokens` |
| `BlobEntryInput.regions` | — | dropped | follows `StoredPart.heldIn`, which v2 does not filter on |
| `BlobEntryInput.stored` | `StoredPartFilter.isStored` | renamed | follows `StoredPart.isStored` |
| `BlobEntryInput.and` | `StoredPartFilter.and` | reshaped | now `[StoredPartFilter]` |
| `BlobEntryInput.or` | `StoredPartFilter.or` | reshaped | now `[StoredPartFilter]` |
| `BlobEntryInput.not` | `StoredPartFilter.not` | reshaped | now `StoredPartFilter` |
| `BlobEntryInput.isNull` | `StoredPartFilter.isNull` | kept |  |

### `BlobEntryListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlobEntryListInput` | — | dropped | no v2 listing filters a list of `BlobEntry` (now `StoredPart`) |
| `BlobEntryListInput.some` | — | dropped | follows its list filter |
| `BlobEntryListInput.every` | — | dropped | follows its list filter |
| `BlobEntryListInput.none` | — | dropped | follows its list filter |
| `BlobEntryListInput.isNull` | — | dropped | follows its list filter |

### `BlobEntryOrder` → `StoredPartOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlobEntryOrder` | `StoredPartOrder` | renamed | follows `StoredPart` |
| `BlobEntryOrder.field` | `StoredPartOrder.field` | reshaped | now `StoredPartOrderField` |
| `BlobEntryOrder.direction` | `StoredPartOrder.direction` | renamed |  |

### `BlobEntryOrderField` → `StoredPartOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlobEntryOrderField` | `StoredPartOrderField` | renamed | follows `StoredPart` |
| `BlobEntryOrderField.SHA_256` | — | dropped | not a sort key in v2 |

### `BlobEntryOutput` → `StoredPart`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlobEntryOutput` | `StoredPart` | reshaped | the glossary's word; a blob is its storage |
| `BlobEntryOutput.sha256` | `StoredPart.sha256` | kept |  |
| `BlobEntryOutput.mimeType` | `StoredPart.mimeType` | kept |  |
| `BlobEntryOutput.name` | `StoredPart.name` | kept |  |
| `BlobEntryOutput.size` | `StoredPart.size` | kept |  |
| `BlobEntryOutput.width` | `StoredPart.width` | kept |  |
| `BlobEntryOutput.height` | `StoredPart.height` | kept |  |
| `BlobEntryOutput.durationMs` | `StoredPart.durationMs` | kept |  |
| `BlobEntryOutput.tokens` | `StoredPart.tokens` | kept |  |
| `BlobEntryOutput.regions` | `StoredPart.heldIn` | reshaped | the `ContextRegion`s, not names |
| `BlobEntryOutput.stored` | `StoredPart.isStored` | renamed | a boolean reads `is...`; old name kept one release, deprecated |
| `BlobEntryOutput.url` | `StoredPart.url` | kept |  |

### `BlueprintConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlueprintConnection` | `BlueprintConnection` | kept |  |
| `BlueprintConnection.results` | `BlueprintConnection.results` | kept |  |
| `BlueprintConnection.cursor` | `BlueprintConnection.cursor` | kept |  |
| `BlueprintConnection.total` | `BlueprintConnection.total` | kept |  |

### `BlueprintDependencyInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlueprintDependencyInput` | — | dropped | nothing filters on `Dependency` in v2 |
| `BlueprintDependencyInput.name` | — | dropped | follows `Dependency.name`, which v2 does not filter on |
| `BlueprintDependencyInput.kind` | — | dropped | follows `BlueprintDependency.kind`, which went |
| `BlueprintDependencyInput.required` | — | dropped | follows `Dependency.required`, which v2 does not filter on |
| `BlueprintDependencyInput.remedy` | — | dropped | follows `Dependency.remedy`, which v2 does not filter on |
| `BlueprintDependencyInput.description` | — | dropped | follows `Dependency.description`, which v2 does not filter on |
| `BlueprintDependencyInput.install` | — | dropped | follows `BlueprintDependency.install`, which went |
| `BlueprintDependencyInput.server` | — | dropped | follows `McpServerDependency.serverName`, which v2 does not filter on |
| `BlueprintDependencyInput.env` | — | dropped | follows `McpServerDependency.variables`, which v2 does not filter on |
| `BlueprintDependencyInput.var` | — | dropped | follows `EnvironmentVariableDependency.variable`, which v2 does not filter on |
| `BlueprintDependencyInput.command` | — | dropped | follows `ProgramDependency.command`, which v2 does not filter on |
| `BlueprintDependencyInput.check` | — | dropped | follows `ScriptedDependency.check`, which v2 does not filter on |
| `BlueprintDependencyInput.and` | — | dropped | follows its filter |
| `BlueprintDependencyInput.or` | — | dropped | follows its filter |
| `BlueprintDependencyInput.not` | — | dropped | follows its filter |
| `BlueprintDependencyInput.isNull` | — | dropped | follows its filter |

### `BlueprintDependencyListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlueprintDependencyListInput` | — | dropped | no v2 listing filters a list of `BlueprintDependency` (now `Dependency`) |
| `BlueprintDependencyListInput.some` | — | dropped | follows its list filter |
| `BlueprintDependencyListInput.every` | — | dropped | follows its list filter |
| `BlueprintDependencyListInput.none` | — | dropped | follows its list filter |
| `BlueprintDependencyListInput.isNull` | — | dropped | follows its list filter |

### `BlueprintDependencyOutput` → `Dependency`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlueprintDependencyOutput` | `Dependency` | reshaped | one type per kind behind `Dependency` |
| `BlueprintDependencyOutput.name` | `Dependency.name` | kept |  |
| `BlueprintDependencyOutput.kind` | `__typename` | reshaped | `McpServerDependency`, `EnvironmentVariableDependency`, `ProgramDependency`, `ScriptedDependency` |
| `BlueprintDependencyOutput.required` | `Dependency.required` | kept |  |
| `BlueprintDependencyOutput.remedy` | `Dependency.remedy` | kept |  |
| `BlueprintDependencyOutput.description` | `Dependency.description` | kept |  |
| `BlueprintDependencyOutput.install` | `EnvironmentVariableDependency.install`, `ProgramDependency.install`, `ScriptedDependency.install` | reshaped | on the kinds that install |
| `BlueprintDependencyOutput.server` | `McpServerDependency.serverName` | reshaped | MCP servers only |
| `BlueprintDependencyOutput.env` | `McpServerDependency.variables` | reshaped | MCP servers only |
| `BlueprintDependencyOutput.var` | `EnvironmentVariableDependency.variable` | reshaped | environment variables only |
| `BlueprintDependencyOutput.command` | `ProgramDependency.command` | reshaped | programs only |
| `BlueprintDependencyOutput.check` | `ScriptedDependency.check` | reshaped | the check, as the revision holds it |

### `BlueprintInput` → `BlueprintFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlueprintInput` | `BlueprintFilter` | kept |  |
| `BlueprintInput.id` | `BlueprintFilter.id` | kept |  |
| `BlueprintInput.name` | `BlueprintFilter.name` | kept |  |
| `BlueprintInput.digest` | `BlueprintRevisionFilter.digest` | renamed | follows `BlueprintRevision.digest` |
| `BlueprintInput.source` | `RunFilter.blueprintSource` | renamed | follows `Run.blueprintSource` |
| `BlueprintInput.version` | `BlueprintRevisionFilter.version` | renamed | follows `BlueprintRevision.version` |
| `BlueprintInput.description` | `BlueprintRevisionFilter.description` | renamed | follows `BlueprintRevision.description` |
| `BlueprintInput.entryStage` | — | dropped | follows `BlueprintRevision.entryStage`, which v2 does not filter on |
| `BlueprintInput.entryStageName` | — | dropped | follows `BlueprintRevision.entryStage`, which v2 does not filter on |
| `BlueprintInput.maxChildDepth` | — | dropped | follows `BlueprintRevision.maxChildDepth`, which v2 does not filter on |
| `BlueprintInput.toolRescan` | — | dropped | follows `BlueprintRevision.toolRescan`, which v2 does not filter on |
| `BlueprintInput.toolGuidance` | — | dropped | follows `Blueprint.toolGuidance`, which went |
| `BlueprintInput.stages` | — | dropped | follows `BlueprintRevision.stages`, which v2 does not filter on |
| `BlueprintInput.regions` | — | dropped | follows `BlueprintRevision.regions`, which v2 does not filter on |
| `BlueprintInput.readPaths` | — | dropped | follows `BlueprintRevision.readPaths`, which v2 does not filter on |
| `BlueprintInput.dependencies` | — | dropped | follows `BlueprintRevision.dependencies`, which v2 does not filter on |
| `BlueprintInput.mimeTypes` | — | dropped | follows `BlueprintRevision.mimeTypeRules`, which v2 does not filter on |
| `BlueprintInput.security` | — | dropped | follows `SettingOverrides.tracksTaint`, which v2 does not filter on |
| `BlueprintInput.sandbox` | — | dropped | follows `SettingOverrides.sandbox`, which v2 does not filter on |
| `BlueprintInput.nudge` | — | dropped | follows `SettingOverrides.nudge`, which v2 does not filter on |
| `BlueprintInput.compaction` | — | dropped | follows `BlueprintRevision.summarizer`, which v2 does not filter on |
| `BlueprintInput.fileTracking` | — | dropped | follows `BlueprintRevision.fileTracking`, which v2 does not filter on |
| `BlueprintInput.repetitionDetection` | — | dropped | follows `BlueprintRevision.repetitionDetection`, which v2 does not filter on |
| `BlueprintInput.safeCommands` | — | dropped | follows `BlueprintRevision.safeCommands`, which v2 does not filter on |
| `BlueprintInput.output` | — | dropped | follows `BlueprintRevision.output`, which v2 does not filter on |
| `BlueprintInput.transforms` | — | dropped | follows `BlueprintRevision.handoffs`, which v2 does not filter on |
| `BlueprintInput.and` | `BlueprintFilter.and` | kept |  |
| `BlueprintInput.or` | `BlueprintFilter.or` | kept |  |
| `BlueprintInput.not` | `BlueprintFilter.not` | kept |  |
| `BlueprintInput.isNull` | `BlueprintRevisionFilter.isNull` | kept | on `BlueprintRevisionFilter` |

### `BlueprintOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlueprintOrder` | `BlueprintOrder` | kept |  |
| `BlueprintOrder.field` | `BlueprintOrder.field` | kept |  |
| `BlueprintOrder.direction` | `BlueprintOrder.direction` | kept |  |

### `BlueprintOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlueprintOrderField` | `BlueprintOrderField` | kept |  |
| `BlueprintOrderField.NAME` | `BlueprintOrderField.NAME` | kept |  |
| `BlueprintOrderField.VERSION` | `BlueprintOrderField.VERSION` | kept |  |

### `BlueprintOutput` → `BlueprintRevision`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlueprintOutput` | `BlueprintRevision` | reshaped | the named blueprint (`Blueprint`) and its content (`BlueprintRevision`, which runs point at) |
| `BlueprintOutput.id` | `Blueprint.id`, `BlueprintRevision.id` | reshaped | `blueprint:<name>` for the blueprint, `blueprint:<name>@<digest12>` for a revision |
| `BlueprintOutput.name` | `Blueprint.name`, `BlueprintRevision.name` | kept | never the invented `"unnamed"` |
| `BlueprintOutput.digest` | `BlueprintRevision.digest` | reshaped | now over the manifest and every file it names |
| `BlueprintOutput.source` | `Run.blueprintSource` | dropped | where a blueprint was read from is a fact about a run, so it lives on the run |
| `BlueprintOutput.version` | `BlueprintRevision.version` | reshaped | null when not written, never `"0.1.0"` |
| `BlueprintOutput.description` | `BlueprintRevision.description` | reshaped | null when not written |
| `BlueprintOutput.entryStage` | `BlueprintRevision.entryStage` | kept |  |
| `BlueprintOutput.entryStageName` | `BlueprintRevision.entryStage` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `BlueprintOutput.maxChildDepth` | `BlueprintRevision.maxChildDepth` | kept |  |
| `BlueprintOutput.toolRescan` | `BlueprintRevision.toolRescan` | kept |  |
| `BlueprintOutput.toolGuidance` | `SettingOverrides.suggestsBatchingCalls`, `SettingOverrides.suggestsShellForMultiStepWork` | merged | on `BlueprintRevision.overrides` |
| `BlueprintOutput.stages` | `BlueprintRevision.stages` | kept |  |
| `BlueprintOutput.regions` | `BlueprintRevision.regions` | kept |  |
| `BlueprintOutput.readPaths` | `BlueprintRevision.readPaths` | reshaped | now `[ReadPathRequest]` |
| `BlueprintOutput.dependencies` | `BlueprintRevision.dependencies` | kept |  |
| `BlueprintOutput.mimeTypes` | `BlueprintRevision.mimeTypeRules` | reshaped | `BlueprintMimeTypeRule`s |
| `BlueprintOutput.security` | `SettingOverrides.tracksTaint` | merged | on `BlueprintRevision.overrides` |
| `BlueprintOutput.sandbox` | `SettingOverrides.sandbox` | reshaped | a `Sandbox`, on `BlueprintRevision.overrides` |
| `BlueprintOutput.nudge` | `SettingOverrides.nudge` | reshaped | a `NudgeSettings`, on `BlueprintRevision.overrides` |
| `BlueprintOutput.compaction` | `BlueprintRevision.summarizer` | reshaped | a `Summarizer` |
| `BlueprintOutput.fileTracking` | `BlueprintRevision.fileTracking` | kept |  |
| `BlueprintOutput.repetitionDetection` | `BlueprintRevision.repetitionDetection` | kept |  |
| `BlueprintOutput.safeCommands` | `BlueprintRevision.safeCommands` | kept |  |
| `BlueprintOutput.output` | `BlueprintRevision.output` | kept |  |
| `BlueprintOutput.transforms` | `BlueprintRevision.handoffs` | reshaped | `ContextHandoff`s |
| `BlueprintOutput.tools` | `Blueprint.tools` | kept | as installed now |
| `BlueprintOutput.tools(filter:)` | `Blueprint.tools(filter:)` | kept | on `Blueprint` |
| `BlueprintOutput.tools(orderBy:)` | `Blueprint.tools(orderBy:)` | kept | on `Blueprint` |
| `BlueprintOutput.tools(first:)` | `Blueprint.tools(first:)` | kept | on `Blueprint` |
| `BlueprintOutput.tools(after:)` | `Blueprint.tools(after:)` | kept | on `Blueprint` |
| `BlueprintOutput.scripts` | `Blueprint.extensions` | reshaped | `Extension`s |
| `BlueprintOutput.scripts(filter:)` | `Blueprint.extensions(filter:)` | reshaped | now `ExtensionFilter` |
| `BlueprintOutput.scripts(orderBy:)` | `Blueprint.extensions(orderBy:)` | reshaped | now `[ExtensionOrder]` |
| `BlueprintOutput.scripts(first:)` | `Blueprint.extensions(first:)` | renamed | on `Blueprint` |
| `BlueprintOutput.scripts(after:)` | `Blueprint.extensions(after:)` | renamed | on `Blueprint` |

### `BlueprintRef`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlueprintRef` | `BlueprintRef` | kept |  |
| `BlueprintRef.name` | `BlueprintRef.name` | kept |  |
| `BlueprintRef.digest` | `BlueprintRef.digest` | kept |  |

### `BlueprintSecurityInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlueprintSecurityInput` | — | dropped | `BlueprintSecurity` went |
| `BlueprintSecurityInput.taintTracking` | — | dropped | follows `BlueprintSecurity` |
| `BlueprintSecurityInput.and` | — | dropped | follows its filter |
| `BlueprintSecurityInput.or` | — | dropped | follows its filter |
| `BlueprintSecurityInput.not` | — | dropped | follows its filter |
| `BlueprintSecurityInput.isNull` | — | dropped | follows its filter |

### `BlueprintSecurityOutput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlueprintSecurityOutput` | — | dropped | a wrapper named "Blueprint" that also sat on `Stage`; `SettingOverrides.tracksTaint` |
| `BlueprintSecurityOutput.taintTracking` | `SettingOverrides.tracksTaint` | merged | a boolean; null inherits |

### `BlueprintSource`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlueprintSource` | `BlueprintSource` | kept |  |
| `BlueprintSource.SNAPSHOT` | `BlueprintSource.SNAPSHOT` | kept |  |
| `BlueprintSource.INSTALLED` | `BlueprintSource.INSTALLED_FALLBACK` | renamed | the honest name, on `Run.blueprintSource`; the listing no longer reports a source |

### `BlueprintSourceFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BlueprintSourceFilter` | `BlueprintSourceFilter` | kept |  |
| `BlueprintSourceFilter.eq` | `BlueprintSourceFilter.eq` | kept |  |
| `BlueprintSourceFilter.ne` | `BlueprintSourceFilter.ne` | kept |  |
| `BlueprintSourceFilter.in` | `BlueprintSourceFilter.in` | kept |  |
| `BlueprintSourceFilter.notIn` | `BlueprintSourceFilter.notIn` | kept |  |
| `BlueprintSourceFilter.isNull` | `BlueprintSourceFilter.isNull` | kept |  |

### `BooleanFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BooleanFilter` | `BooleanFilter` | kept |  |
| `BooleanFilter.eq` | `BooleanFilter.eq` | kept |  |
| `BooleanFilter.ne` | `BooleanFilter.ne` | kept |  |
| `BooleanFilter.isNull` | `BooleanFilter.isNull` | kept |  |

### `BuiltinToolOutput` → `BuiltinTool`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `BuiltinToolOutput` | `BuiltinTool` | reshaped | sub-agent tools split out as `SubagentTool`; aliases folded into `aliases` |
| `BuiltinToolOutput.name` | `BuiltinTool.name` | kept |  |
| `BuiltinToolOutput.description` | `ToolRevision.description` | reshaped | on `Tool.revision` |
| `BuiltinToolOutput.arguments` | `ToolRevision.arguments` | reshaped | on `Tool.revision` |
| `BuiltinToolOutput.origin` | `__typename` | dropped | `BuiltinTool` or `SubagentTool` |

### `CallbackWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CallbackWrite` | `CallbackWrite` | kept |  |
| `CallbackWrite.url` | `CallbackWrite.url` | kept |  |
| `CallbackWrite.secret` | `CallbackWrite.secret` | kept |  |

### `CancelRunRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CancelRunRequest` | `CancelRunRequest` | kept |  |
| `CancelRunRequest.id` | `CancelRunRequest.id` | kept |  |

### `CancelRunResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CancelRunResult` | `CancelRunResult` | kept |  |
| `CancelRunResult.run` | `CancelRunResult.run` | kept |  |
| `CancelRunResult.warnings` | `CancelRunResult.warnings` | kept |  |

### `CancelRunsRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CancelRunsRequest` | `CancelRunsRequest` | kept |  |
| `CancelRunsRequest.filter` | `CancelRunsRequest.filter` | kept |  |

### `CancelRunsResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CancelRunsResult` | `CancelRunsResult` | kept |  |
| `CancelRunsResult.runs` | `CancelRunsResult.outcomes` | dropped | deprecated; each run answers as a `SweepItem` type |
| `CancelRunsResult.skipped` | `CancelRunsResult.outcomes` | dropped | deprecated; `SweepRefused`, `SweepAlreadyInState` and the rest |

### `CaptureStatus` → `CapturedRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CaptureStatus` | `CapturedRequest` | reshaped | each capture state is a type; null is not captured |
| `CaptureStatus.RETAINED` | `RetainedRequest` | reshaped | a type per state |
| `CaptureStatus.NOT_CAPTURED` | `RequestAssembly.body` null | dropped | a type per state |
| `CaptureStatus.REDACTED` | `RedactedRequest` | reshaped | a type per state |
| `CaptureStatus.EXPIRED` | `ExpiredRequest` | reshaped | a type per state |

### `CaptureStatusFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CaptureStatusFilter` | — | dropped | nothing filters on `CapturedRequest` in v2 |
| `CaptureStatusFilter.eq` | — | dropped | follows `CaptureStatus` |
| `CaptureStatusFilter.ne` | — | dropped | follows `CaptureStatus` |
| `CaptureStatusFilter.in` | — | dropped | follows `CaptureStatus` |
| `CaptureStatusFilter.notIn` | — | dropped | follows `CaptureStatus` |
| `CaptureStatusFilter.isNull` | — | dropped | follows its filter |

### `CheckAgentArgsOutput` → `SubAgentArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CheckAgentArgsOutput` | `SubAgentArguments` | merged | `check_agent`, `wait_for_agent` and `kill_agent` take the same arguments |
| `CheckAgentArgsOutput.agentId` | `SubAgentArguments.runId` | renamed | the run id the model wrote, with `run` resolving it |

### `CheckAgentCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CheckAgentCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `CheckAgentCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `CheckAgentCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `CheckAgentCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `CheckAgentCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `SubAgentArguments` for `check_agent` |

### `CheckEndpointRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CheckEndpointRequest` | `CheckEndpointRequest` | kept |  |
| `CheckEndpointRequest.baseUrl` | `CheckEndpointRequest.baseUrl` | kept |  |
| `CheckEndpointRequest.apiKey` | `CheckEndpointRequest.apiKey` | kept |  |
| `CheckEndpointRequest.headers` | `CheckEndpointRequest.headers` | kept |  |

### `CheckEndpointResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CheckEndpointResult` | `CheckEndpointResult` | kept |  |
| `CheckEndpointResult.modelIds` | `CheckEndpointResult.modelNames` | renamed | names, not ids; old name kept one release, deprecated |

### `CheckMachineResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CheckMachineResult` | `CheckMachineResult` | kept |  |
| `CheckMachineResult.report` | `CheckMachineResult.diagnostics` | reshaped | the same `Diagnostics` the offline read answers |

### `CheckMcpServerRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CheckMcpServerRequest` | `CheckMcpServerRequest` | kept |  |
| `CheckMcpServerRequest.name` | `CheckMcpServerRequest.name` | kept |  |

### `CheckMcpServerResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CheckMcpServerResult` | `CheckMcpServerResult` | kept |  |
| `CheckMcpServerResult.mcpServer` | `CheckMcpServerResult.mcpServer` | kept |  |
| `CheckMcpServerResult.toolNames` | `CheckMcpServerResult.mcpServer` (`McpServer.toolListing`) | reshaped | the fresh listing, as `McpTool`s under the names grants use |

### `CheckProviderRequest` → `CheckProviderSignInRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CheckProviderRequest` | `CheckProviderSignInRequest` | renamed | it checks a subscription sign-in |
| `CheckProviderRequest.provider` | `CheckProviderSignInRequest.providerName` | renamed | a name |

### `CheckProviderResult` → `CheckProviderSignInResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CheckProviderResult` | `CheckProviderSignInResult` | reshaped | implements `CheckOutcome`: a failing account is `passed: false`, not `UPSTREAM` |
| `CheckProviderResult.provider` | `CheckProviderSignInResult.provider` | kept |  |
| `CheckProviderResult.models` | `CheckProviderSignInResult.models` | kept |  |
| `CheckProviderResult.unlistedModelIds` | `CheckProviderSignInResult.unlistedModelNames` | renamed | names, not ids; old name kept one release, deprecated |

### `CodexOptionsOutput` → `CodexSettings`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CodexOptionsOutput` | `CodexSettings` | renamed | settings, not options |
| `CodexOptionsOutput.reasoningEffort` | `CodexSettings.reasoningEffort` | kept |  |
| `CodexOptionsOutput.verbosity` | `CodexSettings.verbosity` | kept |  |
| `CodexOptionsOutput.replaysReasoning` | `CodexSettings.replaysReasoning` | kept |  |

### `CodexOptionsWrite` → `CodexSettingsWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CodexOptionsWrite` | `CodexSettingsWrite` | renamed | settings, not options |
| `CodexOptionsWrite.reasoningEffort` | `CodexSettingsWrite.reasoningEffort` | kept |  |
| `CodexOptionsWrite.verbosity` | `CodexSettingsWrite.verbosity` | kept |  |
| `CodexOptionsWrite.replaysReasoning` | `CodexSettingsWrite.replaysReasoning` | kept |  |

### `CodexReasoningEffort`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CodexReasoningEffort` | `CodexReasoningEffort` | kept |  |
| `CodexReasoningEffort.NONE` | `CodexReasoningEffort.NONE` | kept |  |
| `CodexReasoningEffort.MINIMAL` | `CodexReasoningEffort.MINIMAL` | kept |  |
| `CodexReasoningEffort.LOW` | `CodexReasoningEffort.LOW` | kept |  |
| `CodexReasoningEffort.MEDIUM` | `CodexReasoningEffort.MEDIUM` | kept |  |
| `CodexReasoningEffort.HIGH` | `CodexReasoningEffort.HIGH` | kept |  |
| `CodexReasoningEffort.XHIGH` | `CodexReasoningEffort.XHIGH` | kept |  |

### `CodexVerbosity`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CodexVerbosity` | `CodexVerbosity` | kept |  |
| `CodexVerbosity.LOW` | `CodexVerbosity.LOW` | kept |  |
| `CodexVerbosity.MEDIUM` | `CodexVerbosity.MEDIUM` | kept |  |
| `CodexVerbosity.HIGH` | `CodexVerbosity.HIGH` | kept |  |

### `CompactionConfigInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CompactionConfigInput` | — | dropped | nothing filters on `Summarizer` in v2 |
| `CompactionConfigInput.provider` | — | dropped | follows `Summarizer.providerName`, which v2 does not filter on |
| `CompactionConfigInput.model` | — | dropped | follows `Summarizer.modelName`, which v2 does not filter on |
| `CompactionConfigInput.systemPrompt` | — | dropped | follows `Summarizer.systemPrompt`, which v2 does not filter on |
| `CompactionConfigInput.userPromptTemplate` | — | dropped | follows `Summarizer.promptTemplate`, which v2 does not filter on |
| `CompactionConfigInput.maxSummaryTokens` | — | dropped | follows `Summarizer.maxSummaryTokens`, which v2 does not filter on |
| `CompactionConfigInput.temperature` | — | dropped | follows `Summarizer.temperature`, which v2 does not filter on |
| `CompactionConfigInput.and` | — | dropped | follows its filter |
| `CompactionConfigInput.or` | — | dropped | follows its filter |
| `CompactionConfigInput.not` | — | dropped | follows its filter |
| `CompactionConfigInput.isNull` | — | dropped | follows its filter |

### `CompactionConfigOutput` → `Summarizer`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CompactionConfigOutput` | `Summarizer` | renamed | the model that summarizes |
| `CompactionConfigOutput.provider` | `Summarizer.providerName` | renamed | a name; old name kept one release, deprecated |
| `CompactionConfigOutput.model` | `Summarizer.modelName` | renamed | a name; old name kept one release, deprecated |
| `CompactionConfigOutput.systemPrompt` | `Summarizer.systemPrompt` | kept |  |
| `CompactionConfigOutput.userPromptTemplate` | `Summarizer.promptTemplate` | renamed | shorter; old name kept one release, deprecated |
| `CompactionConfigOutput.maxSummaryTokens` | `Summarizer.maxSummaryTokens` | kept |  |
| `CompactionConfigOutput.temperature` | `Summarizer.temperature` | kept |  |

### `ConfigClearable`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ConfigClearable` | — | dropped | `StringSetting` sets or clears a setting in one field |
| `ConfigClearable.OVERRIDE_MODEL` | `ModelRoutingWrite.overrideModel` | renamed | `StringSetting.clear` on the setting itself |
| `ConfigClearable.FALLBACK_MODEL` | `ModelRoutingWrite.fallbackModel` | renamed | `StringSetting.clear` on the setting itself |

### `ConfigErrorKind`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ConfigErrorKind` | — | dropped | the `FileProblem` implementer is the step |
| `ConfigErrorKind.READ` | `FileUnreadable` | reshaped | a type each |
| `ConfigErrorKind.PARSE` | `FileUnparsable` | reshaped | a type each |
| `ConfigErrorKind.VALIDATE` | `SettingRefused` | reshaped | a type each |
| `ConfigErrorKind.UNKNOWN` | — | dropped | unreachable: the fault is this server's own |

### `ConfigErrorOutput` → `FileProblem`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ConfigErrorOutput` | `FileProblem` | reshaped | one type per failing step |
| `ConfigErrorOutput.kind` | `__typename` | reshaped | `FileUnreadable`, `FileUnparsable`, `SettingRefused` |
| `ConfigErrorOutput.path` | `FileProblem.path` | kept |  |
| `ConfigErrorOutput.message` | `FileProblem.message` | kept |  |
| `ConfigErrorOutput.line` | `FileUnparsable.line` | reshaped | parse failures only |
| `ConfigErrorOutput.column` | `FileUnparsable.column` | reshaped | parse failures only |
| `ConfigErrorOutput.key` | `SettingRefused.key` | reshaped | validation failures only |
| `ConfigErrorOutput.since` | `FileProblem.since` | reshaped | null when not known, never 1970 |
| `ConfigErrorOutput.note` | `FileProblem.detail` | dropped | the same constant string on every error |

### `ConfigFileStatusOutput` → `SettingsFile`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ConfigFileStatusOutput` | `SettingsFile` | reshaped | the problem is typed |
| `ConfigFileStatusOutput.path` | `SettingsFile.path` | kept |  |
| `ConfigFileStatusOutput.exists` | `SettingsFile.exists` | kept |  |
| `ConfigFileStatusOutput.error` | `SettingsFile.problem` | reshaped | a `FileProblem` |

### `ConfigHealthChangedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ConfigHealthChangedEvent` | `ConfigHealthChangedEvent` | kept |  |
| `ConfigHealthChangedEvent.seq` | `ConfigHealthChangedEvent.seq` | kept |  |
| `ConfigHealthChangedEvent.at` | `ConfigHealthChangedEvent.at` | kept |  |
| `ConfigHealthChangedEvent.healthy` | `ConfigHealthChangedEvent.config` (`Config.problem`) | reshaped | the frame carries the whole `Config` |
| `ConfigHealthChangedEvent.path` | `Config.path` | merged | on `ConfigHealthChangedEvent.config` |
| `ConfigHealthChangedEvent.error` | `Config.problem` | merged | on `ConfigHealthChangedEvent.config` |
| `ConfigHealthChangedEvent.configMtime` | `Config.savedAt` | merged | on `ConfigHealthChangedEvent.config` |

### `ConfigHealthOutput` → `Config`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ConfigHealthOutput` | `Config` | merged | a wrapper around two fields |
| `ConfigHealthOutput.error` | `Config.problem` | reshaped | a `FileProblem` |
| `ConfigHealthOutput.savedAt` | `Config.savedAt` | merged |  |

### `ConfigOutput` → `Config`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ConfigOutput` | `Config` | reshaped | a `Versioned` node; the server, providers and health moved to their owners |
| `ConfigOutput.routing` | `Config.routing` | reshaped | a `ModelRouting` |
| `ConfigOutput.providers` | `Query.modelProviders` | reshaped | every provider is a `ModelProvider` |
| `ConfigOutput.gateways` | `Query.modelProviders` | reshaped | custom providers are `ModelProvider`s |
| `ConfigOutput.allowsFileUploads` | `Config.allowsFileUploads` | kept |  |
| `ConfigOutput.blueprintPaths` | `Config.blueprintPaths` | kept |  |
| `ConfigOutput.mcpServerCount` | `Query.mcpServers` (`McpServerConnection.total`) | dropped | a count standing for a relation |
| `ConfigOutput.server` | `Query.server` | reshaped | the server is its own root |
| `ConfigOutput.health` | `Config.problem`, `Config.savedAt` | merged | a wrapper around two fields |
| `ConfigOutput.yoloFile` | `Config.approvalPolicyFile` | reshaped | a `SettingsFile` |

### `ConfigWrite` → `ModelRoutingWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ConfigWrite` | `ModelRoutingWrite` | reshaped | routing only; `allowsFileUploads` moved up to the request |
| `ConfigWrite.defaultProvider` | `ModelRoutingWrite.defaultProvider` | kept |  |
| `ConfigWrite.providerOrder` | `ModelRoutingWrite.providerOrder` | kept |  |
| `ConfigWrite.overrideModel` | `ModelRoutingWrite.overrideModel` | reshaped | now `StringSetting` |
| `ConfigWrite.fallbackModel` | `ModelRoutingWrite.fallbackModel` | reshaped | now `StringSetting` |
| `ConfigWrite.allowsFileUploads` | `UpdateConfigRequest.allowsFileUploads` | reshaped | not a routing setting |

### `ContextAppendArgsOutput` → `ContextWriteArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextAppendArgsOutput` | `ContextWriteArguments` | merged | `context_write` and `context_append` take the same arguments; the tool name says which |
| `ContextAppendArgsOutput.region` | `ContextWriteArguments.regionName` | renamed | a region name |
| `ContextAppendArgsOutput.key` | `ContextWriteArguments.key` | kept |  |
| `ContextAppendArgsOutput.content` | `ContextWriteArguments.content` | kept |  |

### `ContextAppendCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextAppendCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `ContextAppendCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `ContextAppendCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `ContextAppendCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `ContextAppendCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `ContextWriteArguments` for `context_append` |

### `ContextAttachArgsOutput` → `ContextAttachArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextAttachArgsOutput` | `ContextAttachArguments` | renamed | argument types are named `XArguments` |
| `ContextAttachArgsOutput.region` | `ContextAttachArguments.regionName` | renamed | a region name |
| `ContextAttachArgsOutput.path` | `ContextAttachArguments.path` | kept |  |
| `ContextAttachArgsOutput.key` | `ContextAttachArguments.key` | kept |  |
| `ContextAttachArgsOutput.caption` | `ContextAttachArguments.caption` | kept |  |
| `ContextAttachArgsOutput.type` | `ContextAttachArguments.mimeType` | renamed | it is a mime type, and `type` reads as a kind |
| `ContextAttachArgsOutput.deliver` | `ContextAttachArguments.delivery` | reshaped | typed as `Delivery` |

### `ContextAttachCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextAttachCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `ContextAttachCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `ContextAttachCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `ContextAttachCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `ContextAttachCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `ContextAttachArguments` for `context_attach` |

### `ContextCause`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextCause` | — | dropped | the `ContextChange` implementer is the cause; `RuntimeContextChange.action` the runtime's own paths |
| `ContextCause.SEED` | `RuntimeAction.SEED` | renamed | the change's type, or `RuntimeContextChange.action` |
| `ContextCause.MESSAGE` | `MessageContextChange` | reshaped | the change's type, or `RuntimeContextChange.action` |
| `ContextCause.MODEL_REPLY` | `ModelContextChange` | reshaped | the change's type, or `RuntimeContextChange.action` |
| `ContextCause.TOOL_RESULT` | `ToolContextChange` | reshaped | the change's type, or `RuntimeContextChange.action` |
| `ContextCause.PRODUCED_PART` | `ModelContextChange` | reshaped | the change's type, or `RuntimeContextChange.action` |
| `ContextCause.COMPACTION` | `RuntimeAction.COMPACTION` | renamed | the change's type, or `RuntimeContextChange.action` |
| `ContextCause.TRANSFORM` | `RuntimeAction.STAGE_TRANSFORM` | renamed | the change's type, or `RuntimeContextChange.action` |
| `ContextCause.CONTEXT_TOOL` | `ToolContextChange` | reshaped | the change's type, or `RuntimeContextChange.action` |
| `ContextCause.HOOK` | `RuntimeAction.HOOK` | renamed | the change's type, or `RuntimeContextChange.action` |
| `ContextCause.FAN_OUT` | `RuntimeAction.FAN_OUT` | renamed | the change's type, or `RuntimeContextChange.action` |
| `ContextCause.INTERACTION` | `InteractionContextChange` | reshaped | the change's type, or `RuntimeContextChange.action` |
| `ContextCause.RESUME` | `RuntimeAction.RESUME` | renamed | the change's type, or `RuntimeContextChange.action` |
| `ContextCause.FRAMEWORK` | `RuntimeAction.BOOKKEEPING` | renamed | the change's type, or `RuntimeContextChange.action` |

### `ContextCauseFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextCauseFilter` | — | dropped | `ContextCause` went |
| `ContextCauseFilter.eq` | — | dropped | follows `ContextCause` |
| `ContextCauseFilter.ne` | — | dropped | follows `ContextCause` |
| `ContextCauseFilter.in` | — | dropped | follows `ContextCause` |
| `ContextCauseFilter.notIn` | — | dropped | follows `ContextCause` |
| `ContextCauseFilter.isNull` | — | dropped | follows its filter |

### `ContextChangeConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextChangeConnection` | `ContextChangeConnection` | kept |  |
| `ContextChangeConnection.results` | `ContextChangeConnection.results` | kept |  |
| `ContextChangeConnection.cursor` | `ContextChangeConnection.cursor` | kept |  |
| `ContextChangeConnection.total` | `ContextChangeConnection.total` | kept |  |

### `ContextChangeInput` → `ContextChangeFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextChangeInput` | `ContextChangeFilter` | kept |  |
| `ContextChangeInput.cause` | — | dropped | follows `ContextChange.cause`, which went |
| `ContextChangeInput.revisionBefore` | — | dropped | follows `ContextChange.before`, which v2 does not filter on |
| `ContextChangeInput.revisionAfter` | — | dropped | follows `ContextChange.after`, which v2 does not filter on |
| `ContextChangeInput.executionId` | `ToolContextChangeFilter.execution` | reshaped | now `ToolExecutionFilter`; follows `ToolContextChange.execution` |
| `ContextChangeInput.regions` | — | dropped | follows `ContextChange.regions`, which v2 does not filter on |
| `ContextChangeInput.journalPosition` | `ContextChangeFilter.journalPosition` | kept |  |
| `ContextChangeInput.at` | `ContextChangeFilter.at` | kept |  |
| `ContextChangeInput.and` | `ContextChangeFilter.and` | kept |  |
| `ContextChangeInput.or` | `ContextChangeFilter.or` | kept |  |
| `ContextChangeInput.not` | `ContextChangeFilter.not` | kept |  |
| `ContextChangeInput.isNull` | — | dropped | follows its filter |

### `ContextChangeListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextChangeListInput` | — | dropped | no v2 listing filters a list of `ContextChange` (now `ContextChange`) |
| `ContextChangeListInput.some` | — | dropped | follows its list filter |
| `ContextChangeListInput.every` | — | dropped | follows its list filter |
| `ContextChangeListInput.none` | — | dropped | follows its list filter |
| `ContextChangeListInput.isNull` | — | dropped | follows its list filter |

### `ContextChangeOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextChangeOrder` | `ContextChangeOrder` | kept |  |
| `ContextChangeOrder.field` | `ContextChangeOrder.field` | kept |  |
| `ContextChangeOrder.direction` | `ContextChangeOrder.direction` | kept |  |

### `ContextChangeOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextChangeOrderField` | `ContextChangeOrderField` | kept |  |
| `ContextChangeOrderField.JOURNAL_POSITION` | `ContextChangeOrderField.JOURNAL_POSITION` | kept |  |

### `ContextChangeOutput` → `ContextChange`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextChangeOutput` | `ContextChange` | reshaped | an interface typed by what made the change |
| `ContextChangeOutput.cause` | `__typename` | reshaped | the `ContextChange` type says what made it; `RuntimeContextChange.action` the runtime's own paths |
| `ContextChangeOutput.revisionBefore` | `ContextChange.before` | reshaped | the `ContextSnapshot` itself |
| `ContextChangeOutput.revisionAfter` | `ContextChange.after` | reshaped | the `ContextSnapshot` itself |
| `ContextChangeOutput.executionId` | `ToolContextChange.execution` | reshaped | only a tool's change has an execution; filter with `toolContextChange: { execution: ... }` |
| `ContextChangeOutput.regions` | `ContextChange.regions` | kept |  |
| `ContextChangeOutput.journalPosition` | `ContextChange.journalPosition` | kept |  |
| `ContextChangeOutput.at` | `ContextChange.at` | kept |  |

### `ContextDeleteArgsOutput` → `ContextDeleteArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextDeleteArgsOutput` | `ContextDeleteArguments` | renamed | argument types are named `XArguments` |
| `ContextDeleteArgsOutput.region` | `ContextDeleteArguments.regionName` | renamed | a region name |
| `ContextDeleteArgsOutput.key` | `ContextDeleteArguments.key` | kept |  |
| `ContextDeleteArgsOutput.index` | `ContextDeleteArguments.index` | kept |  |
| `ContextDeleteArgsOutput.oldest` | `ContextDeleteArguments.oldest` | kept |  |

### `ContextDeleteCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextDeleteCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `ContextDeleteCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `ContextDeleteCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `ContextDeleteCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `ContextDeleteCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `ContextDeleteArguments` for `context_delete` |

### `ContextExportArgsOutput` → `ContextExportArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextExportArgsOutput` | `ContextExportArguments` | renamed | argument types are named `XArguments` |
| `ContextExportArgsOutput.name` | `ContextExportArguments.regionName` | renamed | the region's name |
| `ContextExportArgsOutput.path` | `ContextExportArguments.path` | kept |  |

### `ContextExportCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextExportCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `ContextExportCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `ContextExportCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `ContextExportCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `ContextExportCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `ContextExportArguments` for `context_export` |

### `ContextHistoryOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextHistoryOrder` | — | dropped | the listing it sorted went or became a plain list |
| `ContextHistoryOrder.field` | — | dropped | the listing it sorted went or became a plain list |
| `ContextHistoryOrder.direction` | — | dropped | the listing it sorted went or became a plain list |

### `ContextHistoryOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextHistoryOrderField` | — | dropped | the listing it sorted went or became a plain list |
| `ContextHistoryOrderField.SEQUENCE` | — | dropped | not a sort key in v2 |

### `ContextListArgsOutput` → `ContextListArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextListArgsOutput` | `ContextListArguments` | renamed | argument types are named `XArguments` |
| `ContextListArgsOutput.region` | `ContextListArguments.regionName` | renamed | a region name |

### `ContextListCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextListCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `ContextListCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `ContextListCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `ContextListCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `ContextListCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `ContextListArguments` for `context_list` |

### `ContextReadArgsOutput` → `ContextReadArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextReadArgsOutput` | `ContextReadArguments` | renamed | argument types are named `XArguments` |
| `ContextReadArgsOutput.region` | `ContextReadArguments.regionName` | renamed | a region name |
| `ContextReadArgsOutput.key` | `ContextReadArguments.key` | kept |  |
| `ContextReadArgsOutput.index` | `ContextReadArguments.index` | kept |  |

### `ContextReadCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextReadCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `ContextReadCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `ContextReadCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `ContextReadCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `ContextReadCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `ContextReadArguments` for `context_read` |

### `ContextRegionInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextRegionInput` | — | dropped | nothing filters on `ContextRegion` in v2 |
| `ContextRegionInput.name` | — | dropped | follows `ContextRegion.name`, which v2 does not filter on |
| `ContextRegionInput.kind` | — | dropped | follows `ContextRegion.definition`, which v2 does not filter on |
| `ContextRegionInput.tokens` | — | dropped | follows `ContextRegion.tokens`, which v2 does not filter on |
| `ContextRegionInput.maxTokens` | — | dropped | follows `ContextRegion.maxTokens`, which v2 does not filter on |
| `ContextRegionInput.entryCount` | — | dropped | follows `ContextRegion.entryCount`, which v2 does not filter on |
| `ContextRegionInput.description` | — | dropped | follows `ContextRegion.description`, which went |
| `ContextRegionInput.content` | — | dropped | follows `ContextRegion.text`, which v2 does not filter on |
| `ContextRegionInput.and` | — | dropped | follows its filter |
| `ContextRegionInput.or` | — | dropped | follows its filter |
| `ContextRegionInput.not` | — | dropped | follows its filter |
| `ContextRegionInput.isNull` | — | dropped | follows its filter |

### `ContextRegionListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextRegionListInput` | — | dropped | no v2 listing filters a list of `ContextRegion` (now `ContextRegion`) |
| `ContextRegionListInput.some` | — | dropped | follows its list filter |
| `ContextRegionListInput.every` | — | dropped | follows its list filter |
| `ContextRegionListInput.none` | — | dropped | follows its list filter |
| `ContextRegionListInput.isNull` | — | dropped | follows its list filter |

### `ContextRegionOutput` → `ContextRegion`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextRegionOutput` | `ContextRegion` | reshaped | `definition` links the declared region; entries are typed |
| `ContextRegionOutput.name` | `ContextRegion.name` | kept |  |
| `ContextRegionOutput.kind` | `ContextRegion.definition` | reshaped | the declared `Region`, typed by what it does when it fills |
| `ContextRegionOutput.tokens` | `ContextRegion.tokens` | kept |  |
| `ContextRegionOutput.maxTokens` | `ContextRegion.maxTokens` | kept |  |
| `ContextRegionOutput.entryCount` | `ContextRegion.entryCount` | kept |  |
| `ContextRegionOutput.description` | `ContextRegion.definition` (`DeclaredRegion.description`) | merged | on the declaration |
| `ContextRegionOutput.content` | `ContextRegion.text` | renamed | plus typed `entries`; old name kept one release, deprecated |

### `ContextSnapshotPointConnection` → `ContextSnapshotConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextSnapshotPointConnection` | `ContextSnapshotConnection` | renamed | follows `ContextSnapshot` |
| `ContextSnapshotPointConnection.results` | `ContextSnapshotConnection.results` | renamed |  |
| `ContextSnapshotPointConnection.cursor` | `ContextSnapshotConnection.cursor` | renamed |  |
| `ContextSnapshotPointConnection.total` | `ContextSnapshotConnection.total` | renamed |  |

### `ContextSnapshotPointInput` → `ContextSnapshotFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextSnapshotPointInput` | `ContextSnapshotFilter` | renamed | follows `ContextSnapshot` |
| `ContextSnapshotPointInput.at` | `ContextSnapshotFilter.takenAt` | renamed | follows `ContextSnapshot.takenAt` |
| `ContextSnapshotPointInput.stage` | — | dropped | follows `ContextSnapshot.stage`, which v2 does not filter on |
| `ContextSnapshotPointInput.window` | — | dropped | follows `ContextSnapshot`, which v2 does not filter on |
| `ContextSnapshotPointInput.and` | `ContextSnapshotFilter.and` | reshaped | now `[ContextSnapshotFilter]` |
| `ContextSnapshotPointInput.or` | `ContextSnapshotFilter.or` | reshaped | now `[ContextSnapshotFilter]` |
| `ContextSnapshotPointInput.not` | `ContextSnapshotFilter.not` | reshaped | now `ContextSnapshotFilter` |
| `ContextSnapshotPointInput.isNull` | `ContextSnapshotFilter.isNull` | kept |  |

### `ContextSnapshotPointOutput` → `ContextSnapshot`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextSnapshotPointOutput` | `ContextSnapshot` | merged | one type for a recorded window |
| `ContextSnapshotPointOutput.at` | `ContextSnapshot.takenAt` | renamed | says what the time is; old name kept one release, deprecated |
| `ContextSnapshotPointOutput.stage` | `ContextSnapshot.stage` | reshaped | the window's own record, a `RunStage`; the second, disagreeing reading goes |
| `ContextSnapshotPointOutput.window` | `ContextSnapshot` | merged | the snapshot is the window |

### `ContextTransformInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextTransformInput` | — | dropped | nothing filters on `ContextHandoff` in v2 |
| `ContextTransformInput.fromBlueprint` | — | dropped | follows `ContextHandoff.fromBlueprintName`, which v2 does not filter on |
| `ContextTransformInput.toBlueprint` | — | dropped | follows `ContextHandoff.toBlueprintName`, which v2 does not filter on |
| `ContextTransformInput.mappings` | — | dropped | follows `ContextHandoff.regions`, which v2 does not filter on |
| `ContextTransformInput.and` | — | dropped | follows its filter |
| `ContextTransformInput.or` | — | dropped | follows its filter |
| `ContextTransformInput.not` | — | dropped | follows its filter |
| `ContextTransformInput.isNull` | — | dropped | follows its filter |

### `ContextTransformListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextTransformListInput` | — | dropped | no v2 listing filters a list of `ContextTransform` (now `ContextHandoff`) |
| `ContextTransformListInput.some` | — | dropped | follows its list filter |
| `ContextTransformListInput.every` | — | dropped | follows its list filter |
| `ContextTransformListInput.none` | — | dropped | follows its list filter |
| `ContextTransformListInput.isNull` | — | dropped | follows its list filter |

### `ContextTransformOutput` → `ContextHandoff`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextTransformOutput` | `ContextHandoff` | renamed | "transform" also names the edge concept |
| `ContextTransformOutput.fromBlueprint` | `ContextHandoff.fromBlueprintName` | renamed | a name; old name kept one release, deprecated |
| `ContextTransformOutput.toBlueprint` | `ContextHandoff.toBlueprintName` | renamed | a name; old name kept one release, deprecated |
| `ContextTransformOutput.mappings` | `ContextHandoff.regions` | reshaped | `RegionHandoff`s |

### `ContextUpdatedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextUpdatedEvent` | `ContextUpdatedEvent` | kept |  |
| `ContextUpdatedEvent.seq` | `ContextUpdatedEvent.seq` | kept |  |
| `ContextUpdatedEvent.at` | `ContextUpdatedEvent.at` | kept |  |
| `ContextUpdatedEvent.runId` | `ContextUpdatedEvent.runId` | kept |  |
| `ContextUpdatedEvent.agentId` | `RunEvent.runId` | dropped | the run's live agent id is not a `Run` fact; frames carry `runId` and `run` |
| `ContextUpdatedEvent.totalTokens` | `ContextUpdatedEvent.totalTokens` | reshaped | now `Int` |
| `ContextUpdatedEvent.maxTokens` | `ContextUpdatedEvent.maxTokens` | reshaped | now `Int` |
| `ContextUpdatedEvent.run` | `ContextUpdatedEvent.run` | kept |  |

### `ContextWindowInput` → `ContextSnapshotFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextWindowInput` | `ContextSnapshotFilter` | renamed | follows `ContextSnapshot` |
| `ContextWindowInput.revision` | `ContextSnapshotFilter.digest` | renamed | follows `ContextSnapshot.digest` |
| `ContextWindowInput.totalTokens` | `ContextSnapshotFilter.totalTokens` | renamed | follows `ContextSnapshot.totalTokens` |
| `ContextWindowInput.maxTokens` | `ContextSnapshotFilter.maxTokens` | renamed | follows `ContextSnapshot.maxTokens` |
| `ContextWindowInput.stageName` | — | dropped | follows `ContextSnapshot.stage`, which v2 does not filter on |
| `ContextWindowInput.regions` | — | dropped | follows `ContextSnapshot.regions`, which v2 does not filter on |
| `ContextWindowInput.and` | `ContextSnapshotFilter.and` | reshaped | now `[ContextSnapshotFilter]` |
| `ContextWindowInput.or` | `ContextSnapshotFilter.or` | reshaped | now `[ContextSnapshotFilter]` |
| `ContextWindowInput.not` | `ContextSnapshotFilter.not` | reshaped | now `ContextSnapshotFilter` |
| `ContextWindowInput.isNull` | `ContextSnapshotFilter.isNull` | kept |  |

### `ContextWindowOutput` → `ContextSnapshot`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextWindowOutput` | `ContextSnapshot` | merged | one type for the window now and at each recorded point |
| `ContextWindowOutput.revision` | `ContextSnapshot.digest` | renamed | a content address; `revision` now means a `Revision`; old name kept one release, deprecated |
| `ContextWindowOutput.totalTokens` | `ContextSnapshot.totalTokens` | kept |  |
| `ContextWindowOutput.maxTokens` | `ContextSnapshot.maxTokens` | kept |  |
| `ContextWindowOutput.stageName` | `ContextSnapshot.stage` | reshaped | a relation to the `RunStage` |
| `ContextWindowOutput.regions` | `ContextSnapshot.regions` | kept |  |

### `ContextWriteArgsOutput` → `ContextWriteArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextWriteArgsOutput` | `ContextWriteArguments` | renamed | argument types are named `XArguments` |
| `ContextWriteArgsOutput.region` | `ContextWriteArguments.regionName` | renamed | a region name |
| `ContextWriteArgsOutput.key` | `ContextWriteArguments.key` | kept |  |
| `ContextWriteArgsOutput.content` | `ContextWriteArguments.content` | kept |  |

### `ContextWriteCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ContextWriteCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `ContextWriteCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `ContextWriteCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `ContextWriteCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `ContextWriteCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `ContextWriteArguments` for `context_write` |

### `CostBreakdownInput` → `CostFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CostBreakdownInput` | `CostFilter` | renamed | follows `Cost` |
| `CostBreakdownInput.costUsd` | `CostFilter.totalUsd` | renamed | follows `Cost.totalUsd` |
| `CostBreakdownInput.costPricedUsd` | `CostFilter.pricedUsd` | renamed | follows `Cost.pricedUsd` |
| `CostBreakdownInput.costIsExact` | — | dropped | follows `Cost.providerReported`, which v2 does not filter on |
| `CostBreakdownInput.unpricedCalls` | `CostFilter.unpricedCalls` | renamed | follows `Cost.unpricedCalls` |
| `CostBreakdownInput.and` | — | dropped | follows its filter |
| `CostBreakdownInput.or` | — | dropped | follows its filter |
| `CostBreakdownInput.not` | — | dropped | follows its filter |
| `CostBreakdownInput.isNull` | — | dropped | follows its filter |

### `CostBreakdownOutput` → `Cost`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CostBreakdownOutput` | `Cost` | renamed | the spend for any scope |
| `CostBreakdownOutput.costUsd` | `Cost.totalUsd` | renamed | the whole spend; old name kept one release, deprecated |
| `CostBreakdownOutput.costPricedUsd` | `Cost.pricedUsd` | renamed | the priced subtotal; old name kept one release, deprecated |
| `CostBreakdownOutput.costIsExact` | `Cost.providerReported` | renamed | says what it means: the provider's own figures; old name kept one release, deprecated |
| `CostBreakdownOutput.unpricedCalls` | `Cost.unpricedCalls` | kept |  |

### `CreateBlueprintRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CreateBlueprintRequest` | `CreateBlueprintRequest` | kept |  |
| `CreateBlueprintRequest.name` | `[agent] name` in `CreateBlueprintRequest.manifest` | dropped | the manifest names itself; two names for one blueprint broke `updateBlueprint` |
| `CreateBlueprintRequest.manifest` | `CreateBlueprintRequest.manifest` | kept |  |

### `CreateBlueprintResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CreateBlueprintResult` | `CreateBlueprintResult` | kept |  |
| `CreateBlueprintResult.blueprint` | `CreateBlueprintResult.blueprint` | kept |  |

### `CreateDirectoryRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CreateDirectoryRequest` | `CreateDirectoryRequest` | kept |  |
| `CreateDirectoryRequest.parentPath` | `CreateDirectoryRequest.parentPath` | kept |  |
| `CreateDirectoryRequest.name` | `CreateDirectoryRequest.name` | kept |  |

### `CreateDirectoryResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CreateDirectoryResult` | `CreateDirectoryResult` | kept |  |
| `CreateDirectoryResult.directory` | `CreateDirectoryResult.directory` | kept |  |

### `CreateMcpServerRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CreateMcpServerRequest` | `CreateMcpServerRequest` | kept |  |
| `CreateMcpServerRequest.server` | `CreateMcpServerRequest.server` | kept |  |

### `CreateMcpServerResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CreateMcpServerResult` | `CreateMcpServerResult` | kept |  |
| `CreateMcpServerResult.mcpServer` | `CreateMcpServerResult.mcpServer` | kept |  |

### `CurrentStageInput` → `RunStageFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CurrentStageInput` | `RunStageFilter` | renamed | follows `RunStage` |
| `CurrentStageInput.name` | `RunStageFilter.name` | renamed | follows `RunStage.name` |
| `CurrentStageInput.index` | `RunStageFilter.position` | renamed | follows `RunStage.position` |
| `CurrentStageInput.of` | — | dropped | follows `Run.stages`, which v2 does not filter on |
| `CurrentStageInput.and` | `RunStageFilter.and` | reshaped | now `[RunStageFilter]` |
| `CurrentStageInput.or` | `RunStageFilter.or` | reshaped | now `[RunStageFilter]` |
| `CurrentStageInput.not` | `RunStageFilter.not` | reshaped | now `RunStageFilter` |
| `CurrentStageInput.isNull` | `RunStageFilter.isNull` | kept |  |

### `CurrentStageOutput` → `RunStage`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CurrentStageOutput` | `RunStage` | merged | `Run.currentStage` is the ledger row itself |
| `CurrentStageOutput.name` | `RunStage.name` | merged | `Run.currentStage` is a `RunStage` |
| `CurrentStageOutput.index` | `RunStage.position` | renamed | its position in declared order; old name kept one release, deprecated |
| `CurrentStageOutput.of` | `Run.stages` | dropped | the length of `stages` |

### `CurrentTimeCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `CurrentTimeCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `CurrentTimeCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `CurrentTimeCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `CurrentTimeCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |

### `Cursor`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `Cursor` | `Cursor` | kept |  |

### `DaemonIdentityOutput` → `Daemon`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DaemonIdentityOutput` | `Daemon` | merged | the same facts twice |
| `DaemonIdentityOutput.version` | `Daemon.version` | kept | now nullable |
| `DaemonIdentityOutput.build` | `Daemon.build` | kept | now nullable |
| `DaemonIdentityOutput.pid` | `Daemon.pid` | kept | now nullable |
| `DaemonIdentityOutput.toolEnv` | `Daemon.credentialNames` | renamed | old name kept one release, deprecated |

### `DaemonLinkChangedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DaemonLinkChangedEvent` | `DaemonLinkChangedEvent` | kept |  |
| `DaemonLinkChangedEvent.seq` | `DaemonLinkChangedEvent.seq` | kept |  |
| `DaemonLinkChangedEvent.at` | `DaemonLinkChangedEvent.at` | kept |  |
| `DaemonLinkChangedEvent.connected` | `Daemon.events` (`DaemonLink.state`) | reshaped | on `DaemonLinkChangedEvent.daemon` |
| `DaemonLinkChangedEvent.daemon` | `DaemonLinkChangedEvent.daemon` | kept |  |
| `DaemonLinkChangedEvent.restarted` | `DaemonLinkChangedEvent.restarted` | kept |  |
| `DaemonLinkChangedEvent.restartAdvised` | `Daemon.sameBuildAsServer` | reshaped | on `DaemonLinkChangedEvent.daemon` |

### `DaemonStatusOutput` → `Daemon`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DaemonStatusOutput` | `Daemon` | reshaped | two links, not one boolean |
| `DaemonStatusOutput.reachable` | `Daemon.control`, `Daemon.events` | reshaped | two `DaemonLink`s with a three-state `ConnectionState`; true before anything was tried was a guess |
| `DaemonStatusOutput.version` | `Daemon.version` | kept |  |
| `DaemonStatusOutput.build` | `Daemon.build` | kept |  |
| `DaemonStatusOutput.pid` | `Daemon.pid` | kept |  |
| `DaemonStatusOutput.toolEnv` | `Daemon.credentialNames` | renamed | says what they are; old name kept one release, deprecated |
| `DaemonStatusOutput.restarts` | `Daemon.restarts` | kept |  |
| `DaemonStatusOutput.restartAdvised` | `Daemon.sameBuildAsServer` | reshaped | a boolean, not a sentence |
| `DaemonStatusOutput.journal` | `Daemon.journal` | kept |  |

### `Decimal`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `Decimal` | `Decimal` | kept |  |

### `DecimalFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DecimalFilter` | `DecimalFilter` | kept |  |
| `DecimalFilter.eq` | `DecimalFilter.eq` | kept |  |
| `DecimalFilter.ne` | `DecimalFilter.ne` | kept |  |
| `DecimalFilter.in` | `DecimalFilter.in` | kept |  |
| `DecimalFilter.notIn` | `DecimalFilter.notIn` | kept |  |
| `DecimalFilter.lt` | `DecimalFilter.lt` | kept |  |
| `DecimalFilter.lte` | `DecimalFilter.lte` | kept |  |
| `DecimalFilter.gt` | `DecimalFilter.gt` | kept |  |
| `DecimalFilter.gte` | `DecimalFilter.gte` | kept |  |
| `DecimalFilter.isNull` | `DecimalFilter.isNull` | kept |  |

### `DeleteBlueprintRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DeleteBlueprintRequest` | `DeleteBlueprintRequest` | kept |  |
| `DeleteBlueprintRequest.blueprint` | `DeleteBlueprintRequest.blueprint` | kept |  |

### `DeleteBlueprintResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DeleteBlueprintResult` | `DeleteBlueprintResult` | kept |  |
| `DeleteBlueprintResult.deletedId` | `DeleteBlueprintResult.deletedId` | kept |  |

### `DeleteMcpServerRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DeleteMcpServerRequest` | `DeleteMcpServerRequest` | kept |  |
| `DeleteMcpServerRequest.name` | `DeleteMcpServerRequest.name` | kept |  |

### `DeleteMcpServerResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DeleteMcpServerResult` | `DeleteMcpServerResult` | kept |  |
| `DeleteMcpServerResult.deletedId` | `DeleteMcpServerResult.deletedId` | kept |  |

### `DeleteMimeRowRequest` → `DeleteMimeTypeRuleRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DeleteMimeRowRequest` | `DeleteMimeTypeRuleRequest` | renamed | a rule of the mime registry |
| `DeleteMimeRowRequest.mimeType` | `DeleteMimeTypeRuleRequest.mimeType` | kept |  |

### `DeleteMimeRowResult` → `DeleteMimeTypeRuleResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DeleteMimeRowResult` | `DeleteMimeTypeRuleResult` | renamed | a rule of the mime registry |
| `DeleteMimeRowResult.deletedMimeType` | `DeleteMimeTypeRuleResult.deletedMimeType` | kept |  |

### `DeleteRunsRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DeleteRunsRequest` | `DeleteRunsRequest` | kept |  |
| `DeleteRunsRequest.filter` | `DeleteRunsRequest.filter` | kept |  |
| `DeleteRunsRequest.force` | `DeleteRunsRequest.force` | kept |  |

### `DeleteRunsResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DeleteRunsResult` | `DeleteRunsResult` | kept |  |
| `DeleteRunsResult.deletedIds` | `DeleteRunsResult.deletedIds` | kept |  |
| `DeleteRunsResult.skipped` | `DeleteRunsResult.outcomes` | dropped | deprecated; each run answers as a `SweepItem` type |

### `DeleteScriptRequest` → `DeleteExtensionRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DeleteScriptRequest` | `DeleteExtensionRequest` | renamed | extension, not script |
| `DeleteScriptRequest.script` | `DeleteExtensionRequest.extension` | reshaped | an `ExtensionRef` |

### `DeleteScriptResult` → `DeleteExtensionResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DeleteScriptResult` | `DeleteExtensionResult` | renamed | extension, not script |
| `DeleteScriptResult.deletedId` | `DeleteExtensionResult.deletedId` | kept |  |

### `DeleteYoloProfileRequest` → `DeleteApprovalPolicyRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DeleteYoloProfileRequest` | `DeleteApprovalPolicyRequest` | renamed | plain English |
| `DeleteYoloProfileRequest.name` | `DeleteApprovalPolicyRequest.name` | kept |  |

### `DeleteYoloProfileResult` → `DeleteApprovalPolicyResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DeleteYoloProfileResult` | `DeleteApprovalPolicyResult` | renamed | plain English |
| `DeleteYoloProfileResult.deletedId` | `DeleteApprovalPolicyResult.deletedId` | kept |  |

### `Delivery`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `Delivery` | `Delivery` | kept |  |
| `Delivery.NATIVE` | `Delivery.NATIVE` | kept |  |
| `Delivery.TEXT` | `Delivery.TEXT` | kept |  |
| `Delivery.STAND_IN` | `Delivery.STAND_IN` | kept |  |

### `DenyWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DenyWrite` | `DenyWrite` | kept |  |
| `DenyWrite.feedback` | `DenyWrite.feedback` | kept |  |

### `DependencyInstallInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DependencyInstallInput` | — | dropped | nothing filters on `DependencyInstall` in v2 |
| `DependencyInstallInput.command` | — | dropped | follows `DependencyInstall.anyOs`, which v2 does not filter on |
| `DependencyInstallInput.commands` | — | dropped | follows `DependencyInstall.perOs`, which v2 does not filter on |
| `DependencyInstallInput.script` | — | dropped | follows `DependencyInstall.script`, which v2 does not filter on |
| `DependencyInstallInput.server` | — | dropped | follows `McpServerDependency.template`, which v2 does not filter on |
| `DependencyInstallInput.and` | — | dropped | follows its filter |
| `DependencyInstallInput.or` | — | dropped | follows its filter |
| `DependencyInstallInput.not` | — | dropped | follows its filter |
| `DependencyInstallInput.isNull` | — | dropped | follows its filter |

### `DependencyInstallOutput` → `DependencyInstall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DependencyInstallOutput` | `DependencyInstall` | reshaped | the server template moved to `McpServerDependency` |
| `DependencyInstallOutput.command` | `DependencyInstall.anyOs` | renamed | says when it runs; old name kept one release, deprecated |
| `DependencyInstallOutput.commands` | `DependencyInstall.perOs` | reshaped | `InstallCommand`s with a typed `os` |
| `DependencyInstallOutput.script` | `DependencyInstall.script` | reshaped | the `DependencyInstaller` file, as the revision holds it |
| `DependencyInstallOutput.server` | `McpServerDependency.template` | reshaped | on the MCP dependency |

### `DependencyKind`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DependencyKind` | — | dropped | the `Dependency` implementer is the kind |
| `DependencyKind.MCP_SERVER` | `McpServerDependency` | reshaped | a type each |
| `DependencyKind.ENV` | `EnvironmentVariableDependency` | reshaped | a type each |
| `DependencyKind.BINARY` | `ProgramDependency` | reshaped | a type each |
| `DependencyKind.SCRIPT` | `ScriptedDependency` | reshaped | a type each |

### `DependencyKindFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DependencyKindFilter` | — | dropped | `DependencyKind` went |
| `DependencyKindFilter.eq` | — | dropped | follows `DependencyKind` |
| `DependencyKindFilter.ne` | — | dropped | follows `DependencyKind` |
| `DependencyKindFilter.in` | — | dropped | follows `DependencyKind` |
| `DependencyKindFilter.notIn` | — | dropped | follows `DependencyKind` |
| `DependencyKindFilter.isNull` | — | dropped | follows its filter |

### `DirectiveEntryInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DirectiveEntryInput` | — | dropped | nothing filters on `ReviseAnswer` in v2 |
| `DirectiveEntryInput.option` | — | dropped | follows `ReviseAnswer.label`, which v2 does not filter on |
| `DirectiveEntryInput.instruction` | — | dropped | follows `ReviseAnswer.instruction`, which v2 does not filter on |
| `DirectiveEntryInput.and` | — | dropped | follows its filter |
| `DirectiveEntryInput.or` | — | dropped | follows its filter |
| `DirectiveEntryInput.not` | — | dropped | follows its filter |
| `DirectiveEntryInput.isNull` | — | dropped | follows its filter |

### `DirectiveEntryListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DirectiveEntryListInput` | — | dropped | no v2 listing filters a list of `DirectiveEntry` (now `ReviseAnswer`) |
| `DirectiveEntryListInput.some` | — | dropped | follows its list filter |
| `DirectiveEntryListInput.every` | — | dropped | follows its list filter |
| `DirectiveEntryListInput.none` | — | dropped | follows its list filter |
| `DirectiveEntryListInput.isNull` | — | dropped | follows its list filter |

### `DirectiveEntryOutput` → `ReviseAnswer`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DirectiveEntryOutput` | `ReviseAnswer` | reshaped | an answer that revises |
| `DirectiveEntryOutput.option` | `ReviseAnswer.label` | renamed | the answer's label; old name kept one release, deprecated |
| `DirectiveEntryOutput.instruction` | `ReviseAnswer.instruction` | kept |  |

### `DirectoryOutput` → `Directory`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DirectoryOutput` | `Directory` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `DirectoryOutput.path` | `Directory.path` | kept |  |
| `DirectoryOutput.parent` | `Directory.parent` | reshaped | a `Directory` |
| `DirectoryOutput.home` | `Server.homeDirectory` | reshaped | a machine fact, not a directory's |
| `DirectoryOutput.cwd` | `Server.workingDirectory` | reshaped | a machine fact |
| `DirectoryOutput.entries` | `Directory.subdirectories` | reshaped | `Directory`s, not names |

### `DoctorCheckOutput` → `DiagnosticCheck`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DoctorCheckOutput` | `DiagnosticCheck` | reshaped | a four-state status and the time it took |
| `DoctorCheckOutput.name` | `DiagnosticCheck.name` | kept |  |
| `DoctorCheckOutput.ok` | `DiagnosticCheck.status` | reshaped | a four-state `CheckStatus` |
| `DoctorCheckOutput.detail` | `DiagnosticCheck.detail` | kept |  |

### `DoctorReportOutput` → `Diagnostics`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `DoctorReportOutput` | `Diagnostics` | reshaped | every layer listed; `isLive` goes |
| `DoctorReportOutput.ok` | `Diagnostics.passed` | renamed | old name kept one release, deprecated |
| `DoctorReportOutput.isLive` | `Mutation.checkMachine` | reshaped | a boolean that switched meaning; `diagnostics` is offline, `checkMachine` live, and every layer is listed |
| `DoctorReportOutput.checks` | `Diagnostics.checks` | reshaped | `DiagnosticCheck`s |

### `EditDocumentArgsOutput` → `EditDocumentArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `EditDocumentArgsOutput` | `EditDocumentArguments` | renamed | argument types are named `XArguments` |
| `EditDocumentArgsOutput.content` | `EditDocumentArguments.content` | kept |  |
| `EditDocumentArgsOutput.prompt` | `EditDocumentArguments.prompt` | kept |  |

### `EditDocumentCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `EditDocumentCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `EditDocumentCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `EditDocumentCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `EditDocumentCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `EditDocumentCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `EditDocumentArguments` for `edit_document` |

### `EditFileArgsOutput` → `EditFileArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `EditFileArgsOutput` | `EditFileArguments` | renamed | argument types are named `XArguments` |
| `EditFileArgsOutput.path` | `EditFileArguments.path` | kept |  |
| `EditFileArgsOutput.oldStr` | `EditFileArguments.oldText` | renamed | plain English |
| `EditFileArgsOutput.newStr` | `EditFileArguments.newText` | renamed | plain English |

### `EditFileCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `EditFileCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `EditFileCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `EditFileCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `EditFileCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `EditFileCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `EditFileArguments` for `edit_file` |

### `EffectiveNudgeOutput` → `EffectiveNudge`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `EffectiveNudgeOutput` | `EffectiveNudge` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `EffectiveNudgeOutput.nudges` | `EffectiveNudge.nudges` | kept |  |
| `EffectiveNudgeOutput.max` | `EffectiveNudge.max` | kept |  |
| `EffectiveNudgeOutput.text` | `EffectiveNudge.text` | kept |  |

### `EffectiveStageSettingsOutput` → `EffectiveStageSettings`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `EffectiveStageSettingsOutput` | `EffectiveStageSettings` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `EffectiveStageSettingsOutput.includesBatchHint` | `EffectiveStageSettings.includesBatchHint` | kept |  |
| `EffectiveStageSettingsOutput.shellHintEligible` | `EffectiveStageSettings.shellHintEligible` | kept |  |
| `EffectiveStageSettingsOutput.nudge` | `EffectiveStageSettings.nudge` | kept |  |
| `EffectiveStageSettingsOutput.sandbox` | `EffectiveStageSettings.sandbox` | kept |  |
| `EffectiveStageSettingsOutput.tracksTaint` | `EffectiveStageSettings.tracksTaint` | kept |  |

### `EnvEntryInput` → `KeyValueFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `EnvEntryInput` | `KeyValueFilter` | renamed | follows `KeyValue` |
| `EnvEntryInput.name` | `KeyValueFilter.key` | renamed | follows `KeyValue.key` |
| `EnvEntryInput.value` | `KeyValueFilter.value` | renamed | follows `KeyValue.value` |
| `EnvEntryInput.and` | `KeyValueFilter.and` | reshaped | now `[KeyValueFilter]` |
| `EnvEntryInput.or` | `KeyValueFilter.or` | reshaped | now `[KeyValueFilter]` |
| `EnvEntryInput.not` | `KeyValueFilter.not` | reshaped | now `KeyValueFilter` |
| `EnvEntryInput.isNull` | `KeyValueFilter.isNull` | kept |  |

### `EnvEntryListInput` → `KeyValueListFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `EnvEntryListInput` | `KeyValueListFilter` | renamed | follows `KeyValue` |
| `EnvEntryListInput.some` | `KeyValueListFilter.some` | reshaped | now `KeyValueFilter` |
| `EnvEntryListInput.every` | `KeyValueListFilter.every` | reshaped | now `KeyValueFilter` |
| `EnvEntryListInput.none` | `KeyValueListFilter.none` | reshaped | now `KeyValueFilter` |
| `EnvEntryListInput.isNull` | `KeyValueListFilter.isNull` | kept |  |

### `EnvEntryOutput` → `KeyValue`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `EnvEntryOutput` | `KeyValue` | merged | one name-and-value type |
| `EnvEntryOutput.name` | `KeyValue.key` | renamed | one name-and-value type; old name kept one release, deprecated |
| `EnvEntryOutput.value` | `KeyValue.value` | kept |  |

### `EnvironmentInfoCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `EnvironmentInfoCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `EnvironmentInfoCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `EnvironmentInfoCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `EnvironmentInfoCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |

### `Event`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `Event` | `Event` | kept |  |
| `Event.seq` | `Event.seq` | kept |  |
| `Event.at` | `Event.at` | kept |  |

### `EventsDroppedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `EventsDroppedEvent` | `EventsDroppedEvent` | kept |  |
| `EventsDroppedEvent.count` | `EventsDroppedEvent.count` | kept |  |
| `EventsDroppedEvent.seq` | `EventsDroppedEvent.seq` | kept |  |
| `EventsDroppedEvent.at` | `EventsDroppedEvent.at` | kept |  |

### `ExportStatus` → `JobState`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ExportStatus` | `JobState` | reshaped | one `JobState` union for every job |
| `ExportStatus.QUEUED` | `JobQueued` | reshaped | a `JobState` type each |
| `ExportStatus.RUNNING` | `JobRunning` | reshaped | a `JobState` type each |
| `ExportStatus.COMPLETE` | `JobComplete` | reshaped | a `JobState` type each |
| `ExportStatus.FAILED` | `JobFailed` | reshaped | a `JobState` type each |

### `FanOutArgsOutput` → `FanOutArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FanOutArgsOutput` | `FanOutArguments` | renamed | argument types are named `XArguments` |
| `FanOutArgsOutput.agent` | `FanOutArguments.blueprintName` | renamed | a blueprint name |
| `FanOutArgsOutput.items` | `FanOutArguments.workItems` | renamed | `items` is a paging word |
| `FanOutArgsOutput.maxWorkers` | `FanOutArguments.maxWorkers` | kept |  |

### `FanOutCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FanOutCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `FanOutCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `FanOutCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `FanOutCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `FanOutCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `FanOutArguments` for `fan_out` |

### `FanOutInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FanOutInput` | — | dropped | nothing filters on `FanOutStage` in v2 |
| `FanOutInput.workerAgent` | — | dropped | follows `WorkerBlueprint.blueprintName`, which v2 does not filter on |
| `FanOutInput.workerStage` | — | dropped | follows `WorkerStage.stage`, which v2 does not filter on |
| `FanOutInput.workerStageName` | — | dropped | follows `WorkerStage.stage`, which v2 does not filter on |
| `FanOutInput.workerQuery` | — | dropped | follows `WorkerSearch.query`, which v2 does not filter on |
| `FanOutInput.mergeStage` | — | dropped | follows `FanOutStage.mergeStage`, which v2 does not filter on |
| `FanOutInput.mergeStageName` | — | dropped | follows `FanOutStage.mergeStage`, which v2 does not filter on |
| `FanOutInput.splitPrompt` | — | dropped | follows `FanOutStage.splitPrompt`, which v2 does not filter on |
| `FanOutInput.maxWorkers` | — | dropped | follows `FanOutStage.maxWorkers`, which v2 does not filter on |
| `FanOutInput.maxItems` | — | dropped | follows `FanOutStage.maxItems`, which v2 does not filter on |
| `FanOutInput.maxAttempts` | — | dropped | follows `FanOutStage.maxAttempts`, which v2 does not filter on |
| `FanOutInput.onWorkerFailure` | — | dropped | follows `FanOutStage.onWorkerFailure`, which v2 does not filter on |
| `FanOutInput.resultsRegion` | — | dropped | follows `FanOutStage.resultsRegion`, which v2 does not filter on |
| `FanOutInput.resultsRegionName` | — | dropped | follows `FanOutStage.resultsRegion`, which v2 does not filter on |
| `FanOutInput.and` | — | dropped | follows its filter |
| `FanOutInput.or` | — | dropped | follows its filter |
| `FanOutInput.not` | — | dropped | follows its filter |
| `FanOutInput.isNull` | — | dropped | follows its filter |

### `FanOutItemOutput` → `FanOutItem`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FanOutItemOutput` | `FanOutItem` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `FanOutItemOutput.id` | `FanOutItem.id` | kept |  |
| `FanOutItemOutput.context` | `FanOutItem.context` | kept |  |

### `FanOutOutput` → `FanOutStage`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FanOutOutput` | `FanOutStage` | merged | into `FanOutStage`; the three worker fields become the `FanOutWorker` union |
| `FanOutOutput.workerAgent` | `WorkerBlueprint.blueprintName` | reshaped | one member of the `FanOutWorker` union |
| `FanOutOutput.workerStage` | `WorkerStage.stage` | reshaped | one member of the `FanOutWorker` union |
| `FanOutOutput.workerStageName` | `WorkerStage.stage` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `FanOutOutput.workerQuery` | `WorkerSearch.query` | reshaped | one member of the `FanOutWorker` union |
| `FanOutOutput.mergeStage` | `FanOutStage.mergeStage` | merged |  |
| `FanOutOutput.mergeStageName` | `FanOutStage.mergeStage` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `FanOutOutput.splitPrompt` | `FanOutStage.splitPrompt` | merged |  |
| `FanOutOutput.maxWorkers` | `FanOutStage.maxWorkers` | merged |  |
| `FanOutOutput.maxItems` | `FanOutStage.maxItems` | merged |  |
| `FanOutOutput.maxAttempts` | `FanOutStage.maxAttempts` | merged |  |
| `FanOutOutput.onWorkerFailure` | `FanOutStage.onWorkerFailure` | merged |  |
| `FanOutOutput.resultsRegion` | `FanOutStage.resultsRegion` | reshaped | never null |
| `FanOutOutput.resultsRegionName` | `FanOutStage.resultsRegion` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |

### `FileEntryConnection` → `RunDirectoryEntryConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FileEntryConnection` | `RunDirectoryEntryConnection` | renamed | follows `RunDirectoryEntry` |
| `FileEntryConnection.results` | `RunDirectoryEntryConnection.results` | renamed |  |
| `FileEntryConnection.cursor` | `RunDirectoryEntryConnection.cursor` | renamed |  |
| `FileEntryConnection.total` | `RunDirectoryEntryConnection.total` | renamed |  |
| `FileEntryConnection.path` | — | dropped | follows its connection |
| `FileEntryConnection.parent` | — | dropped | follows its connection |
| `FileEntryConnection.workdir` | — | dropped | follows its connection |
| `FileEntryConnection.isTruncated` | — | dropped | follows its connection |
| `FileEntryConnection.isModifiedFilesTruncated` | — | dropped | follows its connection |
| `FileEntryConnection.modifyingToolCallCount` | — | dropped | follows its connection |

### `FileEntryInput` → `RunDirectoryEntryFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FileEntryInput` | `RunDirectoryEntryFilter` | renamed | follows `RunDirectoryEntry` |
| `FileEntryInput.name` | `RunDirectoryEntryFilter.name` | renamed | follows `RunDirectoryEntry.name` |
| `FileEntryInput.path` | `RunDirectoryEntryFilter.path` | renamed | follows `RunDirectoryEntry.path` |
| `FileEntryInput.isDir` | — | dropped | follows `FileEntry.isDir`, which went |
| `FileEntryInput.size` | `RunFileFilter.size` | renamed | follows `RunFile.size` |
| `FileEntryInput.exists` | — | dropped | follows `ChangedFile.file`, which v2 does not filter on |
| `FileEntryInput.isOutsideWorkdir` | — | dropped | follows `ChangedFile.outsideWorkingDirectory`, which v2 does not filter on |
| `FileEntryInput.mimeType` | `RunFileFilter.mimeType` | renamed | follows `RunFile.mimeType` |
| `FileEntryInput.and` | `RunDirectoryEntryFilter.and` | reshaped | now `[RunDirectoryEntryFilter]` |
| `FileEntryInput.or` | `RunDirectoryEntryFilter.or` | reshaped | now `[RunDirectoryEntryFilter]` |
| `FileEntryInput.not` | `RunDirectoryEntryFilter.not` | reshaped | now `RunDirectoryEntryFilter` |
| `FileEntryInput.isNull` | `RunDirectoryEntryFilter.isNull` | kept |  |

### `FileEntryOutput` → `RunDirectoryEntry`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FileEntryOutput` | `RunDirectoryEntry` | reshaped | split into `RunFile` and `RunDirectory`, and `ChangedFile` for the recorded list |
| `FileEntryOutput.name` | `RunDirectoryEntry.name` | kept |  |
| `FileEntryOutput.path` | `RunDirectoryEntry.path` | kept |  |
| `FileEntryOutput.isDir` | `__typename` | reshaped | `RunFile` or `RunDirectory`: a boolean that switched meaning |
| `FileEntryOutput.size` | `RunFile.size` | kept | on `RunFile` |
| `FileEntryOutput.exists` | `ChangedFile.file` | reshaped | null once deleted |
| `FileEntryOutput.isOutsideWorkdir` | `ChangedFile.outsideWorkingDirectory` | renamed | plain English |
| `FileEntryOutput.mimeType` | `RunFile.mimeType` | kept | now nullable |

### `FileSource`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FileSource` | — | dropped | an argument that switched between two questions: `Run.changedFiles` and `Run.workingDirectory` |
| `FileSource.MODIFIED` | `Run.changedFiles` | renamed | two questions, two fields |
| `FileSource.WORKDIR` | `Run.workingDirectory` | renamed | two questions, two fields |

### `FileTrackingConfigInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FileTrackingConfigInput` | — | dropped | nothing filters on `FileTracking` in v2 |
| `FileTrackingConfigInput.region` | — | dropped | follows `FileTracking.region`, which v2 does not filter on |
| `FileTrackingConfigInput.regionName` | — | dropped | follows `FileTracking.region`, which v2 does not filter on |
| `FileTrackingConfigInput.trackReads` | — | dropped | follows `FileTracking.tracksReads`, which v2 does not filter on |
| `FileTrackingConfigInput.trackWrites` | — | dropped | follows `FileTracking.tracksWrites`, which v2 does not filter on |
| `FileTrackingConfigInput.maxFileTokens` | — | dropped | follows `FileTracking.maxFileTokens`, which v2 does not filter on |
| `FileTrackingConfigInput.and` | — | dropped | follows its filter |
| `FileTrackingConfigInput.or` | — | dropped | follows its filter |
| `FileTrackingConfigInput.not` | — | dropped | follows its filter |
| `FileTrackingConfigInput.isNull` | — | dropped | follows its filter |

### `FileTrackingConfigOutput` → `FileTracking`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FileTrackingConfigOutput` | `FileTracking` | renamed | no storage words |
| `FileTrackingConfigOutput.region` | `FileTracking.region` | reshaped | never null |
| `FileTrackingConfigOutput.regionName` | `FileTracking.region` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `FileTrackingConfigOutput.trackReads` | `FileTracking.tracksReads` | renamed | a boolean reads as a verb; old name kept one release, deprecated |
| `FileTrackingConfigOutput.trackWrites` | `FileTracking.tracksWrites` | renamed | a boolean reads as a verb; old name kept one release, deprecated |
| `FileTrackingConfigOutput.maxFileTokens` | `FileTracking.maxFileTokens` | kept |  |

### `FileWindowOutput` → `FileWindow`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FileWindowOutput` | `FileWindow` | reshaped | `truncated` restated `nextOffset` |
| `FileWindowOutput.path` | `FileWindow.path` | kept |  |
| `FileWindowOutput.size` | `FileWindow.size` | kept |  |
| `FileWindowOutput.offset` | `FileWindow.offset` | kept |  |
| `FileWindowOutput.nextOffset` | `FileWindow.nextOffset` | kept |  |
| `FileWindowOutput.content` | `FileWindow.text` | renamed | it is text; old name kept one release, deprecated |
| `FileWindowOutput.truncated` | `FileWindow.nextOffset` | dropped | restated `nextOffset` being set |

### `FinalOutputInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FinalOutputInput` | — | dropped | nothing filters on `Answer` in v2 |
| `FinalOutputInput.content` | — | dropped | follows `Answer.text`, which v2 does not filter on |
| `FinalOutputInput.format` | — | dropped | follows `Answer.format`, which v2 does not filter on |
| `FinalOutputInput.stage` | — | dropped | follows `Answer.stage`, which v2 does not filter on |
| `FinalOutputInput.submittedAt` | — | dropped | follows `Answer.submittedAt`, which v2 does not filter on |
| `FinalOutputInput.truncated` | — | dropped | follows `Answer.truncated`, which v2 does not filter on |
| `FinalOutputInput.and` | — | dropped | follows its filter |
| `FinalOutputInput.or` | — | dropped | follows its filter |
| `FinalOutputInput.not` | — | dropped | follows its filter |
| `FinalOutputInput.isNull` | — | dropped | follows its filter |

### `FinalOutputOutput` → `Answer`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FinalOutputOutput` | `Answer` | reshaped | the answer a run submitted; it owns its artifacts |
| `FinalOutputOutput.content` | `Answer.text` | renamed | it is text; old name kept one release, deprecated |
| `FinalOutputOutput.format` | `Answer.format` | kept |  |
| `FinalOutputOutput.stage` | `Answer.stage` | reshaped | a relation to the `RunStage` |
| `FinalOutputOutput.submittedAt` | `Answer.submittedAt` | kept |  |
| `FinalOutputOutput.truncated` | `Answer.truncated` | kept |  |

### `FixedInput` → `TokenCountFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FixedInput` | `TokenCountFilter` | renamed | follows `TokenCount` |
| `FixedInput.tokens` | `TokenCountFilter.tokens` | renamed | follows `TokenCount.tokens` |
| `FixedInput.and` | `TokenCountFilter.and` | reshaped | now `[TokenCountFilter]` |
| `FixedInput.or` | `TokenCountFilter.or` | reshaped | now `[TokenCountFilter]` |
| `FixedInput.not` | `TokenCountFilter.not` | reshaped | now `TokenCountFilter` |
| `FixedInput.isNull` | `TokenCountFilter.isNull` | kept |  |

### `FixedOutput` → `TokenCount`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FixedOutput` | `TokenCount` | merged | a number of tokens: the same type region budgets and reply limits use |
| `FixedOutput.tokens` | `TokenCount.tokens` | kept |  |

### `FloatFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `FloatFilter` | `FloatFilter` | kept |  |
| `FloatFilter.eq` | `FloatFilter.eq` | kept |  |
| `FloatFilter.ne` | `FloatFilter.ne` | kept |  |
| `FloatFilter.in` | `FloatFilter.in` | kept |  |
| `FloatFilter.notIn` | `FloatFilter.notIn` | kept |  |
| `FloatFilter.lt` | `FloatFilter.lt` | kept |  |
| `FloatFilter.lte` | `FloatFilter.lte` | kept |  |
| `FloatFilter.gt` | `FloatFilter.gt` | kept |  |
| `FloatFilter.gte` | `FloatFilter.gte` | kept |  |
| `FloatFilter.isNull` | `FloatFilter.isNull` | kept |  |

### `GatewayKind`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `GatewayKind` | — | dropped | the `ModelProvider` implementer is the kind |
| `GatewayKind.SCRIPT` | `ScriptProvider` | reshaped | a type each |
| `GatewayKind.OPENAI_COMPATIBLE` | `OpenAiCompatibleProvider` | reshaped | a type each |
| `GatewayKind.OPENAI` | `NamedOpenAiProvider` | reshaped | a type each |

### `GatewayOutput` → `ModelProvider`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `GatewayOutput` | `ModelProvider` | reshaped | a custom provider is a `ModelProvider`: `OpenAiCompatibleProvider`, `NamedOpenAiProvider` or `ScriptProvider` |
| `GatewayOutput.name` | `ModelProvider.name` | kept |  |
| `GatewayOutput.kind` | `__typename` | dropped | `OpenAiCompatibleProvider`, `NamedOpenAiProvider` or `ScriptProvider` |
| `GatewayOutput.baseUrl` | `OpenAiCompatibleProvider.baseUrl`, `NamedOpenAiProvider.baseUrl`, `ScriptProvider.baseUrl` | reshaped | on each type |
| `GatewayOutput.script` | `ScriptProvider.extension` | reshaped | the `ProviderExtension` it runs on |
| `GatewayOutput.hasApiKey` | `OpenAiCompatibleProvider.hasApiKey`, `NamedOpenAiProvider.hasApiKey`, `ScriptProvider.hasApiKey` | reshaped | on each type |
| `GatewayOutput.headerNames` | `OpenAiCompatibleProvider.headerNames`, `NamedOpenAiProvider.headerNames` | reshaped | endpoints only |
| `GatewayOutput.models` | `OpenAiCompatibleProvider.fallbackModelNames`, `NamedOpenAiProvider.servedModelNames`, `ScriptProvider.servedModelNames` | reshaped | says what the list is for |
| `GatewayOutput.unknownKeys` | `ScriptProvider.extraSettingNames` | renamed | names of the settings handed to the script |

### `GatewayWrite` → `ModelProviderWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `GatewayWrite` | `ModelProviderWrite` | reshaped | one `@oneOf` field per kind of provider |
| `GatewayWrite.name` | `OpenAiCompatibleProviderWrite.name`, `NamedOpenAiProviderWrite.name`, `ScriptProviderWrite.name` | reshaped | on each write |
| `GatewayWrite.kind` | `ModelProviderWrite` | reshaped | the `@oneOf` field chosen: `openAiCompatible`, `namedOpenAi` or `script` |
| `GatewayWrite.baseUrl` | `OpenAiCompatibleProviderWrite.baseUrl`, `NamedOpenAiProviderWrite.baseUrl` | reshaped | on each write |
| `GatewayWrite.script` | `ScriptProviderWrite.extensionPath` | reshaped | the provider extension it runs on |
| `GatewayWrite.apiKey` | `OpenAiCompatibleProviderWrite.apiKey` | reshaped | a `StringSetting` |
| `GatewayWrite.headers` | `OpenAiCompatibleProviderWrite.headers`, `NamedOpenAiProviderWrite.headers` | reshaped | endpoints only |
| `GatewayWrite.models` | `OpenAiCompatibleProviderWrite.fallbackModelNames`, `ScriptProviderWrite.servedModelNames` | reshaped | says what the list is for |

### `HintSetting`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `HintSetting` | — | dropped | null inherits, as everywhere else |
| `HintSetting.INHERIT` | `SettingOverrides.suggestsBatchingCalls` null | dropped | a boolean; null inherits |
| `HintSetting.INCLUDE` | `SettingOverrides.suggestsBatchingCalls` true | dropped | a boolean; null inherits |
| `HintSetting.OMIT` | `SettingOverrides.suggestsBatchingCalls` false | dropped | a boolean; null inherits |

### `HintSettingFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `HintSettingFilter` | — | dropped | `HintSetting` went |
| `HintSettingFilter.eq` | — | dropped | follows `HintSetting` |
| `HintSettingFilter.ne` | — | dropped | follows `HintSetting` |
| `HintSettingFilter.in` | — | dropped | follows `HintSetting` |
| `HintSettingFilter.notIn` | — | dropped | follows `HintSetting` |
| `HintSettingFilter.isNull` | — | dropped | follows its filter |

### `IDFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `IDFilter` | `IDFilter` | kept |  |
| `IDFilter.eq` | `IDFilter.eq` | kept |  |
| `IDFilter.ne` | `IDFilter.ne` | kept |  |
| `IDFilter.in` | `IDFilter.in` | kept |  |
| `IDFilter.notIn` | `IDFilter.notIn` | kept |  |
| `IDFilter.isNull` | `IDFilter.isNull` | kept |  |

### `InferenceAttemptConnection` → `ModelAttemptConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InferenceAttemptConnection` | `ModelAttemptConnection` | renamed | follows `ModelAttempt` |
| `InferenceAttemptConnection.results` | `ModelAttemptConnection.results` | renamed |  |
| `InferenceAttemptConnection.cursor` | `ModelAttemptConnection.cursor` | renamed |  |
| `InferenceAttemptConnection.total` | `ModelAttemptConnection.total` | renamed |  |

### `InferenceAttemptInput` → `ModelAttemptFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InferenceAttemptInput` | `ModelAttemptFilter` | renamed | follows `ModelAttempt` |
| `InferenceAttemptInput.stage` | `ModelAttemptFilter.stage` | reshaped | now `RunStageFilter`; follows `ModelAttempt.stage` |
| `InferenceAttemptInput.attempt` | — | dropped | follows `ModelAttempt.number`, which v2 does not filter on |
| `InferenceAttemptInput.provider` | `ModelAttemptFilter.providerName` | renamed | follows `ModelAttempt.providerName` |
| `InferenceAttemptInput.model` | `ModelAttemptFilter.modelName` | renamed | follows `ModelAttempt.modelName` |
| `InferenceAttemptInput.outcome` | — | dropped | follows `InferenceAttempt.outcome`, which went |
| `InferenceAttemptInput.durationMs` | `ModelAttemptFilter.durationMs` | renamed | follows `ModelAttempt.durationMs` |
| `InferenceAttemptInput.backoffMs` | — | dropped | follows `ModelAttempt.backoffMs`, which v2 does not filter on |
| `InferenceAttemptInput.digest` | — | dropped | follows `ModelAttempt.sent`, which v2 does not filter on |
| `InferenceAttemptInput.modelInput` | — | dropped | follows `ModelAttempt.assembly`, which v2 does not filter on |
| `InferenceAttemptInput.at` | `ModelAttemptFilter.at` | renamed | follows `ModelAttempt.at` |
| `InferenceAttemptInput.failover` | — | dropped | follows `ModelAttempt.failover`, which v2 does not filter on |
| `InferenceAttemptInput.and` | `ModelAttemptFilter.and` | reshaped | now `[ModelAttemptFilter]` |
| `InferenceAttemptInput.or` | `ModelAttemptFilter.or` | reshaped | now `[ModelAttemptFilter]` |
| `InferenceAttemptInput.not` | `ModelAttemptFilter.not` | reshaped | now `ModelAttemptFilter` |
| `InferenceAttemptInput.isNull` | — | dropped | follows its filter |

### `InferenceAttemptOrder` → `ModelAttemptOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InferenceAttemptOrder` | `ModelAttemptOrder` | renamed | follows `ModelAttempt` |
| `InferenceAttemptOrder.field` | `ModelAttemptOrder.field` | reshaped | now `ModelAttemptOrderField` |
| `InferenceAttemptOrder.direction` | `ModelAttemptOrder.direction` | renamed |  |

### `InferenceAttemptOrderField` → `ModelAttemptOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InferenceAttemptOrderField` | `ModelAttemptOrderField` | renamed | follows `ModelAttempt` |
| `InferenceAttemptOrderField.SEQUENCE` | — | dropped | not a sort key in v2 |

### `InferenceAttemptOutput` → `ModelAttempt`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InferenceAttemptOutput` | `ModelAttempt` | reshaped | one type per outcome: `AnsweredAttempt`, `FailedAttempt` |
| `InferenceAttemptOutput.stage` | `ModelAttempt.stage` | reshaped | a relation to the `RunStage`; null for a run-level lane |
| `InferenceAttemptOutput.attempt` | `ModelAttempt.number` | renamed | which trip, from 1; old name kept one release, deprecated |
| `InferenceAttemptOutput.provider` | `ModelAttempt.providerName` | renamed | the name as configured; `ModelAttempt.provider` is the provider revision it used |
| `InferenceAttemptOutput.model` | `ModelAttempt.modelName` | renamed | the name as configured; `ModelAttempt.model` is the catalogue entry |
| `InferenceAttemptOutput.outcome` | `__typename` | reshaped | the attempt type: `AnsweredAttempt` or `FailedAttempt`, each with its own fields |
| `InferenceAttemptOutput.durationMs` | `ModelAttempt.durationMs` | kept |  |
| `InferenceAttemptOutput.backoffMs` | `ModelAttempt.backoffMs` | kept |  |
| `InferenceAttemptOutput.digest` | `ModelAttempt.sent` | reshaped | a `RequestSummary` |
| `InferenceAttemptOutput.modelInput` | `ModelAttempt.assembly` | reshaped | a `RequestAssembly` |
| `InferenceAttemptOutput.at` | `ModelAttempt.at` | kept |  |
| `InferenceAttemptOutput.failover` | `ModelAttempt.failover` | kept |  |

### `InferenceFailoverInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InferenceFailoverInput` | — | dropped | nothing filters on `Failover` in v2 |
| `InferenceFailoverInput.stage` | `ModelAttemptFilter.stage` | reshaped | now `RunStageFilter`; follows `ModelAttempt.stage` |
| `InferenceFailoverInput.iteration` | — | dropped | follows `ModelAttempt.turn`, which v2 does not filter on |
| `InferenceFailoverInput.fromProvider` | — | dropped | follows `Failover.fromProviderName`, which v2 does not filter on |
| `InferenceFailoverInput.fromModel` | — | dropped | follows `Failover.fromModelName`, which v2 does not filter on |
| `InferenceFailoverInput.toProvider` | — | dropped | follows `Failover.toProviderName`, which v2 does not filter on |
| `InferenceFailoverInput.toModel` | — | dropped | follows `Failover.toModelName`, which v2 does not filter on |
| `InferenceFailoverInput.reason` | — | dropped | follows `Failover.why`, which v2 does not filter on |
| `InferenceFailoverInput.failureKind` | — | dropped | follows `Failover.failure`, which v2 does not filter on |
| `InferenceFailoverInput.at` | — | dropped | follows `Failover.at`, which v2 does not filter on |
| `InferenceFailoverInput.and` | — | dropped | follows its filter |
| `InferenceFailoverInput.or` | — | dropped | follows its filter |
| `InferenceFailoverInput.not` | — | dropped | follows its filter |
| `InferenceFailoverInput.isNull` | — | dropped | follows its filter |

### `InferenceFailoverOutput` → `Failover`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InferenceFailoverOutput` | `Failover` | renamed | the attempt it follows carries the stage and turn |
| `InferenceFailoverOutput.stage` | `ModelAttempt.stage` | merged | the attempt it follows has the stage |
| `InferenceFailoverOutput.iteration` | `ModelAttempt.turn` | merged | the attempt it follows has the turn |
| `InferenceFailoverOutput.fromProvider` | `Failover.fromProviderName` | renamed | a name, as configured; old name kept one release, deprecated |
| `InferenceFailoverOutput.fromModel` | `Failover.fromModelName` | renamed | a name, as configured; old name kept one release, deprecated |
| `InferenceFailoverOutput.toProvider` | `Failover.toProviderName` | renamed | a name, as configured; old name kept one release, deprecated |
| `InferenceFailoverOutput.toModel` | `Failover.toModelName` | renamed | a name, as configured; old name kept one release, deprecated |
| `InferenceFailoverOutput.reason` | `Failover.why` | renamed | `reason` is the error vocabulary's word; old name kept one release, deprecated |
| `InferenceFailoverOutput.failureKind` | `Failover.failure` | renamed | a stable label, not a kind; old name kept one release, deprecated |
| `InferenceFailoverOutput.at` | `Failover.at` | kept |  |

### `InstallCommandInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InstallCommandInput` | — | dropped | nothing filters on `InstallCommand` in v2 |
| `InstallCommandInput.os` | — | dropped | follows `InstallCommand.os`, which v2 does not filter on |
| `InstallCommandInput.command` | — | dropped | follows `InstallCommand.command`, which v2 does not filter on |
| `InstallCommandInput.and` | — | dropped | follows its filter |
| `InstallCommandInput.or` | — | dropped | follows its filter |
| `InstallCommandInput.not` | — | dropped | follows its filter |
| `InstallCommandInput.isNull` | — | dropped | follows its filter |

### `InstallCommandListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InstallCommandListInput` | — | dropped | no v2 listing filters a list of `InstallCommand` (now `InstallCommand`) |
| `InstallCommandListInput.some` | — | dropped | follows its list filter |
| `InstallCommandListInput.every` | — | dropped | follows its list filter |
| `InstallCommandListInput.none` | — | dropped | follows its list filter |
| `InstallCommandListInput.isNull` | — | dropped | follows its list filter |

### `InstallCommandOutput` → `InstallCommand`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InstallCommandOutput` | `InstallCommand` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `InstallCommandOutput.os` | `InstallCommand.os` | reshaped | now `OperatingSystem` |
| `InstallCommandOutput.command` | `InstallCommand.command` | kept |  |

### `InstallGlobalToolArgsOutput` → `InstallToolArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InstallGlobalToolArgsOutput` | `InstallToolArguments` | merged | `install_self_tool` and `install_global_tool` take the same arguments |
| `InstallGlobalToolArgsOutput.name` | `InstallToolArguments.name` | kept |  |
| `InstallGlobalToolArgsOutput.source` | `InstallToolArguments.code` | renamed | Rhai code, and `source` reads as provenance |
| `InstallGlobalToolArgsOutput.overwrite` | `InstallToolArguments.overwrite` | kept |  |

### `InstallGlobalToolCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InstallGlobalToolCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `InstallGlobalToolCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `InstallGlobalToolCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `InstallGlobalToolCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `InstallGlobalToolCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `InstallToolArguments` for `install_global_tool` |

### `InstallMethod`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InstallMethod` | `InstallMethod` | kept |  |
| `InstallMethod.HOMEBREW` | `InstallMethod.HOMEBREW` | kept |  |
| `InstallMethod.SCOOP` | `InstallMethod.SCOOP` | kept |  |
| `InstallMethod.CARGO` | `InstallMethod.CARGO` | kept |  |
| `InstallMethod.SCRIPT` | `InstallMethod.SCRIPT` | kept |  |
| `InstallMethod.UNKNOWN` | `InstallMethod.OTHER` | renamed | a known answer, not an unknown |

### `InstallSelfToolArgsOutput` → `InstallToolArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InstallSelfToolArgsOutput` | `InstallToolArguments` | merged | same arguments as `install_global_tool` |
| `InstallSelfToolArgsOutput.name` | `InstallToolArguments.name` | kept |  |
| `InstallSelfToolArgsOutput.source` | `InstallToolArguments.code` | renamed | Rhai code |
| `InstallSelfToolArgsOutput.overwrite` | `InstallToolArguments.overwrite` | kept |  |

### `InstallSelfToolCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InstallSelfToolCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `InstallSelfToolCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `InstallSelfToolCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `InstallSelfToolCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `InstallSelfToolCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `InstallToolArguments` for `install_self_tool` |

### `InteractionAnswerWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InteractionAnswerWrite` | `InteractionAnswerWrite` | kept |  |
| `InteractionAnswerWrite.choice` | `InteractionAnswerWrite.choice` | kept |  |
| `InteractionAnswerWrite.text` | `InteractionAnswerWrite.text` | kept |  |
| `InteractionAnswerWrite.approve` | `InteractionAnswerWrite.approve` | kept |  |
| `InteractionAnswerWrite.deny` | `InteractionAnswerWrite.deny` | kept |  |

### `InteractionConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InteractionConnection` | `InteractionConnection` | kept |  |
| `InteractionConnection.results` | `InteractionConnection.results` | kept |  |
| `InteractionConnection.cursor` | `InteractionConnection.cursor` | kept |  |
| `InteractionConnection.total` | `InteractionConnection.total` | kept |  |

### `InteractionInput` → `InteractionFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InteractionInput` | `InteractionFilter` | kept |  |
| `InteractionInput.id` | `InteractionFilter.id` | kept |  |
| `InteractionInput.run` | — | dropped | follows `Interaction.run`, which v2 does not filter on |
| `InteractionInput.kind` | — | dropped | follows `Interaction.kind`, which went |
| `InteractionInput.prompt` | `InteractionFilter.prompt` | kept |  |
| `InteractionInput.body` | — | dropped | follows `Interaction.body`, which went |
| `InteractionInput.options` | `ChoiceQuestionFilter.options` | renamed | follows `ChoiceQuestion.options` |
| `InteractionInput.toolName` | — | dropped | follows `Interaction.toolName`, which went |
| `InteractionInput.stageName` | `InteractionFilter.stage` | reshaped | now `RunStageFilter`; follows `Interaction.stage` |
| `InteractionInput.isRequired` | `InteractionFilter.holdsRun` | renamed | follows `Interaction.holdsRun` |
| `InteractionInput.askedAt` | `InteractionFilter.askedAt` | kept |  |
| `InteractionInput.settlement` | `InteractionFilter.settlement` | kept |  |
| `InteractionInput.settledAt` | `SettlementFilter.settledAt` | renamed | follows `Settlement.settledAt` |
| `InteractionInput.and` | `InteractionFilter.and` | kept |  |
| `InteractionInput.or` | `InteractionFilter.or` | kept |  |
| `InteractionInput.not` | `InteractionFilter.not` | kept |  |
| `InteractionInput.isNull` | — | dropped | follows its filter |

### `InteractionKind`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InteractionKind` | — | dropped | the `Interaction` implementer is the kind |
| `InteractionKind.FREE_TEXT` | `TextQuestion` | reshaped | a type per kind; `TaintGateApproval` is new |
| `InteractionKind.MULTIPLE_CHOICE` | `ChoiceQuestion` | reshaped | a type per kind; `TaintGateApproval` is new |
| `InteractionKind.CONFIRM` | `ConfirmQuestion` | reshaped | a type per kind; `TaintGateApproval` is new |
| `InteractionKind.TOOL_APPROVAL` | `ToolApproval` | reshaped | a type per kind; `TaintGateApproval` is new |
| `InteractionKind.EDIT_TEXT` | `DocumentEdit` | reshaped | a type per kind; `TaintGateApproval` is new |

### `InteractionKindFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InteractionKindFilter` | — | dropped | `InteractionKind` went |
| `InteractionKindFilter.eq` | — | dropped | follows `InteractionKind` |
| `InteractionKindFilter.ne` | — | dropped | follows `InteractionKind` |
| `InteractionKindFilter.in` | — | dropped | follows `InteractionKind` |
| `InteractionKindFilter.notIn` | — | dropped | follows `InteractionKind` |
| `InteractionKindFilter.isNull` | — | dropped | follows its filter |

### `InteractionOpenedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InteractionOpenedEvent` | `InteractionOpenedEvent` | kept |  |
| `InteractionOpenedEvent.seq` | `InteractionOpenedEvent.seq` | kept |  |
| `InteractionOpenedEvent.at` | `InteractionOpenedEvent.at` | kept |  |
| `InteractionOpenedEvent.runId` | `InteractionOpenedEvent.runId` | kept |  |
| `InteractionOpenedEvent.agentId` | `RunEvent.runId` | dropped | the run's live agent id is not a `Run` fact; frames carry `runId` and `run` |
| `InteractionOpenedEvent.interaction` | `InteractionOpenedEvent.interaction` | kept |  |
| `InteractionOpenedEvent.run` | `InteractionOpenedEvent.run` | kept |  |

### `InteractionOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InteractionOrder` | `InteractionOrder` | kept |  |
| `InteractionOrder.field` | `InteractionOrder.field` | kept |  |
| `InteractionOrder.direction` | `InteractionOrder.direction` | kept |  |

### `InteractionOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InteractionOrderField` | `InteractionOrderField` | kept |  |
| `InteractionOrderField.SEQUENCE` | — | dropped | not a sort key in v2 |

### `InteractionOutput` → `Interaction`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InteractionOutput` | `Interaction` | reshaped | an interface over six types that say what is asked; open and settled read the same |
| `InteractionOutput.id` | `Interaction.id` | kept |  |
| `InteractionOutput.run` | `Interaction.run` | kept |  |
| `InteractionOutput.kind` | `__typename` | dropped | the type is the kind: `ToolApproval`, `TaintGateApproval`, `TextQuestion`, `ChoiceQuestion`, `ConfirmQuestion`, `DocumentEdit` |
| `InteractionOutput.prompt` | `Interaction.prompt` | kept |  |
| `InteractionOutput.body` | `TextQuestion.document`, `ChoiceQuestion.document`, `ConfirmQuestion.document`, `DocumentEdit.document` | reshaped | kept after settling |
| `InteractionOutput.options` | `ChoiceQuestion.options` | reshaped | only choice questions have options; kept after settling |
| `InteractionOutput.toolCall` | `ToolApproval.call`, `TaintGateApproval.call` | reshaped | only approvals gate a call |
| `InteractionOutput.toolName` | `ToolApproval.call` (`ToolCall.toolName`) | reshaped | on the call |
| `InteractionOutput.stageName` | `Interaction.stage` | reshaped | a relation to the run's `RunStage` |
| `InteractionOutput.isRequired` | `Interaction.holdsRun` | renamed | now nullable; says what it means for the run; old name kept one release, deprecated |
| `InteractionOutput.askedAt` | `Interaction.askedAt` | kept |  |
| `InteractionOutput.settlement` | `Interaction.settlement` | kept |  |
| `InteractionOutput.settledAt` | `Settlement.settledAt` | merged | on the settlement |

### `InteractionPointInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InteractionPointInput` | — | dropped | nothing filters on `InteractionPoint` in v2 |
| `InteractionPointInput.name` | — | dropped | follows `InteractionPoint.name`, which v2 does not filter on |
| `InteractionPointInput.prompt` | — | dropped | follows `InteractionPoint.prompt`, which v2 does not filter on |
| `InteractionPointInput.required` | — | dropped | follows `FreeTextPoint.answerRequired`, which v2 does not filter on |
| `InteractionPointInput.unattended` | — | dropped | follows `InteractionPoint.whenUnattended`, which v2 does not filter on |
| `InteractionPointInput.style` | — | dropped | follows `InteractionPoint.style`, which went |
| `InteractionPointInput.options` | — | dropped | follows `MultipleChoicePoint.options`, which v2 does not filter on |
| `InteractionPointInput.directives` | — | dropped | follows `ReviseAnswer`, which v2 does not filter on |
| `InteractionPointInput.abortOptions` | — | dropped | follows `AbortAnswer`, which v2 does not filter on |
| `InteractionPointInput.editOptions` | — | dropped | follows `EditAnswer`, which v2 does not filter on |
| `InteractionPointInput.documentRegion` | — | dropped | follows `InteractionPoint.documentRegion`, which v2 does not filter on |
| `InteractionPointInput.documentRegionName` | — | dropped | follows `InteractionPoint.documentRegion`, which v2 does not filter on |
| `InteractionPointInput.and` | — | dropped | follows its filter |
| `InteractionPointInput.or` | — | dropped | follows its filter |
| `InteractionPointInput.not` | — | dropped | follows its filter |
| `InteractionPointInput.isNull` | — | dropped | follows its filter |

### `InteractionPointListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InteractionPointListInput` | — | dropped | no v2 listing filters a list of `InteractionPoint` (now `InteractionPoint`) |
| `InteractionPointListInput.some` | — | dropped | follows its list filter |
| `InteractionPointListInput.every` | — | dropped | follows its list filter |
| `InteractionPointListInput.none` | — | dropped | follows its list filter |
| `InteractionPointListInput.isNull` | — | dropped | follows its list filter |

### `InteractionPointOutput` → `InteractionPoint`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InteractionPointOutput` | `InteractionPoint` | reshaped | split by style; answers are `AnswerOption` types |
| `InteractionPointOutput.name` | `InteractionPoint.name` | kept |  |
| `InteractionPointOutput.prompt` | `InteractionPoint.prompt` | kept |  |
| `InteractionPointOutput.required` | `FreeTextPoint.answerRequired` | reshaped | read only for free text |
| `InteractionPointOutput.unattended` | `InteractionPoint.whenUnattended` | renamed | says when it applies; old name kept one release, deprecated |
| `InteractionPointOutput.style` | `__typename` | reshaped | `FreeTextPoint`, `MultipleChoicePoint`, `ConfirmPoint` |
| `InteractionPointOutput.options` | `MultipleChoicePoint.options` | reshaped | `AnswerOption`s, each with what it does |
| `InteractionPointOutput.directives` | `ReviseAnswer` | reshaped | answers that revise, among the `AnswerOption`s |
| `InteractionPointOutput.abortOptions` | `AbortAnswer` | reshaped | answers that cancel, among the `AnswerOption`s |
| `InteractionPointOutput.editOptions` | `EditAnswer` | reshaped | answers that edit, among the `AnswerOption`s |
| `InteractionPointOutput.documentRegion` | `InteractionPoint.documentRegion` | kept |  |
| `InteractionPointOutput.documentRegionName` | `InteractionPoint.documentRegion` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |

### `InteractionPointStyle`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InteractionPointStyle` | — | dropped | the `InteractionPoint` implementer is the style |
| `InteractionPointStyle.FREE_TEXT` | `FreeTextPoint` | reshaped | a type per style |
| `InteractionPointStyle.MULTIPLE_CHOICE` | `MultipleChoicePoint` | reshaped | a type per style |
| `InteractionPointStyle.CONFIRM` | `ConfirmPoint` | reshaped | a type per style |

### `InteractionPointStyleFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `InteractionPointStyleFilter` | — | dropped | `InteractionPointStyle` went |
| `InteractionPointStyleFilter.eq` | — | dropped | follows `InteractionPointStyle` |
| `InteractionPointStyleFilter.ne` | — | dropped | follows `InteractionPointStyle` |
| `InteractionPointStyleFilter.in` | — | dropped | follows `InteractionPointStyle` |
| `InteractionPointStyleFilter.notIn` | — | dropped | follows `InteractionPointStyle` |
| `InteractionPointStyleFilter.isNull` | — | dropped | follows its filter |

### `IntFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `IntFilter` | `IntFilter` | kept |  |
| `IntFilter.eq` | `IntFilter.eq` | kept |  |
| `IntFilter.ne` | `IntFilter.ne` | kept |  |
| `IntFilter.in` | `IntFilter.in` | kept |  |
| `IntFilter.notIn` | `IntFilter.notIn` | kept |  |
| `IntFilter.lt` | `IntFilter.lt` | kept |  |
| `IntFilter.lte` | `IntFilter.lte` | kept |  |
| `IntFilter.gt` | `IntFilter.gt` | kept |  |
| `IntFilter.gte` | `IntFilter.gte` | kept |  |
| `IntFilter.isNull` | `IntFilter.isNull` | kept |  |

### `JournalHealthOutput` → `JournalHealth`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `JournalHealthOutput` | `JournalHealth` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `JournalHealthOutput.healthy` | `JournalHealth.healthy` | kept |  |
| `JournalHealthOutput.appendsAttempted` | `JournalHealth.appendsAttempted` | kept |  |
| `JournalHealthOutput.appendsFailed` | `JournalHealth.appendsFailed` | kept |  |
| `JournalHealthOutput.snapshotsFailed` | `JournalHealth.snapshotsFailed` | kept |  |
| `JournalHealthOutput.queueDepth` | `JournalHealth.queueDepth` | kept |  |
| `JournalHealthOutput.lastError` | `JournalHealth.lastError` | kept |  |

### `JournalWriteErrorOutput` → `JournalWriteError`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `JournalWriteErrorOutput` | `JournalWriteError` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `JournalWriteErrorOutput.runId` | `JournalWriteError.runId` | kept |  |
| `JournalWriteErrorOutput.path` | `JournalWriteError.path` | kept |  |
| `JournalWriteErrorOutput.message` | `JournalWriteError.message` | kept |  |
| `JournalWriteErrorOutput.at` | `JournalWriteError.at` | kept |  |

### `JSON`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `JSON` | `JSON` | kept |  |

### `JSONFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `JSONFilter` | — | dropped | nothing filters on `JSON` in v2 |
| `JSONFilter.eq` | — | dropped | follows `JSON.eq`, which went |
| `JSONFilter.ne` | — | dropped | follows `JSON.ne`, which went |
| `JSONFilter.isNull` | — | dropped | follows its filter |

### `KeyValueWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `KeyValueWrite` | `KeyValueWrite` | kept |  |
| `KeyValueWrite.key` | `KeyValueWrite.key` | kept |  |
| `KeyValueWrite.value` | `KeyValueWrite.value` | kept |  |

### `KeyVerdictOutput` → `Validation`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `KeyVerdictOutput` | `Validation` | merged | one verdict type for every `validate...` query |
| `KeyVerdictOutput.valid` | `Validation.valid` | kept |  |
| `KeyVerdictOutput.message` | `Validation.problems` | reshaped | a `TextProblem` |

### `KillAgentArgsOutput` → `SubAgentArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `KillAgentArgsOutput` | `SubAgentArguments` | merged | same arguments as `check_agent` |
| `KillAgentArgsOutput.agentId` | `SubAgentArguments.runId` | renamed | the run id the model wrote, with `run` resolving it |

### `KillAgentCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `KillAgentCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `KillAgentCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `KillAgentCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `KillAgentCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `KillAgentCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `SubAgentArguments` for `kill_agent` |

### `ListDirArgsOutput` → `ListDirArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ListDirArgsOutput` | `ListDirArguments` | renamed | argument types are named `XArguments` |
| `ListDirArgsOutput.path` | `ListDirArguments.path` | kept |  |

### `ListDirCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ListDirCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `ListDirCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `ListDirCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `ListDirCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `ListDirCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `ListDirArguments` for `list_dir` |

### `LocaleInfoCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `LocaleInfoCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `LocaleInfoCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `LocaleInfoCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `LocaleInfoCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |

### `LogLineWrittenEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `LogLineWrittenEvent` | `LogLineWrittenEvent` | kept |  |
| `LogLineWrittenEvent.seq` | `LogLineWrittenEvent.seq` | kept |  |
| `LogLineWrittenEvent.at` | `LogLineWrittenEvent.at` | kept |  |
| `LogLineWrittenEvent.runId` | `LogLineWrittenEvent.runId` | kept |  |
| `LogLineWrittenEvent.agentId` | `RunEvent.runId` | dropped | the run's live agent id is not a `Run` fact; frames carry `runId` and `run` |
| `LogLineWrittenEvent.line` | `LogLineWrittenEvent.line` | kept |  |
| `LogLineWrittenEvent.run` | `LogLineWrittenEvent.run` | kept |  |

### `LogStageOptions`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `LogStageOptions` | — | dropped | logs are read per stage, `RunStage.log` |
| `LogStageOptions.index` | `RunStage.log` | dropped | logs are read on the stage |
| `LogStageOptions.all` | `Run.stages` (`RunStage.log`) | dropped | every stage's log is `stages { log }` |

### `LogStream`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `LogStream` | `LogStream` | kept |  |
| `LogStream.OUTPUT` | `LogStream.OUTPUT` | kept |  |
| `LogStream.OPERATIONAL` | `LogStream.OPERATIONAL` | kept |  |

### `MachineEventFrame`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MachineEventFrame` | `MachineEventFrame` | kept |  |
| `MachineEventFrame.DaemonLinkChangedEvent` | `MachineEventFrame.DaemonLinkChangedEvent` | kept |  |
| `MachineEventFrame.ConfigHealthChangedEvent` | `MachineEventFrame.ConfigHealthChangedEvent` | kept |  |
| `MachineEventFrame.UpdateStepChangedEvent` | `MachineEventFrame.UpdateStepChangedEvent` | kept |  |
| `MachineEventFrame.UpdateFinishedEvent` | `MachineEventFrame.UpdateFinishedEvent` | kept |  |
| `MachineEventFrame.SubscriptionOpenedEvent` | `MachineEventFrame.SubscriptionOpenedEvent` | kept |  |
| `MachineEventFrame.EventsDroppedEvent` | `MachineEventFrame.EventsDroppedEvent` | kept |  |

### `MachineEventType` → `MachineEventFrameFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MachineEventType` | `MachineEventFrameFilter` | reshaped | frames are chosen with one field per frame type |
| `MachineEventType.DAEMON_LINK_CHANGED` | `MachineEventFrameFilter.daemonLinkChangedEvent` | reshaped | one boolean field per frame type |
| `MachineEventType.CONFIG_HEALTH_CHANGED` | `MachineEventFrameFilter.configHealthChangedEvent` | reshaped | one boolean field per frame type |
| `MachineEventType.UPDATE_STEP_CHANGED` | `MachineEventFrameFilter.updateStepChangedEvent` | reshaped | one boolean field per frame type |
| `MachineEventType.UPDATE_FINISHED` | `MachineEventFrameFilter.updateFinishedEvent` | reshaped | one boolean field per frame type |

### `MappingTransform`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MappingTransform` | — | dropped | the `RegionHandoff` implementer is the treatment |
| `MappingTransform.DIRECT` | `CopiedRegion` | reshaped | a type each |
| `MappingTransform.SUMMARIZE` | `SummarizedRegion` | reshaped | a type each |
| `MappingTransform.EXTRACT` | `ExtractedRegion` | reshaped | a type each |

### `MappingTransformFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MappingTransformFilter` | — | dropped | `MappingTransform` went |
| `MappingTransformFilter.eq` | — | dropped | follows `MappingTransform` |
| `MappingTransformFilter.ne` | — | dropped | follows `MappingTransform` |
| `MappingTransformFilter.in` | — | dropped | follows `MappingTransform` |
| `MappingTransformFilter.notIn` | — | dropped | follows `MappingTransform` |
| `MappingTransformFilter.isNull` | — | dropped | follows its filter |

### `MaxOutputTokens` → `ReplyTokenLimit`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MaxOutputTokens` | `ReplyTokenLimit` | renamed | says what it limits |
| `MaxOutputTokens.MaxTokensCountOutput` | `ReplyTokenLimit.TokenCount` | renamed |  |
| `MaxOutputTokens.MaxTokensContextPercentOutput` | `ReplyTokenLimit.WindowShare` | renamed |  |
| `MaxOutputTokens.MaxTokensRegionPercentOutput` | `ReplyTokenLimit.RegionShare` | renamed |  |

### `MaxOutputTokensInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MaxOutputTokensInput` | — | dropped | nothing filters on `ReplyTokenLimit` in v2 |
| `MaxOutputTokensInput.maxTokensCount` | — | dropped | follows `MaxOutputTokens.maxTokensCount`, which went |
| `MaxOutputTokensInput.maxTokensContextPercent` | — | dropped | follows `MaxOutputTokens.maxTokensContextPercent`, which went |
| `MaxOutputTokensInput.maxTokensRegionPercent` | — | dropped | follows `MaxOutputTokens.maxTokensRegionPercent`, which went |
| `MaxOutputTokensInput.and` | — | dropped | follows its filter |
| `MaxOutputTokensInput.or` | — | dropped | follows its filter |
| `MaxOutputTokensInput.not` | — | dropped | follows its filter |
| `MaxOutputTokensInput.isNull` | — | dropped | follows its filter |

### `MaxTokensContextPercentInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MaxTokensContextPercentInput` | — | dropped | nothing filters on `WindowShare` in v2 |
| `MaxTokensContextPercentInput.percent` | — | dropped | follows `WindowShare.percent`, which v2 does not filter on |
| `MaxTokensContextPercentInput.and` | — | dropped | follows its filter |
| `MaxTokensContextPercentInput.or` | — | dropped | follows its filter |
| `MaxTokensContextPercentInput.not` | — | dropped | follows its filter |
| `MaxTokensContextPercentInput.isNull` | — | dropped | follows its filter |

### `MaxTokensContextPercentOutput` → `WindowShare`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MaxTokensContextPercentOutput` | `WindowShare` | renamed | a share of the context window |
| `MaxTokensContextPercentOutput.percent` | `WindowShare.percent` | kept |  |

### `MaxTokensCountInput` → `TokenCountFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MaxTokensCountInput` | `TokenCountFilter` | renamed | follows `TokenCount` |
| `MaxTokensCountInput.tokens` | `TokenCountFilter.tokens` | renamed | follows `TokenCount.tokens` |
| `MaxTokensCountInput.and` | `TokenCountFilter.and` | reshaped | now `[TokenCountFilter]` |
| `MaxTokensCountInput.or` | `TokenCountFilter.or` | reshaped | now `[TokenCountFilter]` |
| `MaxTokensCountInput.not` | `TokenCountFilter.not` | reshaped | now `TokenCountFilter` |
| `MaxTokensCountInput.isNull` | `TokenCountFilter.isNull` | kept |  |

### `MaxTokensCountOutput` → `TokenCount`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MaxTokensCountOutput` | `TokenCount` | merged | one token-count type |
| `MaxTokensCountOutput.tokens` | `TokenCount.tokens` | kept |  |

### `MaxTokensRegionPercentInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MaxTokensRegionPercentInput` | — | dropped | nothing filters on `RegionShare` in v2 |
| `MaxTokensRegionPercentInput.percent` | — | dropped | follows `RegionShare.percent`, which v2 does not filter on |
| `MaxTokensRegionPercentInput.region` | — | dropped | follows `RegionShare.region`, which v2 does not filter on |
| `MaxTokensRegionPercentInput.and` | — | dropped | follows its filter |
| `MaxTokensRegionPercentInput.or` | — | dropped | follows its filter |
| `MaxTokensRegionPercentInput.not` | — | dropped | follows its filter |
| `MaxTokensRegionPercentInput.isNull` | — | dropped | follows its filter |

### `MaxTokensRegionPercentOutput` → `RegionShare`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MaxTokensRegionPercentOutput` | `RegionShare` | reshaped | a share of one region, now a relation |
| `MaxTokensRegionPercentOutput.percent` | `RegionShare.percent` | kept |  |
| `MaxTokensRegionPercentOutput.region` | `RegionShare.region` | reshaped | the `Region`, not a name |

### `McpAuth` → `McpAuthorization`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpAuth` | `McpAuthorization` | reshaped | only HTTP servers have credentials, so `NOT_APPLICABLE` goes |
| `McpAuth.NOT_APPLICABLE` | `StdioMcpServer` | dropped | existed only because stdio and HTTP shared a type |
| `McpAuth.NONE` | `McpAuthorization.NONE` | kept |  |
| `McpAuth.HEADER` | `McpAuthorization.CONFIGURED_HEADER` | renamed | plain words |
| `McpAuth.AUTHENTICATED` | `McpAuthorization.SIGNED_IN` | renamed | plain words |
| `McpAuth.EXPIRED` | `McpAuthorization.SIGN_IN_EXPIRED` | renamed | plain words |

### `McpAuthFilter` → `McpAuthorizationFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpAuthFilter` | `McpAuthorizationFilter` | renamed | follows `McpAuthorization` |
| `McpAuthFilter.eq` | `McpAuthorizationFilter.eq` | kept |  |
| `McpAuthFilter.ne` | `McpAuthorizationFilter.ne` | kept |  |
| `McpAuthFilter.in` | `McpAuthorizationFilter.in` | kept |  |
| `McpAuthFilter.notIn` | `McpAuthorizationFilter.notIn` | kept |  |
| `McpAuthFilter.isNull` | `McpAuthorizationFilter.isNull` | kept |  |

### `McpHttpWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpHttpWrite` | `McpHttpWrite` | kept |  |
| `McpHttpWrite.url` | `McpHttpWrite.url` | kept |  |
| `McpHttpWrite.headers` | `McpHttpWrite.headers` | kept |  |

### `McpLoginStatus` → `McpSignInOutcome`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpLoginStatus` | `McpSignInOutcome` | renamed | plain English |
| `McpLoginStatus.AUTHENTICATED` | `McpSignInOutcome.SIGNED_IN` | renamed | plain words |
| `McpLoginStatus.NOT_REQUIRED` | `McpSignInOutcome.NOT_NEEDED` | renamed | plain words |

### `McpServerConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpServerConnection` | `McpServerConnection` | kept |  |
| `McpServerConnection.results` | `McpServerConnection.results` | kept |  |
| `McpServerConnection.cursor` | `McpServerConnection.cursor` | kept |  |
| `McpServerConnection.total` | `McpServerConnection.total` | kept |  |

### `McpServerInput` → `McpServerFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpServerInput` | `McpServerFilter` | kept |  |
| `McpServerInput.id` | `McpServerFilter.id` | kept |  |
| `McpServerInput.name` | `McpServerFilter.name` | kept |  |
| `McpServerInput.transport` | — | dropped | follows `McpServer.transport`, which went |
| `McpServerInput.endpoint` | — | dropped | follows `McpServer.endpoint`, which went |
| `McpServerInput.command` | `StdioMcpServerFilter.command` | renamed | follows `StdioMcpServer.command` |
| `McpServerInput.url` | `HttpMcpServerFilter.url` | renamed | follows `HttpMcpServer.url` |
| `McpServerInput.args` | `StdioMcpServerFilter.args` | renamed | follows `StdioMcpServer.args` |
| `McpServerInput.headerNames` | `HttpMcpServerFilter.headerNames` | renamed | follows `HttpMcpServer.headerNames` |
| `McpServerInput.envNames` | `StdioMcpServerFilter.envNames` | renamed | follows `StdioMcpServer.envNames` |
| `McpServerInput.configError` | `McpServerFilter.configError` | kept |  |
| `McpServerInput.auth` | `HttpMcpServerFilter.authorization` | reshaped | now `McpAuthorizationFilter`; follows `HttpMcpServer.authorization` |
| `McpServerInput.and` | `McpServerFilter.and` | kept |  |
| `McpServerInput.or` | `McpServerFilter.or` | kept |  |
| `McpServerInput.not` | `McpServerFilter.not` | kept |  |
| `McpServerInput.isNull` | `McpServerFilter.isNull` | kept |  |

### `McpServerOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpServerOrder` | `McpServerOrder` | kept |  |
| `McpServerOrder.field` | `McpServerOrder.field` | kept |  |
| `McpServerOrder.direction` | `McpServerOrder.direction` | kept |  |

### `McpServerOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpServerOrderField` | `McpServerOrderField` | kept |  |
| `McpServerOrderField.ID` | `McpServerOrderField.ID` | kept |  |
| `McpServerOrderField.NAME` | `McpServerOrderField.NAME` | kept |  |
| `McpServerOrderField.ENDPOINT` | — | dropped | not a sort key in v2 |

### `McpServerOutput` → `McpServer`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpServerOutput` | `McpServer` | reshaped | split by transport into `StdioMcpServer`, `HttpMcpServer`, `UnresolvedMcpServer` |
| `McpServerOutput.id` | `McpServer.id` | kept |  |
| `McpServerOutput.name` | `McpServer.name` | kept |  |
| `McpServerOutput.transport` | `__typename` | reshaped | `StdioMcpServer`, `HttpMcpServer`, `UnresolvedMcpServer` |
| `McpServerOutput.endpoint` | `StdioMcpServer.command`, `HttpMcpServer.url` | reshaped | meant a command or a URL depending on the transport |
| `McpServerOutput.command` | `StdioMcpServer.command` | reshaped | only a stdio server has one |
| `McpServerOutput.url` | `HttpMcpServer.url` | reshaped | only an HTTP server has one |
| `McpServerOutput.args` | `StdioMcpServer.args` | reshaped | only a stdio server has them |
| `McpServerOutput.headerNames` | `HttpMcpServer.headerNames` | reshaped | only an HTTP server has them |
| `McpServerOutput.envNames` | `StdioMcpServer.envNames` | reshaped | only a stdio server has them |
| `McpServerOutput.configError` | `McpServer.configError` | kept |  |
| `McpServerOutput.auth` | `HttpMcpServer.authorization` | reshaped | only an HTTP server has credentials |

### `McpServerTemplateInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpServerTemplateInput` | — | dropped | nothing filters on `McpServerTemplate` in v2 |
| `McpServerTemplateInput.transport` | — | dropped | follows `McpServerTemplate.transport`, which went |
| `McpServerTemplateInput.command` | — | dropped | follows `StdioMcpServerTemplate.command`, which v2 does not filter on |
| `McpServerTemplateInput.url` | — | dropped | follows `HttpMcpServerTemplate.url`, which v2 does not filter on |
| `McpServerTemplateInput.args` | — | dropped | follows `StdioMcpServerTemplate.args`, which v2 does not filter on |
| `McpServerTemplateInput.env` | — | dropped | follows `StdioMcpServerTemplate.env`, which v2 does not filter on |
| `McpServerTemplateInput.headers` | — | dropped | follows `HttpMcpServerTemplate.headers`, which v2 does not filter on |
| `McpServerTemplateInput.and` | — | dropped | follows its filter |
| `McpServerTemplateInput.or` | — | dropped | follows its filter |
| `McpServerTemplateInput.not` | — | dropped | follows its filter |
| `McpServerTemplateInput.isNull` | — | dropped | follows its filter |

### `McpServerTemplateOutput` → `McpServerTemplate`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpServerTemplateOutput` | `McpServerTemplate` | reshaped | split by transport into a union |
| `McpServerTemplateOutput.transport` | `__typename` | reshaped | `StdioMcpServerTemplate` or `HttpMcpServerTemplate` |
| `McpServerTemplateOutput.command` | `StdioMcpServerTemplate.command` | reshaped | stdio only |
| `McpServerTemplateOutput.url` | `HttpMcpServerTemplate.url` | reshaped | HTTP only |
| `McpServerTemplateOutput.args` | `StdioMcpServerTemplate.args` | reshaped | stdio only |
| `McpServerTemplateOutput.env` | `StdioMcpServerTemplate.env` | reshaped | stdio only |
| `McpServerTemplateOutput.headers` | `HttpMcpServerTemplate.headers` | reshaped | HTTP only |

### `McpServerWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpServerWrite` | `McpServerWrite` | kept |  |
| `McpServerWrite.name` | `McpServerWrite.name` | kept |  |
| `McpServerWrite.transport` | `McpServerWrite.transport` | kept |  |
| `McpServerWrite.env` | `McpStdioWrite.env` | reshaped | only a stdio server has an environment |

### `McpStdioWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpStdioWrite` | `McpStdioWrite` | kept |  |
| `McpStdioWrite.command` | `McpStdioWrite.command` | kept |  |
| `McpStdioWrite.args` | `McpStdioWrite.args` | kept |  |

### `McpTransport`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpTransport` | — | dropped | the `McpServer` implementer is the transport; writes use `McpTransportWrite` |
| `McpTransport.STDIO` | `StdioMcpServer` | reshaped | a type each; writes use `McpTransportWrite` |
| `McpTransport.HTTP` | `HttpMcpServer` | reshaped | a type each; writes use `McpTransportWrite` |

### `McpTransportFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpTransportFilter` | — | dropped | `McpTransport` went |
| `McpTransportFilter.eq` | — | dropped | follows `McpTransport` |
| `McpTransportFilter.ne` | — | dropped | follows `McpTransport` |
| `McpTransportFilter.in` | — | dropped | follows `McpTransport` |
| `McpTransportFilter.notIn` | — | dropped | follows `McpTransport` |
| `McpTransportFilter.isNull` | — | dropped | follows its filter |

### `McpTransportWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `McpTransportWrite` | `McpTransportWrite` | kept |  |
| `McpTransportWrite.stdio` | `McpTransportWrite.stdio` | kept |  |
| `McpTransportWrite.http` | `McpTransportWrite.http` | kept |  |

### `MetadataEntryInput` → `KeyValueFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MetadataEntryInput` | `KeyValueFilter` | renamed | follows `KeyValue` |
| `MetadataEntryInput.key` | `KeyValueFilter.key` | renamed | follows `KeyValue.key` |
| `MetadataEntryInput.value` | `KeyValueFilter.value` | renamed | follows `KeyValue.value` |
| `MetadataEntryInput.and` | `KeyValueFilter.and` | reshaped | now `[KeyValueFilter]` |
| `MetadataEntryInput.or` | `KeyValueFilter.or` | reshaped | now `[KeyValueFilter]` |
| `MetadataEntryInput.not` | `KeyValueFilter.not` | reshaped | now `KeyValueFilter` |
| `MetadataEntryInput.isNull` | `KeyValueFilter.isNull` | kept |  |

### `MetadataEntryListInput` → `KeyValueListFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MetadataEntryListInput` | `KeyValueListFilter` | renamed | follows `KeyValue` |
| `MetadataEntryListInput.some` | `KeyValueListFilter.some` | reshaped | now `KeyValueFilter` |
| `MetadataEntryListInput.every` | `KeyValueListFilter.every` | reshaped | now `KeyValueFilter` |
| `MetadataEntryListInput.none` | `KeyValueListFilter.none` | reshaped | now `KeyValueFilter` |
| `MetadataEntryListInput.isNull` | `KeyValueListFilter.isNull` | kept |  |

### `MetadataEntryOutput` → `KeyValue`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MetadataEntryOutput` | `KeyValue` | merged | one name-and-value type |
| `MetadataEntryOutput.key` | `KeyValue.key` | kept |  |
| `MetadataEntryOutput.value` | `KeyValue.value` | kept |  |

### `MimeRowConnection` → `MimeTypeConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MimeRowConnection` | `MimeTypeConnection` | renamed | follows `MimeType` |
| `MimeRowConnection.results` | `MimeTypeConnection.results` | renamed |  |
| `MimeRowConnection.cursor` | `MimeTypeConnection.cursor` | renamed |  |
| `MimeRowConnection.total` | `MimeTypeConnection.total` | renamed |  |

### `MimeRowInput` → `MimeTypeFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MimeRowInput` | `MimeTypeFilter` | renamed | follows `MimeType` |
| `MimeRowInput.mimeType` | — | dropped | follows `MimeRow.mimeType`, which went |
| `MimeRowInput.origin` | — | dropped | follows `MimeRow.origin`, which went |
| `MimeRowInput.blueprintName` | — | dropped | follows `BlueprintMimeTypeRule.blueprint`, which v2 does not filter on |
| `MimeRowInput.family` | `MimeTypeFilter.family` | renamed | follows `MimeType.family` |
| `MimeRowInput.isText` | `MimeTypeFilter.isText` | renamed | follows `MimeType.isText` |
| `MimeRowInput.tokens` | `MimeTypeFilter.tokens` | renamed | follows `MimeType.tokens` |
| `MimeRowInput.extensions` | `MimeTypeFilter.extensions` | renamed | follows `MimeType.extensions` |
| `MimeRowInput.magic` | — | dropped | follows `MimeType.magic`, which v2 does not filter on |
| `MimeRowInput.standIn` | `MimeTypeFilter.standIn` | renamed | follows `MimeType.standIn` |
| `MimeRowInput.check` | — | dropped | follows `MimeRow.check`, which went |
| `MimeRowInput.and` | `MimeTypeFilter.and` | reshaped | now `[MimeTypeFilter]` |
| `MimeRowInput.or` | `MimeTypeFilter.or` | reshaped | now `[MimeTypeFilter]` |
| `MimeRowInput.not` | `MimeTypeFilter.not` | reshaped | now `MimeTypeFilter` |
| `MimeRowInput.isNull` | — | dropped | follows its filter |

### `MimeRowListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MimeRowListInput` | — | dropped | no v2 listing filters a list of `MimeRow` (now `MimeType`) |
| `MimeRowListInput.some` | — | dropped | follows its list filter |
| `MimeRowListInput.every` | — | dropped | follows its list filter |
| `MimeRowListInput.none` | — | dropped | follows its list filter |
| `MimeRowListInput.isNull` | — | dropped | follows its list filter |

### `MimeRowOrder` → `MimeTypeOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MimeRowOrder` | `MimeTypeOrder` | renamed | follows `MimeType` |
| `MimeRowOrder.field` | `MimeTypeOrder.field` | reshaped | now `MimeTypeOrderField` |
| `MimeRowOrder.direction` | `MimeTypeOrder.direction` | renamed |  |

### `MimeRowOrderField` → `MimeTypeOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MimeRowOrderField` | `MimeTypeOrderField` | renamed | follows `MimeType` |
| `MimeRowOrderField.MIME_TYPE` | `MimeTypeOrderField.MIME_TYPE` | kept |  |
| `MimeRowOrderField.BLUEPRINT_NAME` | — | dropped | not a sort key in v2 |
| `MimeRowOrderField.FAMILY` | `MimeTypeOrderField.FAMILY` | kept |  |

### `MimeRowOrigin`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MimeRowOrigin` | — | dropped | the `MimeTypeRule` implementer is the layer |
| `MimeRowOrigin.BUILTIN` | `BuiltinMimeTypeRule`, `ProviderMimeTypeRule` | dropped | a type per layer; provider rules were reported as built-in |
| `MimeRowOrigin.CONFIG` | `OperatorMimeTypeRule` | reshaped | a type per layer; provider rules were reported as built-in |
| `MimeRowOrigin.BLUEPRINT` | `BlueprintMimeTypeRule` | reshaped | a type per layer; provider rules were reported as built-in |

### `MimeRowOriginFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MimeRowOriginFilter` | — | dropped | `MimeRowOrigin` went |
| `MimeRowOriginFilter.eq` | — | dropped | follows `MimeRowOrigin` |
| `MimeRowOriginFilter.ne` | — | dropped | follows `MimeRowOrigin` |
| `MimeRowOriginFilter.in` | — | dropped | follows `MimeRowOrigin` |
| `MimeRowOriginFilter.notIn` | — | dropped | follows `MimeRowOrigin` |
| `MimeRowOriginFilter.isNull` | — | dropped | follows its filter |

### `MimeRowOutput` → `MimeType`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MimeRowOutput` | `MimeType` | reshaped | split: what a type resolves to (`MimeType`) and each layer's rule as written (`MimeTypeRule`, one type per layer) |
| `MimeRowOutput.mimeType` | `MimeType.mimeType`, `MimeTypeRule.mimeType` | reshaped | split |
| `MimeRowOutput.origin` | `__typename` | dropped | the `MimeTypeRule` type is the layer |
| `MimeRowOutput.blueprintName` | `BlueprintMimeTypeRule.blueprint` | reshaped | the `BlueprintRevision` that ships it |
| `MimeRowOutput.family` | `MimeType.family` | kept |  |
| `MimeRowOutput.isText` | `MimeType.isText` | kept |  |
| `MimeRowOutput.tokens` | `MimeType.tokens` | kept |  |
| `MimeRowOutput.extensions` | `MimeType.extensions` | kept |  |
| `MimeRowOutput.magic` | `MimeType.magic` | kept |  |
| `MimeRowOutput.standIn` | `MimeType.standIn` | kept |  |
| `MimeRowOutput.check` | `MimeType.check`, `OperatorMimeTypeRule.check` | reshaped | a `MimeCheck`, not a path; `liftsCheck` replaces `""` |

### `MimeRowWrite` → `MimeTypeRuleWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MimeRowWrite` | `MimeTypeRuleWrite` | renamed | a rule of the mime registry |
| `MimeRowWrite.mimeType` | `MimeTypeRuleWrite.mimeType` | kept |  |
| `MimeRowWrite.family` | `MimeTypeRuleWrite.family` | kept |  |
| `MimeRowWrite.isText` | `MimeTypeRuleWrite.isText` | kept |  |
| `MimeRowWrite.extensions` | `MimeTypeRuleWrite.extensions` | kept |  |
| `MimeRowWrite.magic` | `MimeTypeRuleWrite.magic` | kept |  |
| `MimeRowWrite.standIn` | `MimeTypeRuleWrite.standIn` | kept |  |
| `MimeRowWrite.check` | `MimeTypeRuleWrite.check` | reshaped | a `MimeCheckWrite`: a path or a lift, not `""` |
| `MimeRowWrite.tokens` | `MimeTypeRuleWrite.tokens` | kept |  |

### `MimeTokenRule`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MimeTokenRule` | `MimeTokenRule` | reshaped | members renamed to say what they count |
| `MimeTokenRule.PerByteOutput` | `MimeTokenRule.TokensPerByte` | renamed |  |
| `MimeTokenRule.PerPixelOutput` | `MimeTokenRule.TokensPerPixel` | renamed |  |
| `MimeTokenRule.PerSecondOutput` | `MimeTokenRule.TokensPerSecond` | renamed |  |
| `MimeTokenRule.PerPageOutput` | `MimeTokenRule.TokensPerPage` | renamed |  |
| `MimeTokenRule.FixedOutput` | `MimeTokenRule.TokenCount` | merged | one token-count type |

### `MimeTokenRuleInput` → `MimeTokenRuleFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MimeTokenRuleInput` | `MimeTokenRuleFilter` | kept |  |
| `MimeTokenRuleInput.perByte` | — | dropped | follows `MimeTokenRule.perByte`, which went |
| `MimeTokenRuleInput.perPixel` | — | dropped | follows `MimeTokenRule.perPixel`, which went |
| `MimeTokenRuleInput.perSecond` | — | dropped | follows `MimeTokenRule.perSecond`, which went |
| `MimeTokenRuleInput.perPage` | — | dropped | follows `MimeTokenRule.perPage`, which went |
| `MimeTokenRuleInput.fixed` | — | dropped | follows `MimeTokenRule.fixed`, which went |
| `MimeTokenRuleInput.and` | `MimeTokenRuleFilter.and` | kept |  |
| `MimeTokenRuleInput.or` | `MimeTokenRuleFilter.or` | kept |  |
| `MimeTokenRuleInput.not` | `MimeTokenRuleFilter.not` | kept |  |
| `MimeTokenRuleInput.isNull` | `MimeTokenRuleFilter.isNull` | kept |  |

### `MimeTokensWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `MimeTokensWrite` | `MimeTokensWrite` | kept |  |
| `MimeTokensWrite.perByte` | `MimeTokensWrite.perByte` | kept |  |
| `MimeTokensWrite.perPixel` | `MimeTokensWrite.perPixel` | kept |  |
| `MimeTokensWrite.perSecond` | `MimeTokensWrite.perSecond` | kept |  |
| `MimeTokensWrite.perPage` | `MimeTokensWrite.perPage` | kept |  |
| `MimeTokensWrite.fixed` | `MimeTokensWrite.fixed` | kept |  |

### `ModelConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ModelConnection` | `ModelConnection` | kept |  |
| `ModelConnection.results` | `ModelConnection.results` | kept |  |
| `ModelConnection.cursor` | `ModelConnection.cursor` | kept |  |
| `ModelConnection.total` | `ModelConnection.total` | kept |  |

### `ModelInput` → `ModelFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ModelInput` | `ModelFilter` | kept |  |
| `ModelInput.id` | `ModelFilter.id` | kept |  |
| `ModelInput.modelId` | `ModelFilter.name` | renamed | follows `Model.name` |
| `ModelInput.providerId` | `ModelFilter.provider` | reshaped | now `ModelProviderFilter`; follows `Model.provider` |
| `ModelInput.providerName` | — | dropped | follows `Model.providerName`, which went |
| `ModelInput.displayName` | `ModelFilter.displayName` | kept |  |
| `ModelInput.maxContextTokens` | `ModelFilter.maxContextTokens` | kept |  |
| `ModelInput.maxOutputTokens` | `ModelFilter.maxOutputTokens` | kept |  |
| `ModelInput.limitsSource` | `ModelFilter.limitsSource` | kept |  |
| `ModelInput.supportsTools` | `ModelFilter.supportsTools` | kept |  |
| `ModelInput.supportsTemperature` | `ModelFilter.supportsTemperature` | kept |  |
| `ModelInput.learned` | `ModelFilter.isListedByProvider` | renamed | follows `Model.isListedByProvider` |
| `ModelInput.released` | `ModelFilter.released` | kept |  |
| `ModelInput.retires` | — | dropped | follows `Model.retires`, which v2 does not filter on |
| `ModelInput.pricing` | `ModelFilter.pricing` | kept |  |
| `ModelInput.inputTypes` | `ModelFilter.inputTypes` | kept |  |
| `ModelInput.outputTypes` | `ModelFilter.outputTypes` | kept |  |
| `ModelInput.and` | `ModelFilter.and` | kept |  |
| `ModelInput.or` | `ModelFilter.or` | kept |  |
| `ModelInput.not` | `ModelFilter.not` | kept |  |
| `ModelInput.isNull` | — | dropped | follows its filter |

### `ModelLimitsSource`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ModelLimitsSource` | `ModelLimitsSource` | kept |  |
| `ModelLimitsSource.API` | `ModelLimitsSource.API` | kept |  |
| `ModelLimitsSource.BUILTIN` | `ModelLimitsSource.BUILTIN` | kept |  |
| `ModelLimitsSource.OVERRIDE` | `ModelLimitsSource.OVERRIDE` | kept |  |
| `ModelLimitsSource.UNKNOWN` | `ModelLimitsSource.UNKNOWN` | kept |  |

### `ModelLimitsSourceFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ModelLimitsSourceFilter` | `ModelLimitsSourceFilter` | kept |  |
| `ModelLimitsSourceFilter.eq` | `ModelLimitsSourceFilter.eq` | kept |  |
| `ModelLimitsSourceFilter.ne` | `ModelLimitsSourceFilter.ne` | kept |  |
| `ModelLimitsSourceFilter.in` | `ModelLimitsSourceFilter.in` | kept |  |
| `ModelLimitsSourceFilter.notIn` | `ModelLimitsSourceFilter.notIn` | kept |  |
| `ModelLimitsSourceFilter.isNull` | `ModelLimitsSourceFilter.isNull` | kept |  |

### `ModelOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ModelOrder` | `ModelOrder` | kept |  |
| `ModelOrder.field` | `ModelOrder.field` | kept |  |
| `ModelOrder.direction` | `ModelOrder.direction` | kept |  |

### `ModelOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ModelOrderField` | `ModelOrderField` | kept |  |
| `ModelOrderField.ID` | `ModelOrderField.ID` | kept |  |
| `ModelOrderField.MODEL_ID` | — | dropped | not a sort key in v2 |
| `ModelOrderField.PROVIDER_ID` | — | dropped | not a sort key in v2 |
| `ModelOrderField.PROVIDER_NAME` | — | dropped | not a sort key in v2 |

### `ModelOutput` → `Model`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ModelOutput` | `Model` | reshaped | `provider` is a relation to its `ModelProvider` |
| `ModelOutput.id` | `Model.id` | kept |  |
| `ModelOutput.modelId` | `Model.name` | renamed | the model as the provider names it; old name kept one release, deprecated |
| `ModelOutput.providerId` | `Model.provider` | reshaped | the `ModelProvider` itself; the old id did not resolve |
| `ModelOutput.providerName` | `Model.provider` (`ModelProvider.name`) | merged | on the relation |
| `ModelOutput.displayName` | `Model.displayName` | kept |  |
| `ModelOutput.maxContextTokens` | `Model.maxContextTokens` | kept |  |
| `ModelOutput.maxOutputTokens` | `Model.maxOutputTokens` | kept |  |
| `ModelOutput.limitsSource` | `Model.limitsSource` | kept |  |
| `ModelOutput.supportsTools` | `Model.supportsTools` | kept |  |
| `ModelOutput.supportsTemperature` | `Model.supportsTemperature` | kept |  |
| `ModelOutput.learned` | `Model.isListedByProvider` | renamed | says what it means; old name kept one release, deprecated |
| `ModelOutput.released` | `Model.released` | kept |  |
| `ModelOutput.retires` | `Model.retires` | kept |  |
| `ModelOutput.pricing` | `Model.pricing` | kept |  |
| `ModelOutput.inputTypes` | `Model.inputTypes` | kept |  |
| `ModelOutput.outputTypes` | `Model.outputTypes` | kept |  |

### `ModelParametersInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ModelParametersInput` | — | dropped | nothing filters on `ModelParameters` in v2 |
| `ModelParametersInput.temperature` | — | dropped | follows `ModelParameters.temperature`, which v2 does not filter on |
| `ModelParametersInput.maxOutputTokens` | — | dropped | follows `ModelParameters.replyLimit`, which v2 does not filter on |
| `ModelParametersInput.providerParams` | — | dropped | follows `ModelParameters.providerParameters`, which v2 does not filter on |
| `ModelParametersInput.and` | — | dropped | follows its filter |
| `ModelParametersInput.or` | — | dropped | follows its filter |
| `ModelParametersInput.not` | — | dropped | follows its filter |
| `ModelParametersInput.isNull` | — | dropped | follows its filter |

### `ModelParametersOutput` → `ModelParameters`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ModelParametersOutput` | `ModelParameters` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `ModelParametersOutput.temperature` | `ModelParameters.temperature` | kept |  |
| `ModelParametersOutput.maxOutputTokens` | `ModelParameters.replyLimit` | renamed | a `ReplyTokenLimit`; old name kept one release, deprecated |
| `ModelParametersOutput.providerParams` | `ModelParameters.providerParameters` | renamed | plain English; old name kept one release, deprecated |

### `ModelPricingInput` → `ModelPricingFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ModelPricingInput` | `ModelPricingFilter` | kept |  |
| `ModelPricingInput.inputPerMtok` | `ModelPricingFilter.inputPerMtok` | kept |  |
| `ModelPricingInput.cachedInputPerMtok` | `ModelPricingFilter.cachedInputPerMtok` | kept |  |
| `ModelPricingInput.cacheWritePerMtok` | `ModelPricingFilter.cacheWritePerMtok` | kept |  |
| `ModelPricingInput.outputPerMtok` | `ModelPricingFilter.outputPerMtok` | kept |  |
| `ModelPricingInput.and` | `ModelPricingFilter.and` | kept |  |
| `ModelPricingInput.or` | `ModelPricingFilter.or` | kept |  |
| `ModelPricingInput.not` | `ModelPricingFilter.not` | kept |  |
| `ModelPricingInput.isNull` | `ModelPricingFilter.isNull` | kept |  |

### `ModelPricingOutput` → `ModelPricing`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ModelPricingOutput` | `ModelPricing` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `ModelPricingOutput.inputPerMtok` | `ModelPricing.inputPerMtok` | kept |  |
| `ModelPricingOutput.cachedInputPerMtok` | `ModelPricing.cachedInputPerMtok` | kept |  |
| `ModelPricingOutput.cacheWritePerMtok` | `ModelPricing.cacheWritePerMtok` | kept |  |
| `ModelPricingOutput.outputPerMtok` | `ModelPricing.outputPerMtok` | kept |  |

### `ModelRequestInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ModelRequestInput` | — | dropped | nothing filters on `RequestAssembly` in v2 |
| `ModelRequestInput.captureStatus` | — | dropped | follows `RequestAssembly.body`, which v2 does not filter on |
| `ModelRequestInput.request` | — | dropped | follows `RetainedRequest.json`, which v2 does not filter on |
| `ModelRequestInput.bytes` | — | dropped | follows `ModelRequest.bytes`, which went |
| `ModelRequestInput.sourceContextDigest` | — | dropped | follows `RequestAssembly.assembledFrom`, which v2 does not filter on |
| `ModelRequestInput.parameters` | — | dropped | follows `RequestAssembly.parameters`, which v2 does not filter on |
| `ModelRequestInput.toolCatalogVersion` | — | dropped | follows `RequestAssembly.toolCatalogVersion`, which v2 does not filter on |
| `ModelRequestInput.assemblyVersion` | — | dropped | follows `RequestAssembly.assemblyVersion`, which v2 does not filter on |
| `ModelRequestInput.and` | — | dropped | follows its filter |
| `ModelRequestInput.or` | — | dropped | follows its filter |
| `ModelRequestInput.not` | — | dropped | follows its filter |
| `ModelRequestInput.isNull` | — | dropped | follows its filter |

### `ModelRequestOutput` → `RequestAssembly`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ModelRequestOutput` | `RequestAssembly` | reshaped | how a request was assembled; the captured body is a union |
| `ModelRequestOutput.captureStatus` | `RequestAssembly.body` | reshaped | the `CapturedRequest` union: null is not captured |
| `ModelRequestOutput.request` | `RetainedRequest.json` | reshaped | only a retained body has the request |
| `ModelRequestOutput.bytes` | `RetainedRequest.bytes`, `RedactedRequest.bytes`, `ExpiredRequest.bytes` | reshaped | on each capture state |
| `ModelRequestOutput.sourceContextDigest` | `RequestAssembly.assembledFrom` | reshaped | the `ContextSnapshot` itself; the old fingerprint joined to nothing |
| `ModelRequestOutput.parameters` | `RequestAssembly.parameters` | kept |  |
| `ModelRequestOutput.toolCatalogVersion` | `RequestAssembly.toolCatalogVersion` | kept |  |
| `ModelRequestOutput.assemblyVersion` | `RequestAssembly.assemblyVersion` | kept |  |

### `Mutation`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `Mutation` | `Mutation` | kept |  |
| `Mutation.pauseRun` | `Mutation.pauseRun` | kept |  |
| `Mutation.pauseRun(request:)` | `Mutation.pauseRun(request:)` | kept |  |
| `Mutation.pauseRuns` | `Mutation.pauseRuns` | kept |  |
| `Mutation.pauseRuns(request:)` | `Mutation.pauseRuns(request:)` | kept |  |
| `Mutation.resumeRun` | `Mutation.resumeRun` | kept |  |
| `Mutation.resumeRun(request:)` | `Mutation.resumeRun(request:)` | kept |  |
| `Mutation.resumeRuns` | `Mutation.resumeRuns` | kept |  |
| `Mutation.resumeRuns(request:)` | `Mutation.resumeRuns(request:)` | kept |  |
| `Mutation.cancelRun` | `Mutation.cancelRun` | kept |  |
| `Mutation.cancelRun(request:)` | `Mutation.cancelRun(request:)` | kept |  |
| `Mutation.cancelRuns` | `Mutation.cancelRuns` | kept |  |
| `Mutation.cancelRuns(request:)` | `Mutation.cancelRuns(request:)` | kept |  |
| `Mutation.spawnRun` | `Mutation.spawnRun` | kept |  |
| `Mutation.spawnRun(request:)` | `Mutation.spawnRun(request:)` | kept |  |
| `Mutation.sendMessage` | `Mutation.sendMessage` | kept |  |
| `Mutation.sendMessage(request:)` | `Mutation.sendMessage(request:)` | kept |  |
| `Mutation.deleteRuns` | `Mutation.deleteRuns` | kept |  |
| `Mutation.deleteRuns(request:)` | `Mutation.deleteRuns(request:)` | kept |  |
| `Mutation.createBlueprint` | `Mutation.createBlueprint` | kept |  |
| `Mutation.createBlueprint(request:)` | `Mutation.createBlueprint(request:)` | kept |  |
| `Mutation.updateBlueprint` | `Mutation.updateBlueprint` | kept |  |
| `Mutation.updateBlueprint(request:)` | `Mutation.updateBlueprint(request:)` | kept |  |
| `Mutation.deleteBlueprint` | `Mutation.deleteBlueprint` | kept |  |
| `Mutation.deleteBlueprint(request:)` | `Mutation.deleteBlueprint(request:)` | kept |  |
| `Mutation.answerInteraction` | `Mutation.answerInteraction` | kept |  |
| `Mutation.answerInteraction(request:)` | `Mutation.answerInteraction(request:)` | kept |  |
| `Mutation.startRunExport` | `Mutation.startRunExport` | kept |  |
| `Mutation.startRunExport(request:)` | `Mutation.startRunExport(request:)` | kept |  |
| `Mutation.refreshModels` | `Mutation.refreshModels` | kept |  |
| `Mutation.refreshModels(request:)` | `Mutation.refreshModels(request:)` | kept |  |
| `Mutation.createMcpServer` | `Mutation.createMcpServer` | kept |  |
| `Mutation.createMcpServer(request:)` | `Mutation.createMcpServer(request:)` | kept |  |
| `Mutation.updateMcpServer` | `Mutation.updateMcpServer` | kept |  |
| `Mutation.updateMcpServer(request:)` | `Mutation.updateMcpServer(request:)` | kept |  |
| `Mutation.deleteMcpServer` | `Mutation.deleteMcpServer` | kept |  |
| `Mutation.deleteMcpServer(request:)` | `Mutation.deleteMcpServer(request:)` | kept |  |
| `Mutation.upsertMimeRow` | `Mutation.upsertMimeTypeRule` | renamed | a rule of the mime registry |
| `Mutation.upsertMimeRow(request:)` | `Mutation.upsertMimeTypeRule(request:)` | renamed |  |
| `Mutation.deleteMimeRow` | `Mutation.deleteMimeTypeRule` | renamed | a rule of the mime registry |
| `Mutation.deleteMimeRow(request:)` | `Mutation.deleteMimeTypeRule(request:)` | renamed |  |
| `Mutation.updateConfig` | `Mutation.updateConfig` | kept |  |
| `Mutation.updateConfig(request:)` | `Mutation.updateConfig(request:)` | kept |  |
| `Mutation.upsertScript` | `Mutation.upsertExtension` | renamed | extension, not script |
| `Mutation.upsertScript(request:)` | `Mutation.upsertExtension(request:)` | renamed |  |
| `Mutation.deleteScript` | `Mutation.deleteExtension` | renamed | extension, not script |
| `Mutation.deleteScript(request:)` | `Mutation.deleteExtension(request:)` | renamed |  |
| `Mutation.checkMachine` | `Mutation.checkMachine` | kept |  |
| `Mutation.createDirectory` | `Mutation.createDirectory` | kept |  |
| `Mutation.createDirectory(request:)` | `Mutation.createDirectory(request:)` | kept |  |
| `Mutation.startUpdate` | `Mutation.startUpdate` | kept |  |
| `Mutation.startUpdate(request:)` | `Mutation.startUpdate(request:)` | kept |  |
| `Mutation.signInProvider` | `Mutation.signInProvider` | kept |  |
| `Mutation.signInProvider(request:)` | `Mutation.signInProvider(request:)` | kept |  |
| `Mutation.signOutProvider` | `Mutation.signOutProvider` | kept |  |
| `Mutation.signOutProvider(request:)` | `Mutation.signOutProvider(request:)` | kept |  |
| `Mutation.checkProvider` | `Mutation.checkProviderSignIn` | renamed | it checks a subscription sign-in |
| `Mutation.checkProvider(request:)` | `Mutation.checkProviderSignIn(request:)` | renamed |  |
| `Mutation.checkMcpServer` | `Mutation.checkMcpServer` | kept |  |
| `Mutation.checkMcpServer(request:)` | `Mutation.checkMcpServer(request:)` | kept |  |
| `Mutation.signInMcpServer` | `Mutation.signInMcpServer` | kept |  |
| `Mutation.signInMcpServer(request:)` | `Mutation.signInMcpServer(request:)` | kept |  |
| `Mutation.checkEndpoint` | `Mutation.checkEndpoint` | kept |  |
| `Mutation.checkEndpoint(request:)` | `Mutation.checkEndpoint(request:)` | kept |  |
| `Mutation.upsertYoloProfile` | `Mutation.upsertApprovalPolicy` | renamed | plain English |
| `Mutation.upsertYoloProfile(request:)` | `Mutation.upsertApprovalPolicy(request:)` | renamed |  |
| `Mutation.deleteYoloProfile` | `Mutation.deleteApprovalPolicy` | renamed | plain English |
| `Mutation.deleteYoloProfile(request:)` | `Mutation.deleteApprovalPolicy(request:)` | renamed |  |

### `Node`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `Node` | `Node` | kept |  |
| `Node.id` | `Node.id` | kept |  |

### `NudgeConfigInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `NudgeConfigInput` | — | dropped | nothing filters on `NudgeSettings` in v2 |
| `NudgeConfigInput.policy` | — | dropped | follows `NudgeSettings.enabled`, which v2 does not filter on |
| `NudgeConfigInput.max` | — | dropped | follows `NudgeSettings.max`, which v2 does not filter on |
| `NudgeConfigInput.text` | — | dropped | follows `NudgeSettings.text`, which v2 does not filter on |
| `NudgeConfigInput.and` | — | dropped | follows its filter |
| `NudgeConfigInput.or` | — | dropped | follows its filter |
| `NudgeConfigInput.not` | — | dropped | follows its filter |
| `NudgeConfigInput.isNull` | — | dropped | follows its filter |

### `NudgeConfigOutput` → `NudgeSettings`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `NudgeConfigOutput` | `NudgeSettings` | reshaped | no storage words; `enabled` replaces the policy enum |
| `NudgeConfigOutput.policy` | `NudgeSettings.enabled` | reshaped | a boolean; null inherits |
| `NudgeConfigOutput.max` | `NudgeSettings.max` | kept |  |
| `NudgeConfigOutput.text` | `NudgeSettings.text` | kept |  |

### `NudgePolicy`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `NudgePolicy` | — | dropped | null inherits; `NudgeSettings.enabled` |
| `NudgePolicy.INHERIT` | `NudgeSettings.enabled` null | dropped | a boolean |
| `NudgePolicy.NUDGE` | `NudgeSettings.enabled` true | dropped | a boolean |
| `NudgePolicy.NEVER_NUDGE` | `NudgeSettings.enabled` false | dropped | a boolean |

### `NudgePolicyFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `NudgePolicyFilter` | — | dropped | `NudgePolicy` went |
| `NudgePolicyFilter.eq` | — | dropped | follows `NudgePolicy` |
| `NudgePolicyFilter.ne` | — | dropped | follows `NudgePolicy` |
| `NudgePolicyFilter.in` | — | dropped | follows `NudgePolicy` |
| `NudgePolicyFilter.notIn` | — | dropped | follows `NudgePolicy` |
| `NudgePolicyFilter.isNull` | — | dropped | follows its filter |

### `OrderDirection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `OrderDirection` | `OrderDirection` | kept |  |
| `OrderDirection.ASC` | `OrderDirection.ASC` | kept |  |
| `OrderDirection.DESC` | `OrderDirection.DESC` | kept |  |

### `OutputArtifactInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `OutputArtifactInput` | — | dropped | nothing filters on `ArtifactSlot` in v2 |
| `OutputArtifactInput.name` | — | dropped | follows `ArtifactSlot.name`, which v2 does not filter on |
| `OutputArtifactInput.mimeType` | — | dropped | follows `ArtifactSlot.mimeType`, which v2 does not filter on |
| `OutputArtifactInput.required` | — | dropped | follows `ArtifactSlot.required`, which v2 does not filter on |
| `OutputArtifactInput.description` | — | dropped | follows `ArtifactSlot.description`, which v2 does not filter on |
| `OutputArtifactInput.and` | — | dropped | follows its filter |
| `OutputArtifactInput.or` | — | dropped | follows its filter |
| `OutputArtifactInput.not` | — | dropped | follows its filter |
| `OutputArtifactInput.isNull` | — | dropped | follows its filter |

### `OutputArtifactListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `OutputArtifactListInput` | — | dropped | no v2 listing filters a list of `OutputArtifact` (now `ArtifactSlot`) |
| `OutputArtifactListInput.some` | — | dropped | follows its list filter |
| `OutputArtifactListInput.every` | — | dropped | follows its list filter |
| `OutputArtifactListInput.none` | — | dropped | follows its list filter |
| `OutputArtifactListInput.isNull` | — | dropped | follows its list filter |

### `OutputArtifactOutput` → `ArtifactSlot`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `OutputArtifactOutput` | `ArtifactSlot` | renamed | a slot the answer fills, not a run's artifact |
| `OutputArtifactOutput.name` | `ArtifactSlot.name` | kept |  |
| `OutputArtifactOutput.mimeType` | `ArtifactSlot.mimeType` | kept |  |
| `OutputArtifactOutput.required` | `ArtifactSlot.required` | kept |  |
| `OutputArtifactOutput.description` | `ArtifactSlot.description` | kept |  |

### `OutputRequestWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `OutputRequestWrite` | `OutputRequestWrite` | kept |  |
| `OutputRequestWrite.format` | `OutputRequestWrite.format` | kept |  |
| `OutputRequestWrite.instructions` | `OutputRequestWrite.instructions` | kept |  |

### `OutputRequirementInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `OutputRequirementInput` | — | dropped | `OutputRequirement` went |
| `OutputRequirementInput.reasks` | — | dropped | follows `OutputRequirement` |
| `OutputRequirementInput.and` | — | dropped | follows its filter |
| `OutputRequirementInput.or` | — | dropped | follows its filter |
| `OutputRequirementInput.not` | — | dropped | follows its filter |
| `OutputRequirementInput.isNull` | — | dropped | follows its filter |

### `OutputRequirementOutput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `OutputRequirementOutput` | — | dropped | `reasks` was always 3; `Stage.outputRequired` |
| `OutputRequirementOutput.reasks` | `Stage.outputRequired` | dropped | always the constant 3 |

### `OutputRouteInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `OutputRouteInput` | — | dropped | nothing filters on `PartRoute` in v2 |
| `OutputRouteInput.pattern` | — | dropped | follows `PartRoute.mimePattern`, which v2 does not filter on |
| `OutputRouteInput.region` | — | dropped | follows `PartRoute.region`, which v2 does not filter on |
| `OutputRouteInput.regionName` | — | dropped | follows `PartRoute.region`, which v2 does not filter on |
| `OutputRouteInput.and` | — | dropped | follows its filter |
| `OutputRouteInput.or` | — | dropped | follows its filter |
| `OutputRouteInput.not` | — | dropped | follows its filter |
| `OutputRouteInput.isNull` | — | dropped | follows its filter |

### `OutputRouteListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `OutputRouteListInput` | — | dropped | no v2 listing filters a list of `OutputRoute` (now `PartRoute`) |
| `OutputRouteListInput.some` | — | dropped | follows its list filter |
| `OutputRouteListInput.every` | — | dropped | follows its list filter |
| `OutputRouteListInput.none` | — | dropped | follows its list filter |
| `OutputRouteListInput.isNull` | — | dropped | follows its list filter |

### `OutputRouteOutput` → `PartRoute`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `OutputRouteOutput` | `PartRoute` | reshaped | it routes parts, not the output |
| `OutputRouteOutput.pattern` | `PartRoute.mimePattern` | renamed | says what it matches; old name kept one release, deprecated |
| `OutputRouteOutput.region` | `PartRoute.region` | reshaped | never null |
| `OutputRouteOutput.regionName` | `PartRoute.region` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |

### `OutputSpecInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `OutputSpecInput` | — | dropped | nothing filters on `OutputShape` in v2 |
| `OutputSpecInput.format` | — | dropped | follows `OutputShape.format`, which v2 does not filter on |
| `OutputSpecInput.instructions` | — | dropped | follows `OutputShape.instructions`, which v2 does not filter on |
| `OutputSpecInput.example` | — | dropped | follows `OutputShape.example`, which v2 does not filter on |
| `OutputSpecInput.schema` | — | dropped | follows `OutputShape.jsonSchema`, which v2 does not filter on |
| `OutputSpecInput.validator` | — | dropped | follows `OutputShape.validator`, which v2 does not filter on |
| `OutputSpecInput.onValidatorError` | — | dropped | follows `OutputValidatorSetting.whenScriptFails`, which v2 does not filter on |
| `OutputSpecInput.overwriteArtifacts` | — | dropped | follows `OutputShape.overwritesArtifacts`, which v2 does not filter on |
| `OutputSpecInput.artifacts` | — | dropped | follows `OutputShape.artifacts`, which v2 does not filter on |
| `OutputSpecInput.and` | — | dropped | follows its filter |
| `OutputSpecInput.or` | — | dropped | follows its filter |
| `OutputSpecInput.not` | — | dropped | follows its filter |
| `OutputSpecInput.isNull` | — | dropped | follows its filter |

### `OutputSpecOutput` → `OutputShape`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `OutputSpecOutput` | `OutputShape` | reshaped | the validator and its failure policy nest |
| `OutputSpecOutput.format` | `OutputShape.format` | kept |  |
| `OutputSpecOutput.instructions` | `OutputShape.instructions` | kept |  |
| `OutputSpecOutput.example` | `OutputShape.example` | kept |  |
| `OutputSpecOutput.schema` | `OutputShape.jsonSchema` | renamed | says which schema; old name kept one release, deprecated |
| `OutputSpecOutput.validator` | `OutputShape.validator` | reshaped | an `OutputValidatorSetting`: the script as the revision holds it, with its failure policy |
| `OutputSpecOutput.onValidatorError` | `OutputValidatorSetting.whenScriptFails` | reshaped | nested with the validator it goes with |
| `OutputSpecOutput.overwriteArtifacts` | `OutputShape.overwritesArtifacts` | renamed | a boolean reads as a verb; old name kept one release, deprecated |
| `OutputSpecOutput.artifacts` | `OutputShape.artifacts` | kept |  |

### `PauseRunRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `PauseRunRequest` | `PauseRunRequest` | kept |  |
| `PauseRunRequest.id` | `PauseRunRequest.id` | kept |  |

### `PauseRunResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `PauseRunResult` | `PauseRunResult` | kept |  |
| `PauseRunResult.run` | `PauseRunResult.run` | kept |  |
| `PauseRunResult.warnings` | `PauseRunResult.warnings` | kept |  |

### `PauseRunsRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `PauseRunsRequest` | `PauseRunsRequest` | kept |  |
| `PauseRunsRequest.filter` | `PauseRunsRequest.filter` | kept |  |

### `PauseRunsResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `PauseRunsResult` | `PauseRunsResult` | kept |  |
| `PauseRunsResult.runs` | `PauseRunsResult.outcomes` | dropped | deprecated; each run answers as a `SweepItem` type |
| `PauseRunsResult.skipped` | `PauseRunsResult.outcomes` | dropped | deprecated; `SweepRefused`, `SweepAlreadyInState` and the rest |

### `PerByteInput` → `TokensPerByteFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `PerByteInput` | `TokensPerByteFilter` | renamed | follows `TokensPerByte` |
| `PerByteInput.tokensPerByte` | `TokensPerByteFilter.tokensPerByte` | renamed | follows `TokensPerByte.tokensPerByte` |
| `PerByteInput.and` | `TokensPerByteFilter.and` | reshaped | now `[TokensPerByteFilter]` |
| `PerByteInput.or` | `TokensPerByteFilter.or` | reshaped | now `[TokensPerByteFilter]` |
| `PerByteInput.not` | `TokensPerByteFilter.not` | reshaped | now `TokensPerByteFilter` |
| `PerByteInput.isNull` | `TokensPerByteFilter.isNull` | kept |  |

### `PerByteOutput` → `TokensPerByte`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `PerByteOutput` | `TokensPerByte` | renamed | says what it counts |
| `PerByteOutput.tokensPerByte` | `TokensPerByte.tokensPerByte` | kept |  |

### `PerPageInput` → `TokensPerPageFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `PerPageInput` | `TokensPerPageFilter` | renamed | follows `TokensPerPage` |
| `PerPageInput.tokensPerPage` | `TokensPerPageFilter.tokensPerPage` | renamed | follows `TokensPerPage.tokensPerPage` |
| `PerPageInput.and` | `TokensPerPageFilter.and` | reshaped | now `[TokensPerPageFilter]` |
| `PerPageInput.or` | `TokensPerPageFilter.or` | reshaped | now `[TokensPerPageFilter]` |
| `PerPageInput.not` | `TokensPerPageFilter.not` | reshaped | now `TokensPerPageFilter` |
| `PerPageInput.isNull` | `TokensPerPageFilter.isNull` | kept |  |

### `PerPageOutput` → `TokensPerPage`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `PerPageOutput` | `TokensPerPage` | renamed | says what it counts |
| `PerPageOutput.tokensPerPage` | `TokensPerPage.tokensPerPage` | kept |  |

### `PerPixelInput` → `TokensPerPixelFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `PerPixelInput` | `TokensPerPixelFilter` | renamed | follows `TokensPerPixel` |
| `PerPixelInput.pixelsPerToken` | `TokensPerPixelFilter.pixelsPerToken` | renamed | follows `TokensPerPixel.pixelsPerToken` |
| `PerPixelInput.max` | `TokensPerPixelFilter.max` | renamed | follows `TokensPerPixel.max` |
| `PerPixelInput.and` | `TokensPerPixelFilter.and` | reshaped | now `[TokensPerPixelFilter]` |
| `PerPixelInput.or` | `TokensPerPixelFilter.or` | reshaped | now `[TokensPerPixelFilter]` |
| `PerPixelInput.not` | `TokensPerPixelFilter.not` | reshaped | now `TokensPerPixelFilter` |
| `PerPixelInput.isNull` | `TokensPerPixelFilter.isNull` | kept |  |

### `PerPixelOutput` → `TokensPerPixel`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `PerPixelOutput` | `TokensPerPixel` | renamed | says what it counts |
| `PerPixelOutput.pixelsPerToken` | `TokensPerPixel.pixelsPerToken` | kept |  |
| `PerPixelOutput.max` | `TokensPerPixel.max` | kept |  |

### `PerPixelWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `PerPixelWrite` | `PerPixelWrite` | kept |  |
| `PerPixelWrite.pixelsPerToken` | `PerPixelWrite.pixelsPerToken` | kept |  |
| `PerPixelWrite.max` | `PerPixelWrite.max` | kept |  |

### `PerSecondInput` → `TokensPerSecondFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `PerSecondInput` | `TokensPerSecondFilter` | renamed | follows `TokensPerSecond` |
| `PerSecondInput.tokensPerSecond` | `TokensPerSecondFilter.tokensPerSecond` | renamed | follows `TokensPerSecond.tokensPerSecond` |
| `PerSecondInput.and` | `TokensPerSecondFilter.and` | reshaped | now `[TokensPerSecondFilter]` |
| `PerSecondInput.or` | `TokensPerSecondFilter.or` | reshaped | now `[TokensPerSecondFilter]` |
| `PerSecondInput.not` | `TokensPerSecondFilter.not` | reshaped | now `TokensPerSecondFilter` |
| `PerSecondInput.isNull` | `TokensPerSecondFilter.isNull` | kept |  |

### `PerSecondOutput` → `TokensPerSecond`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `PerSecondOutput` | `TokensPerSecond` | renamed | says what it counts |
| `PerSecondOutput.tokensPerSecond` | `TokensPerSecond.tokensPerSecond` | kept |  |

### `PresentForReviewArgsOutput` → `PresentForReviewArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `PresentForReviewArgsOutput` | `PresentForReviewArguments` | renamed | argument types are named `XArguments` |
| `PresentForReviewArgsOutput.title` | `PresentForReviewArguments.title` | kept |  |
| `PresentForReviewArgsOutput.markdown` | `PresentForReviewArguments.markdown` | kept |  |

### `PresentForReviewCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `PresentForReviewCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `PresentForReviewCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `PresentForReviewCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `PresentForReviewCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `PresentForReviewCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `PresentForReviewArguments` for `present_for_review` |

### `ProviderAuthKind`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ProviderAuthKind` | — | dropped | the `ModelProvider` implementer is how it authenticates |
| `ProviderAuthKind.API_KEY` | `ApiKeyProvider` | reshaped | a type each |
| `ProviderAuthKind.SIGN_IN` | `SubscriptionProvider` | reshaped | a type each |
| `ProviderAuthKind.NONE` | `LocalProvider`, `CommandLineProvider` | dropped | a type each |

### `ProviderConfigOutput` → `ModelProvider`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ProviderConfigOutput` | `ModelProvider` | merged | merged into the `ModelProvider` implementers (`ApiKeyProvider`, `LocalProvider`, `SubscriptionProvider`, ...) |
| `ProviderConfigOutput.id` | `ModelProvider.id` | reshaped | a `Node` id, `modelProvider:<name>` |
| `ProviderConfigOutput.name` | `ModelProvider.displayName` | renamed | `name` is now the config key |
| `ProviderConfigOutput.auth` | `__typename` | dropped | the `ModelProvider` type says how it authenticates |
| `ProviderConfigOutput.isEnabled` | `ModelProvider.isEnabled` | kept |  |
| `ProviderConfigOutput.hasKey` | `ApiKeyProvider.hasApiKey` | reshaped | only key providers have one |
| `ProviderConfigOutput.baseUrl` | `ApiKeyProvider.baseUrl`, `LocalProvider.baseUrl` | reshaped | now reports `<name>_base_url` too |
| `ProviderConfigOutput.region` | `BedrockSettings.region` | reshaped | only Bedrock has regions |
| `ProviderConfigOutput.options` | `ApiKeyProvider.settings`, `SubscriptionProvider.settings` | reshaped | `ProviderSettings` |

### `ProviderConfigWrite` → `ModelProviderWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ProviderConfigWrite` | `ModelProviderWrite` | reshaped | one `@oneOf` field per kind of provider, so a write carries only settings that provider has |
| `ProviderConfigWrite.provider` | `ModelProviderWrite` | reshaped | the `@oneOf` field names the kind; each write names its provider |
| `ProviderConfigWrite.key` | `ApiKeyProviderWrite.apiKey` | reshaped | a `StringSetting`: set or clear in one field |
| `ProviderConfigWrite.clearKey` | `StringSetting.clear` | merged | said the opposite of `key` |
| `ProviderConfigWrite.isEnabled` | `LocalProviderWrite.isEnabled`, `SubscriptionProviderWrite.isEnabled`, `CommandLineProviderWrite.isEnabled` | reshaped | on providers that have a switch |
| `ProviderConfigWrite.baseUrl` | `ApiKeyProviderWrite.baseUrl`, `LocalProviderWrite.baseUrl` | reshaped | on providers that have an address |
| `ProviderConfigWrite.region` | `BedrockSettingsWrite.region` | reshaped | Bedrock only |
| `ProviderConfigWrite.codex` | `SubscriptionProviderWrite.codex` | reshaped | Codex only |

### `ProviderConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ProviderConnection` | — | dropped | a plain list in v2, bounded by what holds it |
| `ProviderConnection.results` | — | dropped | follows its connection |
| `ProviderConnection.cursor` | — | dropped | follows its connection |
| `ProviderConnection.total` | — | dropped | follows its connection |

### `ProviderInput` → `SubscriptionProviderFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ProviderInput` | `SubscriptionProviderFilter` | renamed | follows `SubscriptionProvider` |
| `ProviderInput.id` | `SubscriptionProviderFilter.id` | renamed | follows `SubscriptionProvider.id` |
| `ProviderInput.name` | `SubscriptionProviderFilter.name` | renamed | follows `SubscriptionProvider.name` |
| `ProviderInput.display` | `ModelProviderFilter.displayName` | renamed | follows `ModelProvider.displayName` |
| `ProviderInput.enabled` | `ModelProviderFilter.isEnabled` | renamed | follows `ModelProvider.isEnabled` |
| `ProviderInput.signedIn` | — | dropped | follows `SubscriptionProvider.signIn`, which v2 does not filter on |
| `ProviderInput.account` | — | dropped | follows `ProviderSignIn.account`, which v2 does not filter on |
| `ProviderInput.plan` | — | dropped | follows `ProviderSignIn.plan`, which v2 does not filter on |
| `ProviderInput.expiresAt` | — | dropped | follows `ProviderSignIn.expiresAt`, which v2 does not filter on |
| `ProviderInput.and` | `SubscriptionProviderFilter.and` | reshaped | now `[SubscriptionProviderFilter]` |
| `ProviderInput.or` | `SubscriptionProviderFilter.or` | reshaped | now `[SubscriptionProviderFilter]` |
| `ProviderInput.not` | `SubscriptionProviderFilter.not` | reshaped | now `SubscriptionProviderFilter` |
| `ProviderInput.isNull` | `SubscriptionProviderFilter.isNull` | kept |  |

### `ProviderOptions` → `ProviderSettings`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ProviderOptions` | `ProviderSettings` | renamed | settings, not options; now also Bedrock's |
| `ProviderOptions.CodexOptionsOutput` | `ProviderSettings.CodexSettings` | renamed |  |

### `ProviderOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ProviderOrder` | — | dropped | the listing it sorted went or became a plain list |
| `ProviderOrder.field` | — | dropped | the listing it sorted went or became a plain list |
| `ProviderOrder.direction` | — | dropped | the listing it sorted went or became a plain list |

### `ProviderOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ProviderOrderField` | — | dropped | the listing it sorted went or became a plain list |
| `ProviderOrderField.ID` | — | dropped | not a sort key in v2 |
| `ProviderOrderField.NAME` | — | dropped | not a sort key in v2 |
| `ProviderOrderField.DISPLAY` | — | dropped | not a sort key in v2 |

### `ProviderOutput` → `SubscriptionProvider`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ProviderOutput` | `SubscriptionProvider` | reshaped | one `ModelProvider` interface covers every provider; this was only the subscription ones |
| `ProviderOutput.id` | `SubscriptionProvider.id` | reshaped | `modelProvider:<name>` |
| `ProviderOutput.name` | `SubscriptionProvider.name` | kept |  |
| `ProviderOutput.display` | `ModelProvider.displayName` | renamed | plain English |
| `ProviderOutput.enabled` | `ModelProvider.isEnabled` | renamed | a boolean reads `is...` |
| `ProviderOutput.signedIn` | `SubscriptionProvider.signIn` | reshaped | the `ProviderSignIn`, null when signed out |
| `ProviderOutput.account` | `ProviderSignIn.account` | merged | on the sign-in |
| `ProviderOutput.plan` | `ProviderSignIn.plan` | merged | on the sign-in |
| `ProviderOutput.expiresAt` | `ProviderSignIn.expiresAt` | merged | on the sign-in |

### `Query`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `Query` | `Query` | kept |  |
| `Query.blueprints` | `Query.blueprints` | kept |  |
| `Query.blueprints(filter:)` | `Query.blueprints(filter:)` | kept |  |
| `Query.blueprints(orderBy:)` | `Query.blueprints(orderBy:)` | kept |  |
| `Query.blueprints(first:)` | `Query.blueprints(first:)` | kept |  |
| `Query.blueprints(after:)` | `Query.blueprints(after:)` | kept |  |
| `Query.blueprint` | `Query.blueprint` | kept |  |
| `Query.blueprint(name:)` | `Query.blueprint(name:)` | kept |  |
| `Query.config` | `Query.config` | kept |  |
| `Query.doctor` | `Query.diagnostics` | renamed | plain English; old name kept one release, deprecated |
| `Query.mcpServers` | `Query.mcpServers` | kept |  |
| `Query.mcpServers(filter:)` | `Query.mcpServers(filter:)` | kept |  |
| `Query.mcpServers(orderBy:)` | `Query.mcpServers(orderBy:)` | kept |  |
| `Query.mcpServers(first:)` | `Query.mcpServers(first:)` | kept |  |
| `Query.mcpServers(after:)` | `Query.mcpServers(after:)` | kept |  |
| `Query.mcpServer` | `Query.mcpServer` | kept |  |
| `Query.mcpServer(name:)` | `Query.mcpServer(name:)` | kept |  |
| `Query.yoloProfiles` | `Query.approvalPolicies` | reshaped | now `ApprovalPolicyConnection`; plain English |
| `Query.yoloProfiles(filter:)` | `Query.approvalPolicies(filter:)` | reshaped | now `ApprovalPolicyFilter` |
| `Query.yoloProfiles(orderBy:)` | `Query.approvalPolicies(orderBy:)` | reshaped | now `[ApprovalPolicyOrder]` |
| `Query.yoloProfiles(first:)` | `Query.approvalPolicies(first:)` | renamed |  |
| `Query.yoloProfiles(after:)` | `Query.approvalPolicies(after:)` | renamed |  |
| `Query.yoloProfile` | `Query.approvalPolicy` | renamed | plain English; old name kept one release, deprecated |
| `Query.yoloProfile(name:)` | `Query.approvalPolicy(name:)` | kept |  |
| `Query.mimeRows` | `Query.mimeTypes` | reshaped | now `MimeTypeConnection`; the effective registry, by type |
| `Query.mimeRows(filter:)` | `Query.mimeTypes(filter:)` | reshaped | now `MimeTypeFilter` |
| `Query.mimeRows(orderBy:)` | `Query.mimeTypes(orderBy:)` | reshaped | now `[MimeTypeOrder]` |
| `Query.mimeRows(first:)` | `Query.mimeTypes(first:)` | renamed |  |
| `Query.mimeRows(after:)` | `Query.mimeTypes(after:)` | renamed |  |
| `Query.scripts` | `Query.extensions` | reshaped | now `ExtensionConnection`; extension, not script |
| `Query.scripts(filter:)` | `Query.extensions(filter:)` | reshaped | now `ExtensionFilter` |
| `Query.scripts(orderBy:)` | `Query.extensions(orderBy:)` | reshaped | now `[ExtensionOrder]` |
| `Query.scripts(first:)` | `Query.extensions(first:)` | renamed |  |
| `Query.scripts(after:)` | `Query.extensions(after:)` | renamed |  |
| `Query.script` | `Query.extension` | reshaped | by `ExtensionRef` |
| `Query.script(ref:)` | `Query.extension(ref:)` | reshaped | an `ExtensionRef` |
| `Query.directory` | `Query.directory` | kept |  |
| `Query.directory(path:)` | `Query.directory(path:)` | kept |  |
| `Query.directory(includeHidden:)` | `Directory.subdirectories(includeHidden:)` | reshaped | belongs to the listing |
| `Query.models` | `Query.models` | kept |  |
| `Query.models(filter:)` | `Query.models(filter:)` | kept |  |
| `Query.models(orderBy:)` | `Query.models(orderBy:)` | kept |  |
| `Query.models(first:)` | `Query.models(first:)` | kept |  |
| `Query.models(after:)` | `Query.models(after:)` | kept |  |
| `Query.model` | `Query.model` | kept |  |
| `Query.model(id:)` | `Query.model(id:)` | kept |  |
| `Query.providers` | `Query.modelProviders` | reshaped | every provider, not only the subscription ones |
| `Query.providers(filter:)` | `Query.modelProviders(filter:)` | reshaped | now `ModelProviderFilter` |
| `Query.providers(orderBy:)` | `Query.modelProviders(orderBy:)` | reshaped | now `[ModelProviderOrder]` |
| `Query.providers(first:)` | `Query.modelProviders(first:)` | renamed |  |
| `Query.providers(after:)` | `Query.modelProviders(after:)` | renamed |  |
| `Query.provider` | `Query.modelProvider` | reshaped | every provider |
| `Query.provider(name:)` | `Query.modelProvider(name:)` | kept |  |
| `Query.tools` | `Query.tools` | kept |  |
| `Query.tools(filter:)` | `Query.tools(filter:)` | kept |  |
| `Query.tools(orderBy:)` | `Query.tools(orderBy:)` | kept |  |
| `Query.tools(first:)` | `Query.tools(first:)` | kept |  |
| `Query.tools(after:)` | `Query.tools(after:)` | kept |  |
| `Query.toolGroups` | `AllTools` | dropped | each group is its own `ToolSelector` type (`AllBuiltinTools`, `AllSubagentTools`, `AllCustomTools`, `AllMcpTools`, `AllTools`) |
| `Query.daemon` | `Query.daemon` | kept |  |
| `Query.updatePlan` | `Query.updatePlan` | kept |  |
| `Query.node` | `Query.node` | kept |  |
| `Query.node(id:)` | `Query.node(id:)` | kept |  |
| `Query.nodes` | `Query.nodes` | kept |  |
| `Query.nodes(ids:)` | `Query.nodes(ids:)` | kept |  |
| `Query.updateJob` | `Query.updateJob` | kept |  |
| `Query.updateJob(id:)` | `Query.updateJob(id:)` | kept |  |
| `Query.updateJobs` | `Query.updateJobs` | kept |  |
| `Query.updateJobs(filter:)` | `Query.updateJobs(filter:)` | kept |  |
| `Query.updateJobs(orderBy:)` | `Query.updateJobs(orderBy:)` | kept |  |
| `Query.updateJobs(first:)` | `Query.updateJobs(first:)` | kept |  |
| `Query.updateJobs(after:)` | `Query.updateJobs(after:)` | kept |  |
| `Query.runExport` | `Query.runExport` | kept |  |
| `Query.runExport(id:)` | `Query.runExport(id:)` | kept |  |
| `Query.openInteractions` | `Query.openInteractions` | kept |  |
| `Query.openInteractions(filter:)` | `Query.openInteractions(filter:)` | kept |  |
| `Query.openInteractions(orderBy:)` | `Query.openInteractions(orderBy:)` | kept |  |
| `Query.openInteractions(first:)` | `Query.openInteractions(first:)` | kept |  |
| `Query.openInteractions(after:)` | `Query.openInteractions(after:)` | kept |  |
| `Query.runs` | `Query.runs` | kept |  |
| `Query.runs(filter:)` | `Query.runs(filter:)` | kept |  |
| `Query.runs(search:)` | `Query.runs(search:)` | kept |  |
| `Query.runs(orderBy:)` | `Query.runs(orderBy:)` | kept |  |
| `Query.runs(first:)` | `Query.runs(first:)` | kept |  |
| `Query.runs(after:)` | `Query.runs(after:)` | kept |  |
| `Query.run` | `Query.run` | kept |  |
| `Query.run(id:)` | `Query.run(id:)` | kept |  |
| `Query.serverTime` | `Server.time` | reshaped | on `Query.server`; the server's clock, not the daemon's |
| `Query.validateBlueprint` | `Query.validateBlueprint` | kept |  |
| `Query.validateBlueprint(manifest:)` | `Query.validateBlueprint(manifest:)` | kept |  |
| `Query.validateBlueprint(as:)` | `Query.validateBlueprint(as:)` | kept |  |
| `Query.validateProviderKey` | `Query.validateProviderKey` | kept |  |
| `Query.validateProviderKey(provider:)` | `Query.validateProviderKey(providerName:)` | renamed | a name |
| `Query.validateProviderKey(key:)` | `Query.validateProviderKey(key:)` | kept |  |
| `Query.validateProviderKey(baseUrl:)` | `Query.validateProviderKey(baseUrl:)` | kept |  |
| `Query.validateScript` | `Query.validateExtension` | reshaped | an `ExtensionDraft` per extension point; answers `Validation` |
| `Query.validateScript(kind:)` | `Query.validateExtension(draft:)` | reshaped | the `ExtensionDraft` field chosen is the kind |
| `Query.validateScript(content:)` | `Query.validateExtension(draft:)` | reshaped | the text, in the draft |
| `Query.validateScript(requiredHooks:)` | `StageHookDraft.points` | reshaped | stage hooks only, typed as `StageHookPoint`s |

### `ReadFileArgsOutput` → `ReadFileArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ReadFileArgsOutput` | `ReadFileArguments` | renamed | argument types are named `XArguments` |
| `ReadFileArgsOutput.path` | `ReadFileArguments.path` | kept |  |

### `ReadFileCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ReadFileCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `ReadFileCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `ReadFileCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `ReadFileCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `ReadFileCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `ReadFileArguments` for `read_file` |

### `ReadFilesArgsOutput` → `ReadFilesArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ReadFilesArgsOutput` | `ReadFilesArguments` | renamed | argument types are named `XArguments` |
| `ReadFilesArgsOutput.paths` | `ReadFilesArguments.paths` | kept |  |

### `ReadFilesCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ReadFilesCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `ReadFilesCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `ReadFilesCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `ReadFilesCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `ReadFilesCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `ReadFilesArguments` for `read_files` |

### `RefreshModelsRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RefreshModelsRequest` | `RefreshModelsRequest` | kept |  |
| `RefreshModelsRequest.provider` | `RefreshModelsRequest.providerName` | renamed | a name |

### `RefreshModelsResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RefreshModelsResult` | `RefreshModelsResult` | kept |  |
| `RefreshModelsResult.models` | `RefreshModelsResult.models` | kept |  |

### `RegionAdmission`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionAdmission` | `RegionAdmission` | kept |  |
| `RegionAdmission.EVICT` | `RegionAdmission.EVICT` | kept |  |
| `RegionAdmission.REJECT` | `RegionAdmission.REJECT` | kept |  |

### `RegionAdmissionFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionAdmissionFilter` | — | dropped | nothing filters on `RegionAdmission` in v2 |
| `RegionAdmissionFilter.eq` | — | dropped | follows `RegionAdmission` |
| `RegionAdmissionFilter.ne` | — | dropped | follows `RegionAdmission` |
| `RegionAdmissionFilter.in` | — | dropped | follows `RegionAdmission` |
| `RegionAdmissionFilter.notIn` | — | dropped | follows `RegionAdmission` |
| `RegionAdmissionFilter.isNull` | — | dropped | follows its filter |

### `RegionEntryRequirementInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionEntryRequirementInput` | — | dropped | nothing filters on `RegionEntryRequirement` in v2 |
| `RegionEntryRequirementInput.region` | — | dropped | follows `RegionEntryRequirement.region`, which v2 does not filter on |
| `RegionEntryRequirementInput.regionName` | — | dropped | follows `RegionEntryRequirement.region`, which v2 does not filter on |
| `RegionEntryRequirementInput.atLeast` | — | dropped | follows `RegionEntryRequirement.atLeast`, which v2 does not filter on |
| `RegionEntryRequirementInput.and` | — | dropped | follows its filter |
| `RegionEntryRequirementInput.or` | — | dropped | follows its filter |
| `RegionEntryRequirementInput.not` | — | dropped | follows its filter |
| `RegionEntryRequirementInput.isNull` | — | dropped | follows its filter |

### `RegionEntryRequirementOutput` → `RegionEntryRequirement`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionEntryRequirementOutput` | `RegionEntryRequirement` | reshaped | name twin dropped |
| `RegionEntryRequirementOutput.region` | `RegionEntryRequirement.region` | reshaped | never null |
| `RegionEntryRequirementOutput.regionName` | `RegionEntryRequirement.region` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `RegionEntryRequirementOutput.atLeast` | `RegionEntryRequirement.atLeast` | kept |  |

### `RegionInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionInput` | — | dropped | nothing filters on `DeclaredRegion` in v2 |
| `RegionInput.name` | — | dropped | follows `Region.name`, which v2 does not filter on |
| `RegionInput.declaredByStage` | — | dropped | follows `DeclaredRegion.stage`, which v2 does not filter on |
| `RegionInput.kind` | — | dropped | follows `Region.kind`, which went |
| `RegionInput.maxTokens` | — | dropped | follows `DeclaredRegion.budget`, which v2 does not filter on |
| `RegionInput.budgetPercent` | — | dropped | follows `WindowShareBudget.percent`, which v2 does not filter on |
| `RegionInput.minTokens` | — | dropped | follows `WindowShareBudget.minTokens`, which v2 does not filter on |
| `RegionInput.budgetMaxTokens` | — | dropped | follows `WindowShareBudget.maxTokens`, which v2 does not filter on |
| `RegionInput.description` | — | dropped | follows `DeclaredRegion.description`, which v2 does not filter on |
| `RegionInput.required` | — | dropped | follows `DeclaredRegion.required`, which v2 does not filter on |
| `RegionInput.requiredMessage` | — | dropped | follows `RegionRequirement.message`, which v2 does not filter on |
| `RegionInput.describeInPrompt` | — | dropped | follows `DeclaredRegion.describedToModel`, which v2 does not filter on |
| `RegionInput.summarizable` | — | dropped | follows `DeclaredRegion.summarizable`, which v2 does not filter on |
| `RegionInput.volatility` | — | dropped | follows `DeclaredRegion.volatility`, which v2 does not filter on |
| `RegionInput.admission` | — | dropped | follows `DeclaredRegion.admission`, which v2 does not filter on |
| `RegionInput.compactAt` | — | dropped | follows `CompactingRegion.compactAtPercent`, which v2 does not filter on |
| `RegionInput.accepts` | — | dropped | follows `DeclaredRegion.accepts`, which v2 does not filter on |
| `RegionInput.seed` | — | dropped | follows `DeclaredRegion.seed`, which v2 does not filter on |
| `RegionInput.maxItems` | — | dropped | follows `SlidingWindowRegion.maxItems`, which v2 does not filter on |
| `RegionInput.strategy` | — | dropped | follows `SlidingWindowRegion.batchEviction`, which v2 does not filter on |
| `RegionInput.overflow` | — | dropped | follows `DropInBatches.overflow`, which v2 does not filter on |
| `RegionInput.compactCount` | — | dropped | follows `SummarizeInBatches.count`, which v2 does not filter on |
| `RegionInput.thresholdTokens` | — | dropped | follows `CompactingRegion.compactAtTokens`, which v2 does not filter on |
| `RegionInput.sourceRegion` | — | dropped | follows `CompactHistoryRegion.source`, which v2 does not filter on |
| `RegionInput.sourceRegionName` | — | dropped | follows `CompactHistoryRegion.source`, which v2 does not filter on |
| `RegionInput.maxEntries` | — | dropped | follows `KeyValueRegion.maxEntries`, which v2 does not filter on |
| `RegionInput.script` | — | dropped | follows `ScriptedRegion.hook`, which v2 does not filter on |
| `RegionInput.pinned` | — | dropped | follows `ScriptedRegion.neverEvicted`, which v2 does not filter on |
| `RegionInput.and` | — | dropped | follows its filter |
| `RegionInput.or` | — | dropped | follows its filter |
| `RegionInput.not` | — | dropped | follows its filter |
| `RegionInput.isNull` | — | dropped | follows its filter |

### `RegionKind`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionKind` | — | dropped | the declared `Region` type is the kind (`ContextRegion.definition`) |
| `RegionKind.PINNED` | `PinnedRegion` | reshaped | a `DeclaredRegion` type per kind |
| `RegionKind.TEMPORARY` | `TemporaryRegion` | reshaped | a `DeclaredRegion` type per kind |
| `RegionKind.CLEARABLE` | `ClearableRegion` | reshaped | a `DeclaredRegion` type per kind |
| `RegionKind.SLIDING_WINDOW` | `SlidingWindowRegion` | reshaped | a `DeclaredRegion` type per kind |
| `RegionKind.COMPACTING` | `CompactingRegion` | reshaped | a `DeclaredRegion` type per kind |
| `RegionKind.COMPACT_HISTORY` | `CompactHistoryRegion` | reshaped | a `DeclaredRegion` type per kind |
| `RegionKind.HASHMAP` | `KeyValueRegion` | reshaped | a `DeclaredRegion` type per kind |
| `RegionKind.CHECKLIST` | `ChecklistRegion` | reshaped | a `DeclaredRegion` type per kind |
| `RegionKind.CUSTOM` | `ScriptedRegion` | reshaped | a `DeclaredRegion` type per kind |

### `RegionKindFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionKindFilter` | — | dropped | `RegionKind` went |
| `RegionKindFilter.eq` | — | dropped | follows `RegionKind` |
| `RegionKindFilter.ne` | — | dropped | follows `RegionKind` |
| `RegionKindFilter.in` | — | dropped | follows `RegionKind` |
| `RegionKindFilter.notIn` | — | dropped | follows `RegionKind` |
| `RegionKindFilter.isNull` | — | dropped | follows its filter |

### `RegionListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionListInput` | — | dropped | no v2 listing filters a list of `Region` (now `DeclaredRegion`) |
| `RegionListInput.some` | — | dropped | follows its list filter |
| `RegionListInput.every` | — | dropped | follows its list filter |
| `RegionListInput.none` | — | dropped | follows its list filter |
| `RegionListInput.isNull` | — | dropped | follows its list filter |

### `RegionMappingInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionMappingInput` | — | dropped | nothing filters on `RegionHandoff` in v2 |
| `RegionMappingInput.fromRegion` | — | dropped | follows `RegionHandoff.fromRegionName`, which v2 does not filter on |
| `RegionMappingInput.toRegion` | — | dropped | follows `RegionHandoff.toRegionName`, which v2 does not filter on |
| `RegionMappingInput.transform` | — | dropped | follows `RegionMapping.transform`, which went |
| `RegionMappingInput.fields` | — | dropped | follows `ExtractedRegion.fields`, which v2 does not filter on |
| `RegionMappingInput.and` | — | dropped | follows its filter |
| `RegionMappingInput.or` | — | dropped | follows its filter |
| `RegionMappingInput.not` | — | dropped | follows its filter |
| `RegionMappingInput.isNull` | — | dropped | follows its filter |

### `RegionMappingListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionMappingListInput` | — | dropped | no v2 listing filters a list of `RegionMapping` (now `RegionHandoff`) |
| `RegionMappingListInput.some` | — | dropped | follows its list filter |
| `RegionMappingListInput.every` | — | dropped | follows its list filter |
| `RegionMappingListInput.none` | — | dropped | follows its list filter |
| `RegionMappingListInput.isNull` | — | dropped | follows its list filter |

### `RegionMappingOutput` → `RegionHandoff`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionMappingOutput` | `RegionHandoff` | reshaped | one type per treatment: `CopiedRegion`, `SummarizedRegion`, `ExtractedRegion` |
| `RegionMappingOutput.fromRegion` | `RegionHandoff.fromRegionName` | renamed | a name: the other blueprint need not be installed; old name kept one release, deprecated |
| `RegionMappingOutput.toRegion` | `RegionHandoff.toRegionName` | renamed | a name; old name kept one release, deprecated |
| `RegionMappingOutput.transform` | `__typename` | reshaped | `CopiedRegion`, `SummarizedRegion`, `ExtractedRegion` |
| `RegionMappingOutput.fields` | `ExtractedRegion.fields` | reshaped | extraction only |

### `RegionOutput` → `DeclaredRegion`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionOutput` | `DeclaredRegion` | reshaped | one type per kind behind `DeclaredRegion`; `RuntimeRegion` for the four the runtime supplies |
| `RegionOutput.name` | `Region.name` | kept | on `Region` |
| `RegionOutput.declaredByStage` | `DeclaredRegion.stage` | renamed | the stage whose layout declares it; old name kept one release, deprecated |
| `RegionOutput.kind` | `__typename` | reshaped | the `DeclaredRegion` type |
| `RegionOutput.maxTokens` | `DeclaredRegion.budget` | reshaped | a `RegionBudget` as written; the provisional 0 for a percentage is gone |
| `RegionOutput.budgetPercent` | `WindowShareBudget.percent` | reshaped | on the budget |
| `RegionOutput.minTokens` | `WindowShareBudget.minTokens` | reshaped | on the budget |
| `RegionOutput.budgetMaxTokens` | `WindowShareBudget.maxTokens` | reshaped | on the budget |
| `RegionOutput.description` | `DeclaredRegion.description` | merged |  |
| `RegionOutput.required` | `DeclaredRegion.required` | reshaped | a `RegionRequirement`, null when not required |
| `RegionOutput.requiredMessage` | `RegionRequirement.message` | reshaped | on the requirement |
| `RegionOutput.describeInPrompt` | `DeclaredRegion.describedToModel` | renamed | says what it does; old name kept one release, deprecated |
| `RegionOutput.summarizable` | `DeclaredRegion.summarizable` | merged |  |
| `RegionOutput.volatility` | `DeclaredRegion.volatility` | merged |  |
| `RegionOutput.admission` | `DeclaredRegion.admission` | merged |  |
| `RegionOutput.compactAt` | `CompactingRegion.compactAtPercent` | reshaped | percent 0-100, as every percent is |
| `RegionOutput.accepts` | `DeclaredRegion.accepts` | merged |  |
| `RegionOutput.seed` | `DeclaredRegion.seed` | merged |  |
| `RegionOutput.maxItems` | `SlidingWindowRegion.maxItems` | reshaped | sliding windows only |
| `RegionOutput.strategy` | `SlidingWindowRegion.batchEviction` | reshaped | a nullable `BatchEviction` union |
| `RegionOutput.overflow` | `DropInBatches.overflow` | reshaped | on its strategy |
| `RegionOutput.compactCount` | `SummarizeInBatches.count` | reshaped | on its strategy |
| `RegionOutput.thresholdTokens` | `CompactingRegion.compactAtTokens` | reshaped | null when not written, never 2147483647 |
| `RegionOutput.sourceRegion` | `CompactHistoryRegion.source` | reshaped | compact-history regions only |
| `RegionOutput.sourceRegionName` | `CompactHistoryRegion.source` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `RegionOutput.maxEntries` | `KeyValueRegion.maxEntries` | reshaped | key-value regions only |
| `RegionOutput.script` | `ScriptedRegion.hook` | reshaped | the region hook, as the revision holds it |
| `RegionOutput.pinned` | `ScriptedRegion.neverEvicted` | reshaped | custom regions only |

### `RegionPeakInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionPeakInput` | — | dropped | nothing filters on `RegionPeak` in v2 |
| `RegionPeakInput.region` | — | dropped | follows `RegionPeak.regionName`, which v2 does not filter on |
| `RegionPeakInput.tokens` | — | dropped | follows `RegionPeak.tokens`, which v2 does not filter on |
| `RegionPeakInput.and` | — | dropped | follows its filter |
| `RegionPeakInput.or` | — | dropped | follows its filter |
| `RegionPeakInput.not` | — | dropped | follows its filter |
| `RegionPeakInput.isNull` | — | dropped | follows its filter |

### `RegionPeakListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionPeakListInput` | — | dropped | no v2 listing filters a list of `RegionPeak` (now `RegionPeak`) |
| `RegionPeakListInput.some` | — | dropped | follows its list filter |
| `RegionPeakListInput.every` | — | dropped | follows its list filter |
| `RegionPeakListInput.none` | — | dropped | follows its list filter |
| `RegionPeakListInput.isNull` | — | dropped | follows its list filter |

### `RegionPeakOutput` → `RegionPeak`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionPeakOutput` | `RegionPeak` | reshaped | the region name is named so |
| `RegionPeakOutput.region` | `RegionPeak.regionName` | renamed | it is a name; old name kept one release, deprecated |
| `RegionPeakOutput.tokens` | `RegionPeak.tokens` | kept |  |

### `RegionRef`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionRef` | `RegionRef` | kept |  |
| `RegionRef.name` | `RegionRef.name` | kept |  |

### `RegionSeed`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionSeed` | `RegionSeed` | kept |  |
| `RegionSeed.SeedFromCallerOutput` | `RegionSeed.SeedFromCaller` | kept |  |
| `RegionSeed.SeedFromGlobOutput` | `RegionSeed.SeedFromGlob` | kept |  |
| `RegionSeed.SeedFromFilesOutput` | `RegionSeed.SeedFromFiles` | kept |  |
| `RegionSeed.SeedFromLiteralOutput` | `RegionSeed.SeedFromLiteral` | kept |  |
| `RegionSeed.SeedFromScriptOutput` | `RegionSeed.SeedFromScript` | kept |  |
| `RegionSeed.SeedFromCommandOutput` | `RegionSeed.SeedFromCommand` | kept |  |
| `RegionSeed.SeedFromToolsOutput` | `RegionSeed.SeedFromTools` | kept |  |

### `RegionSeedInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionSeedInput` | — | dropped | nothing filters on `RegionSeed` in v2 |
| `RegionSeedInput.seedFromCaller` | — | dropped | follows `RegionSeed.seedFromCaller`, which went |
| `RegionSeedInput.seedFromGlob` | — | dropped | follows `RegionSeed.seedFromGlob`, which went |
| `RegionSeedInput.seedFromFiles` | — | dropped | follows `RegionSeed.seedFromFiles`, which went |
| `RegionSeedInput.seedFromLiteral` | — | dropped | follows `RegionSeed.seedFromLiteral`, which went |
| `RegionSeedInput.seedFromScript` | — | dropped | follows `RegionSeed.seedFromScript`, which went |
| `RegionSeedInput.seedFromCommand` | — | dropped | follows `RegionSeed.seedFromCommand`, which went |
| `RegionSeedInput.seedFromTools` | — | dropped | follows `RegionSeed.seedFromTools`, which went |
| `RegionSeedInput.and` | — | dropped | follows its filter |
| `RegionSeedInput.or` | — | dropped | follows its filter |
| `RegionSeedInput.not` | — | dropped | follows its filter |
| `RegionSeedInput.isNull` | — | dropped | follows its filter |

### `RegionSeedWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionSeedWrite` | `RegionSeedWrite` | kept |  |
| `RegionSeedWrite.region` | `RegionSeedWrite.region` | kept |  |
| `RegionSeedWrite.text` | `RegionSeedWrite.text` | kept |  |

### `RegionStrategy` → `BatchEviction`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionStrategy` | `BatchEviction` | reshaped | a nullable union on `SlidingWindowRegion`: it decided which fields applied |
| `RegionStrategy.PER_ITEM` | `SlidingWindowRegion.batchEviction` null | dropped | a `BatchEviction` type each; per item is none |
| `RegionStrategy.BULK` | `DropInBatches` | reshaped | a `BatchEviction` type each; per item is none |
| `RegionStrategy.COMPACT` | `SummarizeInBatches` | reshaped | a `BatchEviction` type each; per item is none |

### `RegionStrategyFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionStrategyFilter` | — | dropped | nothing filters on `BatchEviction` in v2 |
| `RegionStrategyFilter.eq` | — | dropped | follows `RegionStrategy` |
| `RegionStrategyFilter.ne` | — | dropped | follows `RegionStrategy` |
| `RegionStrategyFilter.in` | — | dropped | follows `RegionStrategy` |
| `RegionStrategyFilter.notIn` | — | dropped | follows `RegionStrategy` |
| `RegionStrategyFilter.isNull` | — | dropped | follows its filter |

### `RegionTransitionInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionTransitionInput` | — | dropped | nothing filters on `RegionChange` in v2 |
| `RegionTransitionInput.region` | — | dropped | follows `RegionChange.regionName`, which v2 does not filter on |
| `RegionTransitionInput.digestBefore` | — | dropped | follows `RegionChange.digestBefore`, which v2 does not filter on |
| `RegionTransitionInput.digestAfter` | — | dropped | follows `RegionChange.digestAfter`, which v2 does not filter on |
| `RegionTransitionInput.tokensBefore` | — | dropped | follows `RegionChange.tokensBefore`, which v2 does not filter on |
| `RegionTransitionInput.tokensAfter` | — | dropped | follows `RegionChange.tokensAfter`, which v2 does not filter on |
| `RegionTransitionInput.tokenDelta` | — | dropped | follows `RegionChange.tokenDelta`, which v2 does not filter on |
| `RegionTransitionInput.entriesBefore` | — | dropped | follows `RegionTransition.entriesBefore`, which went |
| `RegionTransitionInput.entriesAfter` | — | dropped | follows `RegionTransition.entriesAfter`, which went |
| `RegionTransitionInput.entriesAdded` | — | dropped | follows `RegionChange.entriesAdded`, which v2 does not filter on |
| `RegionTransitionInput.entriesRemoved` | — | dropped | follows `RegionChange.entriesRemoved`, which v2 does not filter on |
| `RegionTransitionInput.and` | — | dropped | follows its filter |
| `RegionTransitionInput.or` | — | dropped | follows its filter |
| `RegionTransitionInput.not` | — | dropped | follows its filter |
| `RegionTransitionInput.isNull` | — | dropped | follows its filter |

### `RegionTransitionListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionTransitionListInput` | — | dropped | no v2 listing filters a list of `RegionTransition` (now `RegionChange`) |
| `RegionTransitionListInput.some` | — | dropped | follows its list filter |
| `RegionTransitionListInput.every` | — | dropped | follows its list filter |
| `RegionTransitionListInput.none` | — | dropped | follows its list filter |
| `RegionTransitionListInput.isNull` | — | dropped | follows its list filter |

### `RegionTransitionOutput` → `RegionChange`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionTransitionOutput` | `RegionChange` | renamed | a change to one region |
| `RegionTransitionOutput.region` | `RegionChange.regionName` | renamed | a name, with `region` resolving it |
| `RegionTransitionOutput.digestBefore` | `RegionChange.digestBefore` | kept |  |
| `RegionTransitionOutput.digestAfter` | `RegionChange.digestAfter` | kept |  |
| `RegionTransitionOutput.tokensBefore` | `RegionChange.tokensBefore` | kept |  |
| `RegionTransitionOutput.tokensAfter` | `RegionChange.tokensAfter` | kept |  |
| `RegionTransitionOutput.tokenDelta` | `RegionChange.tokenDelta` | kept |  |
| `RegionTransitionOutput.entriesBefore` | `RegionChange.region` (`ContextRegion.entryCount`) | dropped | on the region as it stood after |
| `RegionTransitionOutput.entriesAfter` | `RegionChange.region` (`ContextRegion.entryCount`) | dropped | on the region |
| `RegionTransitionOutput.entriesAdded` | `RegionChange.entriesAdded` | kept |  |
| `RegionTransitionOutput.entriesRemoved` | `RegionChange.entriesRemoved` | kept |  |

### `RegionVolatility`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionVolatility` | `RegionVolatility` | kept |  |
| `RegionVolatility.STABLE` | `RegionVolatility.STABLE` | kept |  |
| `RegionVolatility.GROWS` | `RegionVolatility.GROWS` | kept |  |
| `RegionVolatility.REWRITTEN` | `RegionVolatility.REWRITTEN` | kept |  |

### `RegionVolatilityFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RegionVolatilityFilter` | — | dropped | nothing filters on `RegionVolatility` in v2 |
| `RegionVolatilityFilter.eq` | — | dropped | follows `RegionVolatility` |
| `RegionVolatilityFilter.ne` | — | dropped | follows `RegionVolatility` |
| `RegionVolatilityFilter.in` | — | dropped | follows `RegionVolatility` |
| `RegionVolatilityFilter.notIn` | — | dropped | follows `RegionVolatility` |
| `RegionVolatilityFilter.isNull` | — | dropped | follows its filter |

### `RepetitionDetectionInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RepetitionDetectionInput` | — | dropped | nothing filters on `RepetitionDetection` in v2 |
| `RepetitionDetectionInput.enabled` | — | dropped | follows `RepetitionDetection.enabled`, which v2 does not filter on |
| `RepetitionDetectionInput.maxRepeatCalls` | — | dropped | follows `RepetitionDetection.maxRepeatCalls`, which v2 does not filter on |
| `RepetitionDetectionInput.maxReadonlyStreak` | — | dropped | follows `RepetitionDetection.maxReadonlyStreak`, which v2 does not filter on |
| `RepetitionDetectionInput.and` | — | dropped | follows its filter |
| `RepetitionDetectionInput.or` | — | dropped | follows its filter |
| `RepetitionDetectionInput.not` | — | dropped | follows its filter |
| `RepetitionDetectionInput.isNull` | — | dropped | follows its filter |

### `RepetitionDetectionOutput` → `RepetitionDetection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RepetitionDetectionOutput` | `RepetitionDetection` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `RepetitionDetectionOutput.enabled` | `RepetitionDetection.enabled` | kept |  |
| `RepetitionDetectionOutput.maxRepeatCalls` | `RepetitionDetection.maxRepeatCalls` | kept |  |
| `RepetitionDetectionOutput.maxReadonlyStreak` | `RepetitionDetection.maxReadonlyStreak` | kept |  |

### `RequestDigestInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RequestDigestInput` | — | dropped | nothing filters on `RequestSummary` in v2 |
| `RequestDigestInput.systemHash` | — | dropped | follows `RequestSummary.systemDigest`, which v2 does not filter on |
| `RequestDigestInput.messages` | — | dropped | follows `RequestSummary.messageCount`, which v2 does not filter on |
| `RequestDigestInput.tools` | — | dropped | follows `RequestSummary.toolCount`, which v2 does not filter on |
| `RequestDigestInput.maxTokens` | — | dropped | follows `RequestSummary.maxOutputTokens`, which v2 does not filter on |
| `RequestDigestInput.temperature` | — | dropped | follows `RequestSummary.temperature`, which v2 does not filter on |
| `RequestDigestInput.and` | — | dropped | follows its filter |
| `RequestDigestInput.or` | — | dropped | follows its filter |
| `RequestDigestInput.not` | — | dropped | follows its filter |
| `RequestDigestInput.isNull` | — | dropped | follows its filter |

### `RequestDigestOutput` → `RequestSummary`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RequestDigestOutput` | `RequestSummary` | renamed | a summary of what went out, not a digest |
| `RequestDigestOutput.systemHash` | `RequestSummary.systemDigest` | renamed | one word for a content hash; old name kept one release, deprecated |
| `RequestDigestOutput.messages` | `RequestSummary.messageCount` | renamed | a count; old name kept one release, deprecated |
| `RequestDigestOutput.tools` | `RequestSummary.toolCount` | renamed | a count; old name kept one release, deprecated |
| `RequestDigestOutput.maxTokens` | `RequestSummary.maxOutputTokens` | renamed | says which tokens; old name kept one release, deprecated |
| `RequestDigestOutput.temperature` | `RequestSummary.temperature` | kept |  |

### `ResumeRunRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ResumeRunRequest` | `ResumeRunRequest` | kept |  |
| `ResumeRunRequest.id` | `ResumeRunRequest.id` | kept |  |

### `ResumeRunResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ResumeRunResult` | `ResumeRunResult` | kept |  |
| `ResumeRunResult.run` | `ResumeRunResult.run` | kept |  |
| `ResumeRunResult.warnings` | `ResumeRunResult.warnings` | kept |  |

### `ResumeRunsRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ResumeRunsRequest` | `ResumeRunsRequest` | kept |  |
| `ResumeRunsRequest.filter` | `ResumeRunsRequest.filter` | kept |  |

### `ResumeRunsResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ResumeRunsResult` | `ResumeRunsResult` | kept |  |
| `ResumeRunsResult.runs` | `ResumeRunsResult.outcomes` | dropped | deprecated; each run answers as a `SweepItem` type |
| `ResumeRunsResult.skipped` | `ResumeRunsResult.outcomes` | dropped | deprecated; `SweepRefused`, `SweepAlreadyInState` and the rest |

### `RetryDecision`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RetryDecision` | `RetryDecision` | kept |  |
| `RetryDecision.REPORTED` | `RetryDecision.REPORTED` | kept |  |
| `RetryDecision.SAME_MODEL` | `RetryDecision.SAME_MODEL` | kept |  |
| `RetryDecision.RENEWED_FILES` | `RetryDecision.RENEWED_FILES` | kept |  |

### `RetryDecisionFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RetryDecisionFilter` | `RetryDecisionFilter` | kept |  |
| `RetryDecisionFilter.eq` | `RetryDecisionFilter.eq` | kept |  |
| `RetryDecisionFilter.ne` | `RetryDecisionFilter.ne` | kept |  |
| `RetryDecisionFilter.in` | `RetryDecisionFilter.in` | kept |  |
| `RetryDecisionFilter.notIn` | `RetryDecisionFilter.notIn` | kept |  |
| `RetryDecisionFilter.isNull` | `RetryDecisionFilter.isNull` | kept |  |

### `RoutingConfigOutput` → `ModelRouting`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RoutingConfigOutput` | `ModelRouting` | reshaped | names become typed choices |
| `RoutingConfigOutput.defaultProvider` | `ModelRouting.defaultProvider` | reshaped | a `ProviderChoice`: the name and what it resolves to |
| `RoutingConfigOutput.providerOrder` | `ModelRouting.providerOrder` | reshaped | `ProviderChoice`s |
| `RoutingConfigOutput.overrideModel` | `ModelRouting.overrideModel` | reshaped | a `ModelChoice` |
| `RoutingConfigOutput.fallbackModel` | `ModelRouting.fallbackModel` | reshaped | a `ModelChoice` |

### `RunCompletedEvent` → `RunFinishedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunCompletedEvent` | `RunFinishedEvent` | reshaped | it fired for failed and cancelled runs too |
| `RunCompletedEvent.seq` | `RunFinishedEvent.seq` | kept |  |
| `RunCompletedEvent.at` | `RunFinishedEvent.at` | kept |  |
| `RunCompletedEvent.runId` | `RunFinishedEvent.runId` | kept |  |
| `RunCompletedEvent.agentId` | `RunEvent.runId` | dropped | the run's live agent id is not a `Run` fact; frames carry `runId` and `run` |
| `RunCompletedEvent.status` | `RunFinishedEvent.state` | reshaped | a `Finished` state, carrying the verdict |
| `RunCompletedEvent.error` | `Failed.error` | reshaped | on the `Failed` state |
| `RunCompletedEvent.finalOutput` | `RunFinishedEvent.answer` | reshaped | an `Answer` |
| `RunCompletedEvent.run` | `RunFinishedEvent.run` | kept |  |

### `RunConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunConnection` | `RunConnection` | kept |  |
| `RunConnection.results` | `RunConnection.results` | kept |  |
| `RunConnection.cursor` | `RunConnection.cursor` | kept |  |
| `RunConnection.total` | `RunConnection.total` | kept |  |
| `RunConnection.highlights` | — | dropped | follows its connection |

### `RunEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunEvent` | `RunEvent` | reshaped | implements `Event`; `agentId` goes |
| `RunEvent.seq` | `RunEvent.seq` | kept |  |
| `RunEvent.at` | `RunEvent.at` | kept |  |
| `RunEvent.runId` | `RunEvent.runId` | kept |  |
| `RunEvent.agentId` | `RunEvent.runId` | dropped | the run's live agent id is not a `Run` fact; frames carry `runId` and `run` |
| `RunEvent.run` | `RunEvent.run` | kept |  |

### `RunEventFrame`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunEventFrame` | `RunEventFrame` | kept |  |
| `RunEventFrame.RunSpawnedEvent` | `RunEventFrame.RunSpawnedEvent` | kept |  |
| `RunEventFrame.RunStatusChangedEvent` | `RunEventFrame.RunChangedEvent` | renamed | the status word is gone |
| `RunEventFrame.RunRenamedEvent` | `RunEventFrame.RunRenamedEvent` | kept |  |
| `RunEventFrame.TokensUpdatedEvent` | `RunEventFrame.TokensUpdatedEvent` | kept |  |
| `RunEventFrame.ContextUpdatedEvent` | `RunEventFrame.ContextUpdatedEvent` | kept |  |
| `RunEventFrame.StageTransitionedEvent` | `RunEventFrame.StageTransitionedEvent` | kept |  |
| `RunEventFrame.ToolCallStartedEvent` | `RunEventFrame.ToolCallStartedEvent` | kept |  |
| `RunEventFrame.ToolCallFinishedEvent` | `RunEventFrame.ToolCallFinishedEvent` | kept |  |
| `RunEventFrame.LogLineWrittenEvent` | `RunEventFrame.LogLineWrittenEvent` | kept |  |
| `RunEventFrame.SpendThresholdCrossedEvent` | `RunEventFrame.SpendThresholdCrossedEvent` | kept |  |
| `RunEventFrame.InteractionOpenedEvent` | `RunEventFrame.InteractionOpenedEvent` | kept |  |
| `RunEventFrame.RunCompletedEvent` | `RunEventFrame.RunFinishedEvent` | renamed | it fires for every ending |
| `RunEventFrame.DaemonLinkChangedEvent` | `RunEventFrame.DaemonLinkChangedEvent` | kept |  |
| `RunEventFrame.SubscriptionOpenedEvent` | `RunEventFrame.SubscriptionOpenedEvent` | kept |  |
| `RunEventFrame.EventsDroppedEvent` | `RunEventFrame.EventsDroppedEvent` | kept |  |

### `RunEventType` → `RunEventFrameFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunEventType` | `RunEventFrameFilter` | reshaped | frames are chosen with one field per frame type |
| `RunEventType.RUN_SPAWNED` | `RunEventFrameFilter.runSpawnedEvent` | reshaped | one boolean field per frame type |
| `RunEventType.RUN_STATUS_CHANGED` | `RunEventFrameFilter.runChangedEvent` | reshaped | one boolean field per frame type |
| `RunEventType.RUN_RENAMED` | `RunEventFrameFilter.runRenamedEvent` | reshaped | one boolean field per frame type |
| `RunEventType.TOKENS_UPDATED` | `RunEventFrameFilter.tokensUpdatedEvent` | reshaped | one boolean field per frame type |
| `RunEventType.CONTEXT_UPDATED` | `RunEventFrameFilter.contextUpdatedEvent` | reshaped | one boolean field per frame type |
| `RunEventType.STAGE_TRANSITIONED` | `RunEventFrameFilter.stageTransitionedEvent` | reshaped | one boolean field per frame type |
| `RunEventType.TOOL_CALL_STARTED` | `RunEventFrameFilter.toolCallStartedEvent` | reshaped | one boolean field per frame type |
| `RunEventType.TOOL_CALL_FINISHED` | `RunEventFrameFilter.toolCallFinishedEvent` | reshaped | one boolean field per frame type |
| `RunEventType.LOG_LINE_WRITTEN` | `RunEventFrameFilter.logLineWrittenEvent` | reshaped | one boolean field per frame type |
| `RunEventType.SPEND_THRESHOLD_CROSSED` | `RunEventFrameFilter.spendThresholdCrossedEvent` | reshaped | one boolean field per frame type |
| `RunEventType.INTERACTION_OPENED` | `RunEventFrameFilter.interactionOpenedEvent` | reshaped | one boolean field per frame type |
| `RunEventType.RUN_COMPLETED` | `RunEventFrameFilter.runFinishedEvent` | reshaped | one boolean field per frame type |

### `RunExportOutput` → `RunExport`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunExportOutput` | `RunExport` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `RunExportOutput.id` | `RunExport.id` | reshaped | tagged: `runExport:<unix seconds>-<n>` |
| `RunExportOutput.status` | `Job.state` | reshaped | the `JobState` union |
| `RunExportOutput.written` | `RunExport.runsWritten` | renamed | now nullable; says what it counts; null when expired; old name kept one release, deprecated |
| `RunExportOutput.error` | `JobFailed.error` | reshaped | on the failed state |
| `RunExportOutput.downloadUrl` | `ExportOutcome.download` | reshaped | a `Download` with its expiry, on the completed state |

### `RunFlagsInput` → `RunProblemFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunFlagsInput` | `RunProblemFilter` | renamed | follows `RunProblem` |
| `RunFlagsInput.emptyOutput` | — | dropped | follows `NothingProduced`, which v2 does not filter on |
| `RunFlagsInput.producedOutput` | — | dropped | follows `Run.answer`, which v2 does not filter on |
| `RunFlagsInput.outputForced` | `AnswerSkippedFilter.count` | renamed | follows `AnswerSkipped.count` |
| `RunFlagsInput.noOutputTools` | — | dropped | follows `NoAnswerSubmitted`, which v2 does not filter on |
| `RunFlagsInput.gatesForced` | `GateForcedFilter.count` | renamed | follows `GateForced.count` |
| `RunFlagsInput.maxIterationsHit` | `TurnLimitReachedFilter.count` | renamed | follows `TurnLimitReached.count` |
| `RunFlagsInput.splitsDegraded` | `FanOutDegradedFilter.count` | renamed | follows `FanOutDegraded.count` |
| `RunFlagsInput.modifiedFileCount` | — | dropped | follows `Run.changedFileCount`, which v2 does not filter on |
| `RunFlagsInput.modifiedFiles` | — | dropped | follows `Run.changedFiles`, which v2 does not filter on |
| `RunFlagsInput.searchesRun` | `EverySearchEmptyFilter.searches` | renamed | follows `EverySearchEmpty.searches` |
| `RunFlagsInput.searchesEmpty` | — | dropped | follows `EverySearchEmpty`, which v2 does not filter on |
| `RunFlagsInput.requiredRegionsAbandoned` | `RequiredRegionsEmptyFilter.regionNames` | renamed | follows `RequiredRegionsEmpty.regionNames` |
| `RunFlagsInput.workspaceLost` | — | dropped | follows `WorkingDirectoryLost`, which v2 does not filter on |
| `RunFlagsInput.and` | `RunProblemFilter.and` | reshaped | now `[RunProblemFilter]` |
| `RunFlagsInput.or` | `RunProblemFilter.or` | reshaped | now `[RunProblemFilter]` |
| `RunFlagsInput.not` | `RunProblemFilter.not` | reshaped | now `RunProblemFilter` |
| `RunFlagsInput.isNull` | `RunProblemFilter.isNull` | kept |  |

### `RunFlagsOutput` → `RunProblem`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunFlagsOutput` | `RunProblem` | reshaped | findings become typed `RunProblem`s; facts move to `Run` |
| `RunFlagsOutput.emptyOutput` | `NothingProduced` | reshaped | a typed problem |
| `RunFlagsOutput.producedOutput` | `Run.answer` | dropped | answered by `answer` being set |
| `RunFlagsOutput.outputForced` | `AnswerSkipped.count` | reshaped | a typed problem |
| `RunFlagsOutput.noOutputTools` | `NoAnswerSubmitted` | reshaped | picks which problem an empty run reports |
| `RunFlagsOutput.gatesForced` | `GateForced.count` | reshaped | a typed problem |
| `RunFlagsOutput.maxIterationsHit` | `TurnLimitReached.count` | reshaped | a typed problem |
| `RunFlagsOutput.splitsDegraded` | `FanOutDegraded.count` | reshaped | a typed problem |
| `RunFlagsOutput.modifiedFileCount` | `Run.changedFileCount` | renamed | a fact, not a problem |
| `RunFlagsOutput.modifiedFiles` | `Run.changedFiles` | reshaped | the `ChangedFile`s |
| `RunFlagsOutput.searchesRun` | `EverySearchEmpty.searches` | reshaped | kept only inside the problem it signals |
| `RunFlagsOutput.searchesEmpty` | `EverySearchEmpty` | reshaped | the problem is every search coming back empty |
| `RunFlagsOutput.requiredRegionsAbandoned` | `RequiredRegionsEmpty.regionNames` | reshaped | a typed problem |
| `RunFlagsOutput.workspaceLost` | `WorkingDirectoryLost` | reshaped | a typed problem |

### `RunHighlightOutput` → `RunSearchMatch`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunHighlightOutput` | `RunSearchMatch` | reshaped | one type per place a search can match |
| `RunHighlightOutput.runId` | `RunSearchMatch.run` | reshaped | the `Run` |
| `RunHighlightOutput.field` | `__typename` | reshaped | the match type says where it matched: `RunFieldMatch.field`, `MetadataMatch.key`, `ChangedFileMatch.path`, `ContextMatch.regionName`, `LogMatch.stream`, `JournalMatch.journalPosition` |
| `RunHighlightOutput.snippet` | `RunSearchMatch.snippet` | kept |  |
| `RunHighlightOutput.stageIndex` | `LogMatch.stage` | reshaped | only a log match has a stage |

### `RunInput` → `RunFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunInput` | `RunFilter` | kept |  |
| `RunInput.id` | `RunFilter.id` | kept |  |
| `RunInput.blueprintName` | — | dropped | follows `Run.blueprintName`, which went |
| `RunInput.title` | `RunFilter.title` | kept |  |
| `RunInput.titleError` | — | dropped | follows `Run.titleError`, which v2 does not filter on |
| `RunInput.status` | `RunFilter.state` | reshaped | now `RunStateFilter`; follows `Run.state` |
| `RunInput.error` | `FailedFilter.error` | renamed | follows `Failed.error` |
| `RunInput.task` | `RunFilter.task` | kept |  |
| `RunInput.iteration` | `RunningFilter.turn` | renamed | follows `Running.turn` |
| `RunInput.toolCallCount` | `RunFilter.toolCallCount` | kept |  |
| `RunInput.startedAt` | `RunFilter.startedAt` | kept |  |
| `RunInput.updatedAt` | `RunFilter.lastMovedAt` | renamed | follows `Run.lastMovedAt` |
| `RunInput.lastProgressAt` | `RunFilter.lastMovedAt` | renamed | follows `Run.lastMovedAt` |
| `RunInput.ageSecs` | — | dropped | follows `Run.ageSeconds`, which v2 does not filter on |
| `RunInput.workingSecs` | — | dropped | follows `Run.workingSeconds`, which v2 does not filter on |
| `RunInput.active` | — | dropped | follows `Run.workingSince`, which v2 does not filter on |
| `RunInput.usage` | — | dropped | follows `Run.usage`, which v2 does not filter on |
| `RunInput.cost` | `RunFilter.cost` | reshaped | now `CostFilter` |
| `RunInput.unattended` | `RunFilter.approvalPolicy` | reshaped | now `ApprovalPolicyRevisionFilter`; follows `Run.approvalPolicy` |
| `RunInput.yoloProfileName` | `RunFilter.approvalPolicy` | reshaped | now `ApprovalPolicyRevisionFilter`; follows `Run.approvalPolicy` |
| `RunInput.workdir` | — | dropped | follows `Run.workingDirectory`, which v2 does not filter on |
| `RunInput.parentId` | `RunFilter.parent` | reshaped | now `RunFilter`; follows `Run.parent` |
| `RunInput.ancestorIds` | `RunFilter.ancestors` | reshaped | now `RunListFilter`; follows `Run.ancestors` |
| `RunInput.stageModels` | — | dropped | follows `Run.modelsUsed`, which v2 does not filter on |
| `RunInput.blueprintDigest` | — | dropped | follows `Run.blueprintDigest`, which went |
| `RunInput.waitReason` | `RunFilter.state` | reshaped | now `RunStateFilter`; follows `Run.state` |
| `RunInput.flags` | `RunFilter.problems` | reshaped | now `RunProblemListFilter`; follows `Run.problems` |
| `RunInput.stages` | — | dropped | follows `Run.stages`, which v2 does not filter on |
| `RunInput.context` | — | dropped | follows `Run.context`, which v2 does not filter on |
| `RunInput.finalOutput` | — | dropped | follows `Run.answer`, which v2 does not filter on |
| `RunInput.blobs` | — | dropped | follows `Run.parts`, which v2 does not filter on |
| `RunInput.artifacts` | — | dropped | follows `Answer.artifacts`, which v2 does not filter on |
| `RunInput.parent` | `RunFilter.parent` | kept |  |
| `RunInput.currentStage` | — | dropped | follows `Run.currentStage`, which v2 does not filter on |
| `RunInput.metadata` | `RunFilter.metadata` | reshaped | now `KeyValueListFilter` |
| `RunInput.and` | `RunFilter.and` | kept |  |
| `RunInput.or` | `RunFilter.or` | kept |  |
| `RunInput.not` | `RunFilter.not` | kept |  |
| `RunInput.isNull` | — | dropped | follows its filter |

### `RunOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunOrder` | `RunOrder` | kept |  |
| `RunOrder.field` | `RunOrder.field` | kept |  |
| `RunOrder.direction` | `RunOrder.direction` | kept |  |

### `RunOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunOrderField` | `RunOrderField` | kept |  |
| `RunOrderField.TITLE` | `RunOrderField.TITLE` | kept |  |
| `RunOrderField.STARTED_AT` | `RunOrderField.STARTED_AT` | kept |  |
| `RunOrderField.UPDATED_AT` | — | dropped | not a sort key in v2 |
| `RunOrderField.LAST_PROGRESS_AT` | — | dropped | not a sort key in v2 |

### `RunOutput` → `Run`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunOutput` | `Run` | reshaped | one honest `state` union, typed `problems`, relations instead of names and ids |
| `RunOutput.id` | `Run.id` | kept |  |
| `RunOutput.blueprintName` | `Run.blueprint` (`BlueprintRevision.name`) | merged | a name string beside the relation; the filter answers `blueprint: { name }` from the index |
| `RunOutput.title` | `Run.title` | kept |  |
| `RunOutput.titleError` | `Run.titleError` | kept |  |
| `RunOutput.status` | `Run.state` | reshaped | the `RunState` union tells waiting-on-a-person from waiting-on-workers and carries the verdict |
| `RunOutput.error` | `Failed.error` | reshaped | on the `Failed` state |
| `RunOutput.task` | `Run.task` | kept |  |
| `RunOutput.iteration` | `Running.turn` | reshaped | on the `Running` state; `RunStage` visits hold their turns |
| `RunOutput.toolCallCount` | `Run.toolCallCount` | kept |  |
| `RunOutput.startedAt` | `Run.startedAt` | kept |  |
| `RunOutput.updatedAt` | `Run.lastMovedAt` | dropped | a 30-second heartbeat described as a state change |
| `RunOutput.lastProgressAt` | `Run.lastMovedAt` | renamed | when it last moved; old name kept one release, deprecated |
| `RunOutput.ageSecs` | `Run.ageSeconds` | renamed | no abbreviations; old name kept one release, deprecated |
| `RunOutput.workingSecs` | `Run.workingSeconds` | renamed | no abbreviations; old name kept one release, deprecated |
| `RunOutput.active` | `Run.workingSince` | reshaped | two fields instead of `WorkingClock` |
| `RunOutput.usage` | `Run.usage` | kept |  |
| `RunOutput.cost` | `Run.cost` | kept |  |
| `RunOutput.unattended` | `Run.approvalPolicy` | merged | unattended is `approvalPolicy` set; bare `--yolo` is the built-in `"default"` policy |
| `RunOutput.yoloProfileName` | `Run.approvalPolicy` | reshaped | the `ApprovalPolicyRevision` it ran under, not a name |
| `RunOutput.workdir` | `Run.workingDirectory` | reshaped | a `RunDirectory`; its path is `path` |
| `RunOutput.parentId` | `Run.parent` | merged | the relation; filter with `parent: { id }` |
| `RunOutput.ancestorIds` | `Run.ancestors` | reshaped | the runs; filter with `ancestors: { some: { id } }` |
| `RunOutput.stageModels` | `Run.modelsUsed` | reshaped | a list of `ModelUse` |
| `RunOutput.blueprint` | `Run.blueprint` | reshaped | the exact `BlueprintRevision` it executed, never null |
| `RunOutput.blueprintDigest` | `Run.blueprint` (`BlueprintRevision.digest`) | merged | the revision it executed, exactly |
| `RunOutput.waitReason` | `Run.state` | reshaped | `WaitingOnPerson`, `WaitingOnSubAgents`, `NeedsSetup`, `WaitingUnexplained` |
| `RunOutput.flags` | `Run.problems` | reshaped | typed `RunProblem`s; the facts moved to `changedFiles`, `changedFileCount`, `answer` |
| `RunOutput.stages` | `Run.stages` | reshaped | a plain list bounded by the blueprint's stage count |
| `RunOutput.stages(filter:)` | — | dropped | a plain list now |
| `RunOutput.stages(orderBy:)` | — | dropped | a plain list now |
| `RunOutput.stages(first:)` | — | dropped | a plain list now |
| `RunOutput.stages(after:)` | — | dropped | a plain list now |
| `RunOutput.context` | `Run.context` | reshaped | a `ContextSnapshot` |
| `RunOutput.finalOutput` | `Run.answer` | reshaped | the `Answer`, which owns its artifacts |
| `RunOutput.children` | `Run.children` | kept |  |
| `RunOutput.children(filter:)` | `Run.children(filter:)` | kept |  |
| `RunOutput.children(orderBy:)` | `Run.children(orderBy:)` | kept |  |
| `RunOutput.children(first:)` | `Run.children(first:)` | kept |  |
| `RunOutput.children(after:)` | `Run.children(after:)` | kept |  |
| `RunOutput.treeStatus` | `Run.subtree` | reshaped | a `RunSubtree`, with cost |
| `RunOutput.logs` | `RunStage.log` | reshaped | logs are read per stage, forward from an offset or as a tail |
| `RunOutput.logs(stage:)` | `Run.stages` | dropped | choose the stage, then read its `log` |
| `RunOutput.logs(stream:)` | `RunStage.log(stream:)` | merged |  |
| `RunOutput.logs(tailBytes:)` | `RunStage.log(from:)` | reshaped | `from: { lastBytes }` |
| `RunOutput.blobs` | `Run.parts` | reshaped | stored parts |
| `RunOutput.blobs(filter:)` | `Run.parts(filter:)` | reshaped | now `StoredPartFilter` |
| `RunOutput.blobs(orderBy:)` | `Run.parts(orderBy:)` | reshaped | now `[StoredPartOrder]` |
| `RunOutput.blobs(first:)` | `Run.parts(first:)` | renamed |  |
| `RunOutput.blobs(after:)` | `Run.parts(after:)` | renamed |  |
| `RunOutput.artifacts` | `Answer.artifacts` | reshaped | artifacts belong to the answer, a plain list |
| `RunOutput.artifacts(filter:)` | — | dropped | a plain list on `Answer` now |
| `RunOutput.artifacts(orderBy:)` | — | dropped | a plain list on `Answer` now |
| `RunOutput.artifacts(first:)` | — | dropped | a plain list on `Answer` now |
| `RunOutput.artifacts(after:)` | — | dropped | a plain list on `Answer` now |
| `RunOutput.fileUrl` | `RunFile.url` | reshaped | a field on the file: `file(path) { url }` |
| `RunOutput.fileUrl(path:)` | `Run.file(path:)` | merged |  |
| `RunOutput.fileUrl(download:)` | `RunFile.url(download:)` | merged |  |
| `RunOutput.parent` | `Run.parent` | kept |  |
| `RunOutput.currentStage` | `Run.currentStage` | kept |  |
| `RunOutput.openInteraction` | `WaitingOnPerson.questions` | reshaped | on the state, and `interactions(filter: { settlement: { isNull: true } })` |
| `RunOutput.files` | `Run.workingDirectory` | reshaped | `workingDirectory.entries` and `file(path)`, or `changedFiles`: two questions, two fields |
| `RunOutput.files(path:)` | `Run.file(path:)` | reshaped | or a `RunDirectory`'s `entries` |
| `RunOutput.files(source:)` | `Run.changedFiles` | reshaped | the source picked the question; each is its own field |
| `RunOutput.files(filter:)` | `RunDirectory.entries(filter:)` | merged |  |
| `RunOutput.files(first:)` | `RunDirectory.entries(first:)` | merged |  |
| `RunOutput.files(after:)` | `RunDirectory.entries(after:)` | merged |  |
| `RunOutput.fileContent` | `RunFile.content` | reshaped | a field on the file: `file(path) { content(offset) }` |
| `RunOutput.fileContent(path:)` | `Run.file(path:)` | merged |  |
| `RunOutput.fileContent(offset:)` | `RunFile.content(offset:)` | merged |  |
| `RunOutput.executions` | `Run.executions` | kept |  |
| `RunOutput.executions(filter:)` | `Run.executions(filter:)` | kept |  |
| `RunOutput.executions(orderBy:)` | `Run.executions(orderBy:)` | kept |  |
| `RunOutput.executions(first:)` | `Run.executions(first:)` | kept |  |
| `RunOutput.executions(after:)` | `Run.executions(after:)` | kept |  |
| `RunOutput.interactions` | `Run.interactions` | kept |  |
| `RunOutput.interactions(filter:)` | `Run.interactions(filter:)` | kept |  |
| `RunOutput.interactions(orderBy:)` | `Run.interactions(orderBy:)` | kept |  |
| `RunOutput.interactions(first:)` | `Run.interactions(first:)` | kept |  |
| `RunOutput.interactions(after:)` | `Run.interactions(after:)` | kept |  |
| `RunOutput.inferences` | `Run.modelAttempts` | reshaped | typed attempts |
| `RunOutput.inferences(filter:)` | `Run.modelAttempts(filter:)` | reshaped | now `ModelAttemptFilter` |
| `RunOutput.inferences(orderBy:)` | `Run.modelAttempts(orderBy:)` | reshaped | now `[ModelAttemptOrder]` |
| `RunOutput.inferences(first:)` | `Run.modelAttempts(first:)` | renamed |  |
| `RunOutput.inferences(after:)` | `Run.modelAttempts(after:)` | renamed |  |
| `RunOutput.contextChanges` | `Run.contextChanges` | kept |  |
| `RunOutput.contextChanges(filter:)` | `Run.contextChanges(filter:)` | kept |  |
| `RunOutput.contextChanges(orderBy:)` | `Run.contextChanges(orderBy:)` | kept |  |
| `RunOutput.contextChanges(first:)` | `Run.contextChanges(first:)` | kept |  |
| `RunOutput.contextChanges(after:)` | `Run.contextChanges(after:)` | kept |  |
| `RunOutput.contextHistory` | `Run.contextSnapshots` | reshaped | the snapshots are the history |
| `RunOutput.contextHistory(filter:)` | `Run.contextSnapshots(filter:)` | reshaped | now `ContextSnapshotFilter` |
| `RunOutput.contextHistory(orderBy:)` | `Run.contextSnapshots(orderBy:)` | reshaped | now `[ContextSnapshotOrder]` |
| `RunOutput.contextHistory(first:)` | `Run.contextSnapshots(first:)` | renamed |  |
| `RunOutput.contextHistory(after:)` | `Run.contextSnapshots(after:)` | renamed |  |
| `RunOutput.contextSnapshot` | `Run.contextSnapshot` | kept |  |
| `RunOutput.contextSnapshot(revision:)` | `Run.contextSnapshot(digest:)` | renamed | a content address |
| `RunOutput.blobUrl` | `StoredPart.url` | reshaped | a field on the part: `part(sha256) { url }` |
| `RunOutput.blobUrl(sha256:)` | `Run.part(sha256:)` | merged |  |
| `RunOutput.blobUrl(download:)` | `StoredPart.url(download:)` | merged |  |
| `RunOutput.artifactUrl` | `Artifact.url` | reshaped | a field on the artifact: `answer { artifacts { url } }` |
| `RunOutput.artifactUrl(name:)` | `Artifact.name` | merged | pick the artifact from `Answer.artifacts` |
| `RunOutput.artifactUrl(download:)` | `Artifact.url(download:)` | merged |  |
| `RunOutput.acceptsMessages` | `Run.acceptsMessages` | kept |  |
| `RunOutput.metadata` | `Run.metadata` | reshaped | a list of `KeyValue` |

### `RunRenamedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunRenamedEvent` | `RunRenamedEvent` | kept |  |
| `RunRenamedEvent.seq` | `RunRenamedEvent.seq` | kept |  |
| `RunRenamedEvent.at` | `RunRenamedEvent.at` | kept |  |
| `RunRenamedEvent.runId` | `RunRenamedEvent.runId` | kept |  |
| `RunRenamedEvent.agentId` | `RunEvent.runId` | dropped | the run's live agent id is not a `Run` fact; frames carry `runId` and `run` |
| `RunRenamedEvent.title` | `RunRenamedEvent.title` | kept |  |
| `RunRenamedEvent.run` | `RunRenamedEvent.run` | kept |  |

### `RunSearchOptions`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunSearchOptions` | `RunSearchOptions` | kept |  |
| `RunSearchOptions.query` | `RunSearchOptions.query` | kept |  |
| `RunSearchOptions.in` | `RunSearchOptions.in` | kept |  |

### `RunSpawnedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunSpawnedEvent` | `RunSpawnedEvent` | kept |  |
| `RunSpawnedEvent.seq` | `RunSpawnedEvent.seq` | kept |  |
| `RunSpawnedEvent.at` | `RunSpawnedEvent.at` | kept |  |
| `RunSpawnedEvent.runId` | `RunSpawnedEvent.runId` | kept |  |
| `RunSpawnedEvent.agentId` | `RunEvent.runId` | dropped | the run's live agent id is not a `Run` fact; frames carry `runId` and `run` |
| `RunSpawnedEvent.blueprintName` | `RunSpawnedEvent.run` (`Run.blueprint`) | merged | on the run |
| `RunSpawnedEvent.parentId` | `RunSpawnedEvent.parent` | reshaped | the relation |
| `RunSpawnedEvent.run` | `RunSpawnedEvent.run` | kept |  |

### `RunStatus`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunStatus` | — | dropped | the `RunState` union carries the state and its data; its filter selects by type |
| `RunStatus.STARTING` | `Starting` | reshaped | a `RunState` type per state |
| `RunStatus.RUNNING` | `Running` | reshaped | a `RunState` type per state |
| `RunStatus.WAITING_INPUT` | `WaitingOnPerson`, `WaitingOnSubAgents`, `NeedsSetup`, `WaitingUnexplained` | reshaped | it covered both "stopped until a person answers" and "healthily waiting on workers" |
| `RunStatus.PAUSED` | `Paused` | reshaped | a `RunState` type per state |
| `RunStatus.COMPLETE` | `Completed`, `CompletedWithProblems` | dropped | a `RunState` type per state |
| `RunStatus.COMPLETE_INTERACTIVE` | `Completed` or `CompletedWithProblems` with `acceptsFollowUp: true` | dropped | a `RunState` type per state |
| `RunStatus.ERROR` | `Failed` | reshaped | a `RunState` type per state |
| `RunStatus.CANCELLED` | `Cancelled` | reshaped | a `RunState` type per state |
| `RunStatus.UNKNOWN` | `UnrecognizedState` | reshaped | a `RunState` type per state |

### `RunStatusChangedEvent` → `RunChangedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunStatusChangedEvent` | `RunChangedEvent` | reshaped | the run's `state`, stage and counters; the status word is gone |
| `RunStatusChangedEvent.seq` | `RunChangedEvent.seq` | kept |  |
| `RunStatusChangedEvent.at` | `RunChangedEvent.at` | kept |  |
| `RunStatusChangedEvent.runId` | `RunChangedEvent.runId` | kept |  |
| `RunStatusChangedEvent.agentId` | `RunEvent.runId` | dropped | the run's live agent id is not a `Run` fact; frames carry `runId` and `run` |
| `RunStatusChangedEvent.status` | `RunChangedEvent.state` | reshaped | the `RunState` union |
| `RunStatusChangedEvent.stage` | `RunChangedEvent.stage` | reshaped | a `RunStage` |
| `RunStatusChangedEvent.iteration` | `Running.turn` | reshaped | on the `Running` state |
| `RunStatusChangedEvent.toolCalls` | `RunChangedEvent.toolCallCount` | reshaped | now `Int`; a count, as on `Run` |
| `RunStatusChangedEvent.acceptsMessages` | `RunChangedEvent.acceptsMessages` | kept |  |
| `RunStatusChangedEvent.waitReason` | `RunChangedEvent.state` | merged | the state carries why it waits |
| `RunStatusChangedEvent.title` | `RunRenamedEvent.title` | dropped | the rename frame says it |
| `RunStatusChangedEvent.run` | `RunChangedEvent.run` | kept |  |

### `RunStatusFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunStatusFilter` | — | dropped | `RunStatus` went |
| `RunStatusFilter.eq` | — | dropped | follows `RunStatus` |
| `RunStatusFilter.ne` | — | dropped | follows `RunStatus` |
| `RunStatusFilter.in` | — | dropped | follows `RunStatus` |
| `RunStatusFilter.notIn` | — | dropped | follows `RunStatus` |
| `RunStatusFilter.isNull` | — | dropped | follows its filter |

### `RuntimeInfoCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RuntimeInfoCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `RuntimeInfoCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `RuntimeInfoCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `RuntimeInfoCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |

### `RunTreeStatusOutput` → `RunSubtree`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `RunTreeStatusOutput` | `RunSubtree` | reshaped | plus the missing cost roll-up |
| `RunTreeStatusOutput.rollup` | `RunSubtree.usage` | renamed | tokens over the subtree; old name kept one release, deprecated |
| `RunTreeStatusOutput.depth` | `RunSubtree.depth` | kept |  |
| `RunTreeStatusOutput.descendantCount` | `RunSubtree.runCount` | reshaped | counts this run too |

### `SafeCommandsInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SafeCommandsInput` | — | dropped | nothing filters on `SafeCommands` in v2 |
| `SafeCommandsInput.tools` | — | dropped | follows `SafeCommands.tools`, which v2 does not filter on |
| `SafeCommandsInput.shell` | — | dropped | follows `SafeCommands.shellCommandPrefixes`, which v2 does not filter on |
| `SafeCommandsInput.and` | — | dropped | follows its filter |
| `SafeCommandsInput.or` | — | dropped | follows its filter |
| `SafeCommandsInput.not` | — | dropped | follows its filter |
| `SafeCommandsInput.isNull` | — | dropped | follows its filter |

### `SafeCommandsOutput` → `SafeCommands`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SafeCommandsOutput` | `SafeCommands` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `SafeCommandsOutput.tools` | `SafeCommands.tools` | reshaped | `ToolByName`s |
| `SafeCommandsOutput.shell` | `SafeCommands.shellCommandPrefixes` | renamed | says what they are; old name kept one release, deprecated |

### `SandboxConfigInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SandboxConfigInput` | — | dropped | nothing filters on `Sandbox` in v2 |
| `SandboxConfigInput.kind` | — | dropped | follows `SandboxConfig.kind`, which went |
| `SandboxConfigInput.image` | — | dropped | follows `ContainerSandbox.image`, which v2 does not filter on |
| `SandboxConfigInput.engine` | — | dropped | follows `ContainerSandbox.engine`, which v2 does not filter on |
| `SandboxConfigInput.allowNetwork` | — | dropped | follows `SandboxConfig.allowNetwork`, which went |
| `SandboxConfigInput.mounts` | — | dropped | follows `ContainerSandbox.mounts`, which v2 does not filter on |
| `SandboxConfigInput.keepWarm` | — | dropped | follows `ContainerSandbox.keptWarm`, which v2 does not filter on |
| `SandboxConfigInput.onUnavailable` | — | dropped | follows `SandboxConfig.onUnavailable`, which went |
| `SandboxConfigInput.and` | — | dropped | follows its filter |
| `SandboxConfigInput.or` | — | dropped | follows its filter |
| `SandboxConfigInput.not` | — | dropped | follows its filter |
| `SandboxConfigInput.isNull` | — | dropped | follows its filter |

### `SandboxConfigOutput` → `Sandbox`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SandboxConfigOutput` | `Sandbox` | reshaped | one type per mechanism: `HostExecution`, `NamespaceSandbox`, `ContainerSandbox` |
| `SandboxConfigOutput.kind` | `__typename` | reshaped | `HostExecution`, `NamespaceSandbox`, `ContainerSandbox` |
| `SandboxConfigOutput.image` | `ContainerSandbox.image` | reshaped | containers only |
| `SandboxConfigOutput.engine` | `ContainerSandbox.engine` | reshaped | containers only |
| `SandboxConfigOutput.allowNetwork` | `NamespaceSandbox.allowsNetwork`, `ContainerSandbox.allowsNetwork` | reshaped | meaningless with no sandbox |
| `SandboxConfigOutput.mounts` | `ContainerSandbox.mounts` | reshaped | namespaces take none |
| `SandboxConfigOutput.keepWarm` | `ContainerSandbox.keptWarm` | reshaped | containers only |
| `SandboxConfigOutput.onUnavailable` | `NamespaceSandbox.whenUnavailable`, `ContainerSandbox.whenUnavailable` | reshaped | on the mechanisms that can be missing |

### `SandboxKind`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SandboxKind` | — | dropped | the `Sandbox` implementer is the kind |
| `SandboxKind.NONE` | `HostExecution` | reshaped | a type each |
| `SandboxKind.NAMESPACE` | `NamespaceSandbox` | reshaped | a type each |
| `SandboxKind.CONTAINER` | `ContainerSandbox` | reshaped | a type each |

### `SandboxKindFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SandboxKindFilter` | — | dropped | `SandboxKind` went |
| `SandboxKindFilter.eq` | — | dropped | follows `SandboxKind` |
| `SandboxKindFilter.ne` | — | dropped | follows `SandboxKind` |
| `SandboxKindFilter.in` | — | dropped | follows `SandboxKind` |
| `SandboxKindFilter.notIn` | — | dropped | follows `SandboxKind` |
| `SandboxKindFilter.isNull` | — | dropped | follows its filter |

### `SandboxUnavailable`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SandboxUnavailable` | `SandboxUnavailable` | reshaped | values say what happens |
| `SandboxUnavailable.ERROR` | `SandboxUnavailable.FAIL_SPAWN` | renamed | says what happens |
| `SandboxUnavailable.WARN` | `SandboxUnavailable.RUN_ON_HOST` | renamed | says what happens: commands run unsandboxed |

### `SandboxUnavailableFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SandboxUnavailableFilter` | — | dropped | nothing filters on `SandboxUnavailable` in v2 |
| `SandboxUnavailableFilter.eq` | — | dropped | follows `SandboxUnavailable` |
| `SandboxUnavailableFilter.ne` | — | dropped | follows `SandboxUnavailable` |
| `SandboxUnavailableFilter.in` | — | dropped | follows `SandboxUnavailable` |
| `SandboxUnavailableFilter.notIn` | — | dropped | follows `SandboxUnavailable` |
| `SandboxUnavailableFilter.isNull` | — | dropped | follows its filter |

### `ScriptConnection` → `ExtensionConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ScriptConnection` | `ExtensionConnection` | renamed | follows `Extension` |
| `ScriptConnection.results` | `ExtensionConnection.results` | renamed |  |
| `ScriptConnection.cursor` | `ExtensionConnection.cursor` | renamed |  |
| `ScriptConnection.total` | `ExtensionConnection.total` | renamed |  |

### `ScriptInput` → `ExtensionFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ScriptInput` | `ExtensionFilter` | renamed | follows `Extension` |
| `ScriptInput.id` | `ExtensionFilter.id` | renamed | follows `Extension.id` |
| `ScriptInput.kind` | — | dropped | follows `Script.kind`, which went |
| `ScriptInput.name` | `ExtensionFilter.relativePath` | renamed | follows `Extension.relativePath` |
| `ScriptInput.scope` | — | dropped | follows `Script.scope`, which went |
| `ScriptInput.blueprintName` | — | dropped | follows `Script.blueprintName`, which went |
| `ScriptInput.path` | `ExtensionFilter.path` | renamed | follows `Extension.path` |
| `ScriptInput.relativePath` | `ExtensionFilter.relativePath` | renamed | follows `Extension.relativePath` |
| `ScriptInput.isDeclared` | — | dropped | follows `UnclaimedFile`, which v2 does not filter on |
| `ScriptInput.compiles` | `ExtensionFilter.compileError` | reshaped | now `StringFilter`; follows `Extension.compileError` |
| `ScriptInput.compileError` | `ExtensionFilter.compileError` | renamed | follows `Extension.compileError` |
| `ScriptInput.and` | `ExtensionFilter.and` | reshaped | now `[ExtensionFilter]` |
| `ScriptInput.or` | `ExtensionFilter.or` | reshaped | now `[ExtensionFilter]` |
| `ScriptInput.not` | `ExtensionFilter.not` | reshaped | now `ExtensionFilter` |
| `ScriptInput.isNull` | `ExtensionFilter.isNull` | kept |  |

### `ScriptKind`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ScriptKind` | — | dropped | a kind enum on an output; the `Extension` implementer is the kind |
| `ScriptKind.TOOL` | `ToolExtension` | reshaped | a type per extension point; a file nothing names is a state, not a role |
| `ScriptKind.REGION_HOOK` | `RegionHook` | reshaped | a type per extension point; a file nothing names is a state, not a role |
| `ScriptKind.STAGE_HOOK` | `StageHook` | reshaped | a type per extension point; a file nothing names is a state, not a role |
| `ScriptKind.OUTPUT_VALIDATOR` | `OutputValidator` | reshaped | a type per extension point; a file nothing names is a state, not a role |
| `ScriptKind.MIME_CHECK` | `MimeCheck` | reshaped | a type per extension point; a file nothing names is a state, not a role |
| `ScriptKind.PROVIDER` | `ProviderExtension` | reshaped | a type per extension point; a file nothing names is a state, not a role |
| `ScriptKind.CANDIDATE` | `UnclaimedFile` | reshaped | a type per extension point; a file nothing names is a state, not a role |

### `ScriptKindFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ScriptKindFilter` | — | dropped | `ScriptKind` went |
| `ScriptKindFilter.eq` | — | dropped | follows `ScriptKind` |
| `ScriptKindFilter.ne` | — | dropped | follows `ScriptKind` |
| `ScriptKindFilter.in` | — | dropped | follows `ScriptKind` |
| `ScriptKindFilter.notIn` | — | dropped | follows `ScriptKind` |
| `ScriptKindFilter.isNull` | — | dropped | follows its filter |

### `ScriptOrder` → `ExtensionOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ScriptOrder` | `ExtensionOrder` | renamed | follows `Extension` |
| `ScriptOrder.field` | `ExtensionOrder.field` | reshaped | now `ExtensionOrderField` |
| `ScriptOrder.direction` | `ExtensionOrder.direction` | renamed |  |

### `ScriptOrderField` → `ExtensionOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ScriptOrderField` | `ExtensionOrderField` | renamed | follows `Extension` |
| `ScriptOrderField.ID` | `ExtensionOrderField.ID` | kept |  |
| `ScriptOrderField.NAME` | — | dropped | not a sort key in v2 |
| `ScriptOrderField.BLUEPRINT_NAME` | — | dropped | not a sort key in v2 |

### `ScriptOutput` → `Extension`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ScriptOutput` | `Extension` | reshaped | split into one type per extension point (`ToolExtension`, `StageHook`, ...) behind `Extension`; `kind` decided the fields |
| `ScriptOutput.id` | `Extension.id` | reshaped | `<typeName>[@<blueprint>]:<relativePath>` |
| `ScriptOutput.kind` | `__typename` | dropped | a kind field: `__typename` says it |
| `ScriptOutput.name` | `Extension.relativePath` | merged | one key per file: its path relative to its home directory |
| `ScriptOutput.scope` | `ToolExtension.blueprint`, `MimeCheck.blueprint` | reshaped | each type says which owners are legal; null is machine-wide |
| `ScriptOutput.blueprintName` | `ToolExtension.blueprint`, `StageHook.blueprint` | reshaped | a relation, not a name |
| `ScriptOutput.path` | `Extension.path` | kept |  |
| `ScriptOutput.relativePath` | `Extension.relativePath` | kept |  |
| `ScriptOutput.isDeclared` | `UnclaimedFile` | dropped | always true: files nothing names are `UnclaimedFile`s |
| `ScriptOutput.compiles` | `Extension.compileError` | merged | null is compiles; two fields could disagree |
| `ScriptOutput.compileError` | `Extension.compileError` | kept |  |
| `ScriptOutput.content` | `ExtensionRevision.content` | reshaped | the text is on the revision (`Extension.revision`) |

### `ScriptRef` → `ExtensionRef`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ScriptRef` | `ExtensionRef` | reshaped | one `@oneOf` field per extension point, so an illegal owner cannot be written |
| `ScriptRef.kind` | `ExtensionRef` | reshaped | the `@oneOf` field chosen |
| `ScriptRef.name` | `ScopedFileRef.relativePath` | renamed | the path relative to its home |
| `ScriptRef.blueprintName` | `ScopedFileRef.blueprintName` | reshaped | required on `BlueprintFileRef`, absent on `MachineFileRef` |

### `ScriptScope`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ScriptScope` | — | dropped | replaced by each implementer's `blueprint` relation |
| `ScriptScope.GLOBAL` | `blueprint` null | dropped | the `blueprint` relation |
| `ScriptScope.BLUEPRINT` | `ToolExtension.blueprint` | renamed | the `blueprint` relation |

### `ScriptScopeFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ScriptScopeFilter` | — | dropped | `ScriptScope` went |
| `ScriptScopeFilter.eq` | — | dropped | follows `ScriptScope` |
| `ScriptScopeFilter.ne` | — | dropped | follows `ScriptScope` |
| `ScriptScopeFilter.in` | — | dropped | follows `ScriptScope` |
| `ScriptScopeFilter.notIn` | — | dropped | follows `ScriptScope` |
| `ScriptScopeFilter.isNull` | — | dropped | follows its filter |

### `ScriptToolOutput` → `CustomTool`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ScriptToolOutput` | `CustomTool` | reshaped | a type is never named after its language; now linked to its file (`CustomTool.extension`) |
| `ScriptToolOutput.name` | `CustomTool.name` | kept |  |
| `ScriptToolOutput.description` | `ToolRevision.description` | reshaped | on `CustomTool.revision` |
| `ScriptToolOutput.arguments` | `ToolRevision.arguments` | reshaped | on `CustomTool.revision` |
| `ScriptToolOutput.origin` | `__typename` | dropped | a kind field: `__typename` says it |
| `ScriptToolOutput.path` | `CustomTool.extension` (`Extension.path`) | merged | on its file |
| `ScriptToolOutput.blueprint` | `CustomTool.extension` (`ToolExtension.blueprint`) | reshaped | a relation, on its file |
| `ScriptToolOutput.requires` | `CustomTool.requiredCapabilities` | renamed | plain English; old name kept one release, deprecated |

### `ScriptVerdictOutput` → `Validation`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ScriptVerdictOutput` | `Validation` | merged | one verdict type for every `validate...` query |
| `ScriptVerdictOutput.valid` | `Validation.valid` | kept |  |
| `ScriptVerdictOutput.error` | `Validation.problems` | reshaped | a `TextProblem` with line and column |

### `SearchScope`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SearchScope` | `SearchScope` | reshaped | values named for what they search |
| `SearchScope.META` | `SearchScope.FIELDS` | renamed | run fields and metadata |
| `SearchScope.FILES` | `SearchScope.CHANGED_FILES` | renamed | the recorded changed files |
| `SearchScope.CONTEXT` | `SearchScope.CONTEXT` | kept |  |
| `SearchScope.LOGS` | `SearchScope.LOGS` | kept |  |
| `SearchScope.JOURNAL` | `SearchScope.JOURNAL` | kept |  |

### `SeedFromCallerInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedFromCallerInput` | — | dropped | nothing filters on `SeedFromCaller` in v2 |
| `SeedFromCallerInput.key` | — | dropped | follows `SeedFromCaller.key`, which v2 does not filter on |
| `SeedFromCallerInput.and` | — | dropped | follows its filter |
| `SeedFromCallerInput.or` | — | dropped | follows its filter |
| `SeedFromCallerInput.not` | — | dropped | follows its filter |
| `SeedFromCallerInput.isNull` | — | dropped | follows its filter |

### `SeedFromCallerOutput` → `SeedFromCaller`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedFromCallerOutput` | `SeedFromCaller` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `SeedFromCallerOutput.key` | `SeedFromCaller.key` | kept |  |

### `SeedFromCommandInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedFromCommandInput` | — | dropped | nothing filters on `SeedFromCommand` in v2 |
| `SeedFromCommandInput.command` | — | dropped | follows `SeedFromCommand.command`, which v2 does not filter on |
| `SeedFromCommandInput.and` | — | dropped | follows its filter |
| `SeedFromCommandInput.or` | — | dropped | follows its filter |
| `SeedFromCommandInput.not` | — | dropped | follows its filter |
| `SeedFromCommandInput.isNull` | — | dropped | follows its filter |

### `SeedFromCommandOutput` → `SeedFromCommand`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedFromCommandOutput` | `SeedFromCommand` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `SeedFromCommandOutput.command` | `SeedFromCommand.command` | kept |  |

### `SeedFromFilesInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedFromFilesInput` | — | dropped | nothing filters on `SeedFromFiles` in v2 |
| `SeedFromFilesInput.paths` | — | dropped | follows `SeedFromFiles.paths`, which v2 does not filter on |
| `SeedFromFilesInput.and` | — | dropped | follows its filter |
| `SeedFromFilesInput.or` | — | dropped | follows its filter |
| `SeedFromFilesInput.not` | — | dropped | follows its filter |
| `SeedFromFilesInput.isNull` | — | dropped | follows its filter |

### `SeedFromFilesOutput` → `SeedFromFiles`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedFromFilesOutput` | `SeedFromFiles` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `SeedFromFilesOutput.paths` | `SeedFromFiles.paths` | kept |  |

### `SeedFromGlobInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedFromGlobInput` | — | dropped | nothing filters on `SeedFromGlob` in v2 |
| `SeedFromGlobInput.pattern` | — | dropped | follows `SeedFromGlob.pattern`, which v2 does not filter on |
| `SeedFromGlobInput.and` | — | dropped | follows its filter |
| `SeedFromGlobInput.or` | — | dropped | follows its filter |
| `SeedFromGlobInput.not` | — | dropped | follows its filter |
| `SeedFromGlobInput.isNull` | — | dropped | follows its filter |

### `SeedFromGlobOutput` → `SeedFromGlob`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedFromGlobOutput` | `SeedFromGlob` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `SeedFromGlobOutput.pattern` | `SeedFromGlob.pattern` | kept |  |

### `SeedFromLiteralInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedFromLiteralInput` | — | dropped | nothing filters on `SeedFromLiteral` in v2 |
| `SeedFromLiteralInput.text` | — | dropped | follows `SeedFromLiteral.text`, which v2 does not filter on |
| `SeedFromLiteralInput.and` | — | dropped | follows its filter |
| `SeedFromLiteralInput.or` | — | dropped | follows its filter |
| `SeedFromLiteralInput.not` | — | dropped | follows its filter |
| `SeedFromLiteralInput.isNull` | — | dropped | follows its filter |

### `SeedFromLiteralOutput` → `SeedFromLiteral`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedFromLiteralOutput` | `SeedFromLiteral` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `SeedFromLiteralOutput.text` | `SeedFromLiteral.text` | kept |  |

### `SeedFromScriptInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedFromScriptInput` | — | dropped | nothing filters on `SeedFromScript` in v2 |
| `SeedFromScriptInput.script` | — | dropped | follows `SeedFromScript.path`, which v2 does not filter on |
| `SeedFromScriptInput.and` | — | dropped | follows its filter |
| `SeedFromScriptInput.or` | — | dropped | follows its filter |
| `SeedFromScriptInput.not` | — | dropped | follows its filter |
| `SeedFromScriptInput.isNull` | — | dropped | follows its filter |

### `SeedFromScriptOutput` → `SeedFromScript`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedFromScriptOutput` | `SeedFromScript` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `SeedFromScriptOutput.script` | `SeedFromScript.path` | renamed | a path in the working directory, not a blueprint extension; old name kept one release, deprecated |

### `SeedFromToolsInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedFromToolsInput` | — | dropped | nothing filters on `SeedFromTools` in v2 |
| `SeedFromToolsInput.calls` | — | dropped | follows `SeedFromTools.calls`, which v2 does not filter on |
| `SeedFromToolsInput.refresh` | — | dropped | follows `SeedFromTools.refresh`, which v2 does not filter on |
| `SeedFromToolsInput.and` | — | dropped | follows its filter |
| `SeedFromToolsInput.or` | — | dropped | follows its filter |
| `SeedFromToolsInput.not` | — | dropped | follows its filter |
| `SeedFromToolsInput.isNull` | — | dropped | follows its filter |

### `SeedFromToolsOutput` → `SeedFromTools`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedFromToolsOutput` | `SeedFromTools` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `SeedFromToolsOutput.calls` | `SeedFromTools.calls` | kept |  |
| `SeedFromToolsOutput.refresh` | `SeedFromTools.refresh` | kept |  |

### `SeedRefresh`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedRefresh` | `SeedRefresh` | kept |  |
| `SeedRefresh.ONCE` | `SeedRefresh.ONCE` | kept |  |
| `SeedRefresh.EACH_STAGE` | `SeedRefresh.EACH_STAGE` | kept |  |

### `SeedRefreshFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedRefreshFilter` | — | dropped | nothing filters on `SeedRefresh` in v2 |
| `SeedRefreshFilter.eq` | — | dropped | follows `SeedRefresh` |
| `SeedRefreshFilter.ne` | — | dropped | follows `SeedRefresh` |
| `SeedRefreshFilter.in` | — | dropped | follows `SeedRefresh` |
| `SeedRefreshFilter.notIn` | — | dropped | follows `SeedRefresh` |
| `SeedRefreshFilter.isNull` | — | dropped | follows its filter |

### `SeedToolCallInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedToolCallInput` | — | dropped | nothing filters on `SeedToolCall` in v2 |
| `SeedToolCallInput.tool` | — | dropped | follows `SeedToolCall.tool`, which v2 does not filter on |
| `SeedToolCallInput.args` | — | dropped | follows `SeedToolCall.arguments`, which v2 does not filter on |
| `SeedToolCallInput.and` | — | dropped | follows its filter |
| `SeedToolCallInput.or` | — | dropped | follows its filter |
| `SeedToolCallInput.not` | — | dropped | follows its filter |
| `SeedToolCallInput.isNull` | — | dropped | follows its filter |

### `SeedToolCallListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedToolCallListInput` | — | dropped | no v2 listing filters a list of `SeedToolCall` (now `SeedToolCall`) |
| `SeedToolCallListInput.some` | — | dropped | follows its list filter |
| `SeedToolCallListInput.every` | — | dropped | follows its list filter |
| `SeedToolCallListInput.none` | — | dropped | follows its list filter |
| `SeedToolCallListInput.isNull` | — | dropped | follows its list filter |

### `SeedToolCallOutput` → `SeedToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SeedToolCallOutput` | `SeedToolCall` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `SeedToolCallOutput.tool` | `SeedToolCall.tool` | reshaped | a `ToolByName` |
| `SeedToolCallOutput.args` | `SeedToolCall.arguments` | renamed | no abbreviations; old name kept one release, deprecated |

### `SendMessageRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SendMessageRequest` | `SendMessageRequest` | kept |  |
| `SendMessageRequest.runId` | `SendMessageRequest.runId` | kept |  |
| `SendMessageRequest.text` | `SendMessageRequest.text` | kept |  |
| `SendMessageRequest.region` | `SendMessageRequest.region` | kept |  |
| `SendMessageRequest.attachments` | `SendMessageRequest.attachments` | kept |  |

### `SendMessageResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SendMessageResult` | `SendMessageResult` | kept |  |
| `SendMessageResult.run` | `SendMessageResult.run` | kept |  |

### `SendToAgentArgsOutput` → `SendToAgentArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SendToAgentArgsOutput` | `SendToAgentArguments` | renamed | argument types are named `XArguments` |
| `SendToAgentArgsOutput.agentId` | `SendToAgentArguments.runId` | renamed | the run id the model wrote, with `run` resolving it |
| `SendToAgentArgsOutput.message` | `SendToAgentArguments.message` | kept |  |
| `SendToAgentArgsOutput.targetRegion` | `SendToAgentArguments.regionName` | renamed | a region name |

### `SendToAgentCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SendToAgentCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `SendToAgentCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `SendToAgentCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `SendToAgentCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `SendToAgentCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `SendToAgentArguments` for `send_to_agent` |

### `ServeLimitsOutput` → `ServerLimits`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ServeLimitsOutput` | `ServerLimits` | renamed | plain names |
| `ServeLimitsOutput.maxPageSize` | `ServerLimits.maxPageSize` | kept |  |
| `ServeLimitsOutput.maxIds` | `ServerLimits.maxIds` | kept |  |
| `ServeLimitsOutput.maxFileBytes` | `ServerLimits.maxFileBytes` | kept |  |
| `ServeLimitsOutput.maxListingEntries` | `ServerLimits.maxDirectoryEntries` | renamed | says which listing; old name kept one release, deprecated |
| `ServeLimitsOutput.maxSearchScan` | `ServerLimits.maxRunsSearched` | renamed | says what it counts; old name kept one release, deprecated |
| `ServeLimitsOutput.maxHistoryLimit` | `ServerLimits.maxContextSnapshotsPage` | renamed | says which listing; old name kept one release, deprecated |
| `ServeLimitsOutput.maxConcurrentRequests` | `ServerLimits.maxConcurrentRequests` | reshaped | now `Int` |
| `ServeLimitsOutput.maxUploadBytes` | `ServerLimits.maxUploadBytes` | kept |  |
| `ServeLimitsOutput.requestTimeoutSecs` | `ServerLimits.requestTimeoutSeconds` | renamed | no abbreviations; old name kept one release, deprecated |

### `ServerInfoOutput` → `Server`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ServerInfoOutput` | `Server` | reshaped | the server is its own root |
| `ServerInfoOutput.apiVersion` | `Server.version` | renamed | the build version, not an API contract version; old name kept one release, deprecated |
| `ServerInfoOutput.capabilities` | `Server.restCapabilities` | renamed | REST route flags; GraphQL clients use introspection; old name kept one release, deprecated |
| `ServerInfoOutput.isAdminEnabled` | `Server.isAdminEnabled` | kept |  |
| `ServerInfoOutput.limits` | `Server.limits` | reshaped | a `ServerLimits` |

### `SettlementInput` → `SettlementFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SettlementInput` | `SettlementFilter` | kept |  |
| `SettlementInput.outcome` | — | dropped | follows `Settlement.outcome`, which went |
| `SettlementInput.approved` | `SettlementFilter.approved` | reshaped | now `ApprovedFilter` |
| `SettlementInput.scope` | `ApprovedFilter.scope` | renamed | follows `Approved.scope` |
| `SettlementInput.choice` | `OptionChosenFilter.index` | renamed | follows `OptionChosen.index` |
| `SettlementInput.text` | `TextGivenFilter.text` | renamed | follows `TextGiven.text` |
| `SettlementInput.feedback` | — | dropped | follows `Settlement.feedback`, which went |
| `SettlementInput.and` | `SettlementFilter.and` | kept |  |
| `SettlementInput.or` | `SettlementFilter.or` | kept |  |
| `SettlementInput.not` | `SettlementFilter.not` | kept |  |
| `SettlementInput.isNull` | `SettlementFilter.isNull` | kept |  |

### `SettlementOutcome`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SettlementOutcome` | — | dropped | the `Settlement` implementer is the outcome; `NotAnswered.why` for the three no-answer cases |
| `SettlementOutcome.ANSWERED` | `Approved`, `Denied`, `TextGiven`, `OptionChosen`, `Confirmed`, `Declined` | dropped | the settlement type says how it was answered |
| `SettlementOutcome.TIMED_OUT` | `NoAnswerReason.TIMED_OUT` | renamed | on `NotAnswered.why` |
| `SettlementOutcome.CANCELLED` | `NoAnswerReason.WITHDRAWN` | renamed | the run was cancelled or its asker went away |
| `SettlementOutcome.REFUSED` | `NoAnswerReason.DUPLICATE_ID` | renamed | says what refused it |

### `SettlementOutcomeFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SettlementOutcomeFilter` | — | dropped | `SettlementOutcome` went |
| `SettlementOutcomeFilter.eq` | — | dropped | follows `SettlementOutcome` |
| `SettlementOutcomeFilter.ne` | — | dropped | follows `SettlementOutcome` |
| `SettlementOutcomeFilter.in` | — | dropped | follows `SettlementOutcome` |
| `SettlementOutcomeFilter.notIn` | — | dropped | follows `SettlementOutcome` |
| `SettlementOutcomeFilter.isNull` | — | dropped | follows its filter |

### `SettlementOutput` → `Settlement`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SettlementOutput` | `Settlement` | reshaped | an interface with one type per ending, narrowed per interaction type |
| `SettlementOutput.outcome` | `__typename` | dropped | the `Settlement` type is the outcome |
| `SettlementOutput.approved` | `Approved`, `Denied` | dropped | two types |
| `SettlementOutput.scope` | `Approved.scope` | reshaped | only an approval has a scope |
| `SettlementOutput.choice` | `OptionChosen.index` | reshaped | with the option's text in `OptionChosen.option` |
| `SettlementOutput.text` | `TextGiven.text` | reshaped | only a text answer has text |
| `SettlementOutput.feedback` | `Denied.feedback`, `Declined.feedback` | reshaped | only a refusal has feedback |

### `SetupBlocker`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SetupBlocker` | `SetupBlocker` | kept |  |
| `SetupBlocker.PROVIDER_MISSING` | `SetupBlocker.PROVIDER_MISSING` | kept |  |
| `SetupBlocker.CREDITS_EXHAUSTED` | `SetupBlocker.CREDITS_EXHAUSTED` | kept |  |
| `SetupBlocker.AUTH_FAILED` | `SetupBlocker.AUTH_FAILED` | kept |  |
| `SetupBlocker.FORBIDDEN` | `SetupBlocker.FORBIDDEN` | kept |  |
| `SetupBlocker.PROVIDERS_UNAVAILABLE` | `SetupBlocker.PROVIDERS_UNAVAILABLE` | kept |  |
| `SetupBlocker.PROVIDER_UNREACHABLE` | `SetupBlocker.PROVIDER_UNREACHABLE` | kept |  |
| `SetupBlocker.PROVIDER_TIMED_OUT` | `SetupBlocker.PROVIDER_TIMED_OUT` | kept |  |
| `SetupBlocker.PROVIDER_FAILED` | `SetupBlocker.PROVIDER_FAILED` | kept |  |

### `SetupBlockerFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SetupBlockerFilter` | `SetupBlockerFilter` | kept |  |
| `SetupBlockerFilter.eq` | `SetupBlockerFilter.eq` | kept |  |
| `SetupBlockerFilter.ne` | `SetupBlockerFilter.ne` | kept |  |
| `SetupBlockerFilter.in` | `SetupBlockerFilter.in` | kept |  |
| `SetupBlockerFilter.notIn` | `SetupBlockerFilter.notIn` | kept |  |
| `SetupBlockerFilter.isNull` | `SetupBlockerFilter.isNull` | kept |  |

### `ShellArgsOutput` → `ShellArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ShellArgsOutput` | `ShellArguments` | renamed | argument types are named `XArguments` |
| `ShellArgsOutput.command` | `ShellArguments.command` | kept |  |

### `ShellCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ShellCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `ShellCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `ShellCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `ShellCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `ShellCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `ShellArguments` for `shell` |

### `ShellRuleInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ShellRuleInput` | — | dropped | nothing filters on `ShellRule` in v2 |
| `ShellRuleInput.command` | — | dropped | follows `ShellRule.command`, which v2 does not filter on |
| `ShellRuleInput.args` | — | dropped | follows `ShellRule.args`, which v2 does not filter on |
| `ShellRuleInput.and` | — | dropped | follows its filter |
| `ShellRuleInput.or` | — | dropped | follows its filter |
| `ShellRuleInput.not` | — | dropped | follows its filter |
| `ShellRuleInput.isNull` | — | dropped | follows its filter |

### `ShellRuleListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ShellRuleListInput` | — | dropped | no v2 listing filters a list of `ShellRule` (now `ShellRule`) |
| `ShellRuleListInput.some` | — | dropped | follows its list filter |
| `ShellRuleListInput.every` | — | dropped | follows its list filter |
| `ShellRuleListInput.none` | — | dropped | follows its list filter |
| `ShellRuleListInput.isNull` | — | dropped | follows its list filter |

### `ShellRuleOutput` → `ShellRule`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ShellRuleOutput` | `ShellRule` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `ShellRuleOutput.command` | `ShellRule.command` | kept |  |
| `ShellRuleOutput.args` | `ShellRule.args` | kept |  |

### `ShellRuleWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ShellRuleWrite` | `ShellRuleWrite` | kept |  |
| `ShellRuleWrite.command` | `ShellRuleWrite.command` | kept |  |
| `ShellRuleWrite.args` | `ShellRuleWrite.args` | kept |  |

### `SignInMcpServerRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SignInMcpServerRequest` | `SignInMcpServerRequest` | kept |  |
| `SignInMcpServerRequest.name` | `SignInMcpServerRequest.name` | kept |  |

### `SignInMcpServerResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SignInMcpServerResult` | `SignInMcpServerResult` | kept |  |
| `SignInMcpServerResult.mcpServer` | `SignInMcpServerResult.mcpServer` | reshaped | now `HttpMcpServer` |
| `SignInMcpServerResult.status` | `SignInMcpServerResult.outcome` | renamed | an outcome; old name kept one release, deprecated |

### `SignInProviderRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SignInProviderRequest` | `SignInProviderRequest` | kept |  |
| `SignInProviderRequest.provider` | `SignInProviderRequest.providerName` | renamed | a name |

### `SignInProviderResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SignInProviderResult` | `SignInProviderResult` | kept |  |
| `SignInProviderResult.provider` | `SignInProviderResult.provider` | kept |  |
| `SignInProviderResult.authorizeUrl` | `SignInProviderResult.authorizeUrl` | kept |  |
| `SignInProviderResult.isAlreadyWaiting` | `SignInProviderResult.alreadyInState` | renamed | the retry signal every write uses; old name kept one release, deprecated |

### `SignOutProviderRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SignOutProviderRequest` | `SignOutProviderRequest` | kept |  |
| `SignOutProviderRequest.provider` | `SignOutProviderRequest.providerName` | renamed | a name |

### `SignOutProviderResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SignOutProviderResult` | `SignOutProviderResult` | kept |  |
| `SignOutProviderResult.provider` | `SignOutProviderResult.provider` | kept |  |

### `SkippedOutput` → `SweepItem`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SkippedOutput` | `SweepItem` | dropped | deprecated since sweep items landed; each run answers as a `SweepItem` type |
| `SkippedOutput.id` | `SweepItem.runId` | renamed | every sweep item names its run |
| `SkippedOutput.reason` | `__typename` | dropped | the `SweepItem` type |
| `SkippedOutput.message` | `SweepRefused.message`, `SweepOutcomeUnknown.message` | merged | on the items that have something to say |

### `SkippedToolOutput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SkippedToolOutput` | — | dropped | a skipped file is a `ToolExtension` with `compileError` or `nameTakenBy` set |
| `SkippedToolOutput.path` | `ToolExtension.path` | merged | read with `extensions` |
| `SkippedToolOutput.reason` | `ToolExtension.compileError`, `ToolExtension.nameTakenBy` | reshaped | two reasons, typed |

### `SkipReason`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SkipReason` | — | dropped | deprecated with `Skipped`; each run now answers as a `SweepItem` type |
| `SkipReason.STILL_RUNNING` | `SweepRefused` with `ErrorReason.RUN_STILL_LIVE` | dropped | a `SweepItem` type |
| `SkipReason.RECORD_UNREADABLE` | `SweepRefused` with `ErrorReason.RECORD_UNREADABLE` | dropped | a `SweepItem` type |
| `SkipReason.ALREADY_FINISHED` | `SweepRefused` with `ErrorReason.ALREADY_FINISHED` | dropped | a `SweepItem` type |
| `SkipReason.OTHER` | `SweepRefused` | reshaped | a `SweepItem` type |

### `SpawnAgentArgsOutput` → `SpawnAgentArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SpawnAgentArgsOutput` | `SpawnAgentArguments` | renamed | argument types are named `XArguments` |
| `SpawnAgentArgsOutput.blueprint` | `SpawnAgentArguments.blueprintName` | renamed | a name |
| `SpawnAgentArgsOutput.task` | `SpawnAgentArguments.task` | kept |  |
| `SpawnAgentArgsOutput.wait` | `SpawnAgentArguments.wait` | kept |  |
| `SpawnAgentArgsOutput.seedContext` | `SpawnAgentArguments.seedContext` | kept |  |
| `SpawnAgentArgsOutput.parts` | `SpawnAgentArguments.partNames` | renamed | names or hash prefixes |
| `SpawnAgentArgsOutput.maxChildDepth` | `SpawnAgentArguments.maxChildDepth` | kept |  |
| `SpawnAgentArgsOutput.outputFormat` | `SpawnAgentArguments.outputFormat` | kept |  |
| `SpawnAgentArgsOutput.outputInstructions` | `SpawnAgentArguments.outputInstructions` | kept |  |

### `SpawnAgentCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SpawnAgentCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `SpawnAgentCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `SpawnAgentCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `SpawnAgentCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `SpawnAgentCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `SpawnAgentArguments` for `spawn_agent` |

### `SpawnRunRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SpawnRunRequest` | `SpawnRunRequest` | kept |  |
| `SpawnRunRequest.blueprint` | `SpawnRunRequest.blueprint` | kept |  |
| `SpawnRunRequest.task` | `SpawnRunRequest.task` | kept |  |
| `SpawnRunRequest.model` | `SpawnRunRequest.model` | reshaped | now `ModelRef` |
| `SpawnRunRequest.maxDepth` | `SpawnRunRequest.maxDepth` | kept |  |
| `SpawnRunRequest.workdir` | `SpawnRunRequest.workingDirectory` | renamed | no abbreviations |
| `SpawnRunRequest.yolo` | `SpawnRunRequest.approvalPolicy` | reshaped | an `ApprovalPolicyRef`; bare `--yolo` is the policy named `"default"` |
| `SpawnRunRequest.allowTools` | `SpawnRunRequest.allowTools` | kept |  |
| `SpawnRunRequest.skipSeedCommands` | `SpawnRunRequest.skipSeedCommands` | kept |  |
| `SpawnRunRequest.captureModelInput` | `SpawnRunRequest.captureModelInput` | kept |  |
| `SpawnRunRequest.regions` | `SpawnRunRequest.regions` | kept |  |
| `SpawnRunRequest.metadata` | `SpawnRunRequest.metadata` | kept |  |
| `SpawnRunRequest.output` | `SpawnRunRequest.output` | kept |  |
| `SpawnRunRequest.callback` | `SpawnRunRequest.callback` | kept |  |
| `SpawnRunRequest.attachments` | `SpawnRunRequest.attachments` | kept |  |

### `SpawnRunResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SpawnRunResult` | `SpawnRunResult` | kept |  |
| `SpawnRunResult.run` | `SpawnRunResult.run` | kept |  |
| `SpawnRunResult.warnings` | `SpawnRunResult.warnings` | kept |  |

### `SpendThresholdCrossedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SpendThresholdCrossedEvent` | `SpendThresholdCrossedEvent` | kept |  |
| `SpendThresholdCrossedEvent.seq` | `SpendThresholdCrossedEvent.seq` | kept |  |
| `SpendThresholdCrossedEvent.at` | `SpendThresholdCrossedEvent.at` | kept |  |
| `SpendThresholdCrossedEvent.runId` | `SpendThresholdCrossedEvent.runId` | kept |  |
| `SpendThresholdCrossedEvent.agentId` | `RunEvent.runId` | dropped | the run's live agent id is not a `Run` fact; frames carry `runId` and `run` |
| `SpendThresholdCrossedEvent.thresholdUsd` | `SpendCrossing.thresholdUsd` | merged | one `SpendCrossing` type, also on `Run.spendCrossings` |
| `SpendThresholdCrossedEvent.totalUsd` | `SpendCrossing.spentUsd` | merged |  |
| `SpendThresholdCrossedEvent.complete` | `SpendCrossing.spentIsComplete` | merged |  |
| `SpendThresholdCrossedEvent.stage` | `SpendCrossing.stage` | merged | a `RunStage` |
| `SpendThresholdCrossedEvent.run` | `SpendThresholdCrossedEvent.run` | kept |  |

### `StageContextInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageContextInput` | — | dropped | nothing filters on `StageContext` in v2 |
| `StageContextInput.regions` | — | dropped | follows `StageContext.ownRegions`, which v2 does not filter on |
| `StageContextInput.hide` | — | dropped | follows `StageContext.hidden`, which v2 does not filter on |
| `StageContextInput.hideNames` | — | dropped | follows `StageContext.hidden`, which v2 does not filter on |
| `StageContextInput.reset` | — | dropped | follows `StageContext.reset`, which v2 does not filter on |
| `StageContextInput.resetNames` | — | dropped | follows `StageContext.reset`, which v2 does not filter on |
| `StageContextInput.and` | — | dropped | follows its filter |
| `StageContextInput.or` | — | dropped | follows its filter |
| `StageContextInput.not` | — | dropped | follows its filter |
| `StageContextInput.isNull` | — | dropped | follows its filter |

### `StageContextOutput` → `StageContext`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageContextOutput` | `StageContext` | reshaped | relations only; the name twins go |
| `StageContextOutput.regions` | `StageContext.ownRegions` | reshaped | `DeclaredRegion`s this stage's layout declares |
| `StageContextOutput.hide` | `StageContext.hidden` | reshaped | relations |
| `StageContextOutput.hideNames` | `StageContext.hidden` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `StageContextOutput.reset` | `StageContext.reset` | kept |  |
| `StageContextOutput.resetNames` | `StageContext.reset` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |

### `StageHooksInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageHooksInput` | — | dropped | nothing filters on `StageHooks` in v2 |
| `StageHooksInput.onStageEnter` | — | dropped | follows `StageHooks.onStageEnter`, which v2 does not filter on |
| `StageHooksInput.onStageExit` | — | dropped | follows `StageHooks.onStageExit`, which v2 does not filter on |
| `StageHooksInput.beforeInference` | — | dropped | follows `StageHooks.beforeInference`, which v2 does not filter on |
| `StageHooksInput.afterInference` | — | dropped | follows `StageHooks.afterInference`, which v2 does not filter on |
| `StageHooksInput.onToolCall` | — | dropped | follows `StageHooks.onToolCall`, which v2 does not filter on |
| `StageHooksInput.onCompletion` | — | dropped | follows `StageHooks.onCompletion`, which v2 does not filter on |
| `StageHooksInput.onError` | — | dropped | follows `StageHooks.onError`, which v2 does not filter on |
| `StageHooksInput.and` | — | dropped | follows its filter |
| `StageHooksInput.or` | — | dropped | follows its filter |
| `StageHooksInput.not` | — | dropped | follows its filter |
| `StageHooksInput.isNull` | — | dropped | follows its filter |

### `StageHooksOutput` → `StageHooks`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageHooksOutput` | `StageHooks` | reshaped | each hook is the file as the revision holds it; `onTerminal` added |
| `StageHooksOutput.onStageEnter` | `StageHooks.onStageEnter` | reshaped | now `ExtensionRevision` |
| `StageHooksOutput.onStageExit` | `StageHooks.onStageExit` | reshaped | now `ExtensionRevision` |
| `StageHooksOutput.beforeInference` | `StageHooks.beforeInference` | reshaped | now `ExtensionRevision` |
| `StageHooksOutput.afterInference` | `StageHooks.afterInference` | reshaped | now `ExtensionRevision` |
| `StageHooksOutput.onToolCall` | `StageHooks.onToolCall` | reshaped | now `ExtensionRevision` |
| `StageHooksOutput.onCompletion` | `StageHooks.onCompletion` | reshaped | now `ExtensionRevision` |
| `StageHooksOutput.onError` | `StageHooks.onError` | reshaped | now `ExtensionRevision` |

### `StageInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageInput` | — | dropped | nothing filters on `Stage` in v2 |
| `StageInput.name` | — | dropped | follows `Stage.name`, which v2 does not filter on |
| `StageInput.mode` | — | dropped | follows `Stage.mode`, which went |
| `StageInput.description` | — | dropped | follows `Stage.description`, which v2 does not filter on |
| `StageInput.transitionPrompt` | — | dropped | follows `Stage.transitionPrompt`, which v2 does not filter on |
| `StageInput.model` | — | dropped | follows `Stage.model`, which v2 does not filter on |
| `StageInput.availableTools` | — | dropped | follows `StageTools.grants`, which v2 does not filter on |
| `StageInput.requiredTools` | — | dropped | follows `StageTools.requiredTools`, which v2 does not filter on |
| `StageInput.availableConnectors` | — | dropped | follows `ToolsOfMcpServer`, which v2 does not filter on |
| `StageInput.maxIterations` | — | dropped | follows `Stage.maxIterations`, which went |
| `StageInput.maxRevisits` | — | dropped | follows `Stage.maxRevisits`, which v2 does not filter on |
| `StageInput.outputRequirement` | — | dropped | follows `Stage.outputRequired`, which v2 does not filter on |
| `StageInput.output` | — | dropped | follows `Stage.output`, which v2 does not filter on |
| `StageInput.input` | — | dropped | follows `Stage.input`, which v2 does not filter on |
| `StageInput.acceptsMessages` | — | dropped | follows `Stage.acceptsMessages`, which v2 does not filter on |
| `StageInput.allowComplete` | — | dropped | follows `Stage.mayEndRun`, which v2 does not filter on |
| `StageInput.allowAsWorker` | — | dropped | follows `Stage.mayRunAsWorker`, which v2 does not filter on |
| `StageInput.requiresChildren` | — | dropped | follows `Stage.waitsForChildren`, which v2 does not filter on |
| `StageInput.declaresBlockingTools` | — | dropped | follows `StageTools.offersBlockingToolsOnPurpose`, which v2 does not filter on |
| `StageInput.toolGuidance` | — | dropped | follows `Stage.toolGuidance`, which went |
| `StageInput.toolPermissions` | — | dropped | follows `StageTools.permissions`, which v2 does not filter on |
| `StageInput.toolAccepts` | — | dropped | follows `StageTools.accepts`, which v2 does not filter on |
| `StageInput.toolRouting` | — | dropped | follows `StageTools.resultRouting`, which v2 does not filter on |
| `StageInput.outputRouting` | — | dropped | follows `Stage.partRoutes`, which v2 does not filter on |
| `StageInput.context` | — | dropped | follows `Stage.context`, which v2 does not filter on |
| `StageInput.nudge` | — | dropped | follows `SettingOverrides.nudge`, which v2 does not filter on |
| `StageInput.sandbox` | — | dropped | follows `SettingOverrides.sandbox`, which v2 does not filter on |
| `StageInput.security` | — | dropped | follows `SettingOverrides.tracksTaint`, which v2 does not filter on |
| `StageInput.hooks` | — | dropped | follows `Stage.hooks`, which v2 does not filter on |
| `StageInput.interactionPoints` | — | dropped | follows `InteractionPointStage.interactionPoints`, which v2 does not filter on |
| `StageInput.fanOut` | — | dropped | follows `FanOutStage`, which v2 does not filter on |
| `StageInput.transitions` | — | dropped | follows `Stage.transitions`, which v2 does not filter on |
| `StageInput.and` | — | dropped | follows its filter |
| `StageInput.or` | — | dropped | follows its filter |
| `StageInput.not` | — | dropped | follows its filter |
| `StageInput.isNull` | — | dropped | follows its filter |

### `StageListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageListInput` | — | dropped | no v2 listing filters a list of `Stage` (now `Stage`) |
| `StageListInput.some` | — | dropped | follows its list filter |
| `StageListInput.every` | — | dropped | follows its list filter |
| `StageListInput.none` | — | dropped | follows its list filter |
| `StageListInput.isNull` | — | dropped | follows its list filter |

### `StageMode`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageMode` | — | dropped | the `Stage` implementer is the mode; `interactive` does nothing at run time and reads as `AutonomousStage` |
| `StageMode.AUTONOMOUS` | `AutonomousStage` | reshaped | a type per mode |
| `StageMode.INTERACTIVE` | `AutonomousStage` | dropped | does nothing at run time; read as autonomous, with the `BlueprintRule.INTERACTIVE_MODE_HAS_NO_EFFECT` warning |
| `StageMode.INTERACTIVE_POINTS` | `InteractionPointStage` | reshaped | a type per mode |
| `StageMode.FAN_OUT` | `FanOutStage` | reshaped | a type per mode |
| `StageMode.OUTPUT` | `OutputStage` | reshaped | a type per mode |

### `StageModeFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageModeFilter` | — | dropped | `StageMode` went |
| `StageModeFilter.eq` | — | dropped | follows `StageMode` |
| `StageModeFilter.ne` | — | dropped | follows `StageMode` |
| `StageModeFilter.in` | — | dropped | follows `StageMode` |
| `StageModeFilter.notIn` | — | dropped | follows `StageMode` |
| `StageModeFilter.isNull` | — | dropped | follows its filter |

### `StageModelConfigInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageModelConfigInput` | — | dropped | nothing filters on `StageModel` in v2 |
| `StageModelConfigInput.models` | — | dropped | follows `StageModel.choices`, which v2 does not filter on |
| `StageModelConfigInput.allowUserDefault` | — | dropped | follows `StageModel.fallsBackToMachineDefault`, which v2 does not filter on |
| `StageModelConfigInput.parameters` | — | dropped | follows `StageModel.parameters`, which v2 does not filter on |
| `StageModelConfigInput.requestTimeoutSecs` | — | dropped | follows `StageModel.requestTimeoutSeconds`, which v2 does not filter on |
| `StageModelConfigInput.and` | — | dropped | follows its filter |
| `StageModelConfigInput.or` | — | dropped | follows its filter |
| `StageModelConfigInput.not` | — | dropped | follows its filter |
| `StageModelConfigInput.isNull` | — | dropped | follows its filter |

### `StageModelConfigOutput` → `StageModel`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageModelConfigOutput` | `StageModel` | renamed | no storage words in type names |
| `StageModelConfigOutput.models` | `StageModel.choices` | reshaped | `ModelChoice`s |
| `StageModelConfigOutput.allowUserDefault` | `StageModel.fallsBackToMachineDefault` | renamed | says what it does; old name kept one release, deprecated |
| `StageModelConfigOutput.parameters` | `StageModel.parameters` | kept |  |
| `StageModelConfigOutput.requestTimeoutSecs` | `StageModel.requestTimeoutSeconds` | renamed | no abbreviations; old name kept one release, deprecated |

### `StageModelRouteInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageModelRouteInput` | — | dropped | nothing filters on `ModelChoice` in v2 |
| `StageModelRouteInput.provider` | — | dropped | follows `ModelChoice.providerName`, which v2 does not filter on |
| `StageModelRouteInput.model` | — | dropped | follows `ModelChoice.modelName`, which v2 does not filter on |
| `StageModelRouteInput.and` | — | dropped | follows its filter |
| `StageModelRouteInput.or` | — | dropped | follows its filter |
| `StageModelRouteInput.not` | — | dropped | follows its filter |
| `StageModelRouteInput.isNull` | — | dropped | follows its filter |

### `StageModelRouteListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageModelRouteListInput` | — | dropped | no v2 listing filters a list of `StageModelRoute` (now `ModelChoice`) |
| `StageModelRouteListInput.some` | — | dropped | follows its list filter |
| `StageModelRouteListInput.every` | — | dropped | follows its list filter |
| `StageModelRouteListInput.none` | — | dropped | follows its list filter |
| `StageModelRouteListInput.isNull` | — | dropped | follows its list filter |

### `StageModelRouteOutput` → `ModelChoice`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageModelRouteOutput` | `ModelChoice` | renamed | one provider and model, shared with the config's routing |
| `StageModelRouteOutput.provider` | `ModelChoice.providerName` | renamed | now nullable; a name; old name kept one release, deprecated |
| `StageModelRouteOutput.model` | `ModelChoice.modelName` | renamed | a name; old name kept one release, deprecated |

### `StageModelUseInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageModelUseInput` | — | dropped | nothing filters on `ModelUse` in v2 |
| `StageModelUseInput.provider` | — | dropped | follows `ModelUse.providerName`, which v2 does not filter on |
| `StageModelUseInput.model` | — | dropped | follows `ModelUse.modelName`, which v2 does not filter on |
| `StageModelUseInput.and` | — | dropped | follows its filter |
| `StageModelUseInput.or` | — | dropped | follows its filter |
| `StageModelUseInput.not` | — | dropped | follows its filter |
| `StageModelUseInput.isNull` | — | dropped | follows its filter |

### `StageModelUseListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageModelUseListInput` | — | dropped | no v2 listing filters a list of `StageModelUse` (now `ModelUse`) |
| `StageModelUseListInput.some` | — | dropped | follows its list filter |
| `StageModelUseListInput.every` | — | dropped | follows its list filter |
| `StageModelUseListInput.none` | — | dropped | follows its list filter |
| `StageModelUseListInput.isNull` | — | dropped | follows its list filter |

### `StageModelUseOutput` → `ModelUse`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageModelUseOutput` | `ModelUse` | reshaped | names kept, plus the provider revision and model relations |
| `StageModelUseOutput.provider` | `ModelUse.providerName` | renamed | a name; `provider` is the revision it used |
| `StageModelUseOutput.model` | `ModelUse.modelName` | renamed | a name; `model` is the catalogue entry |

### `StageOutput` → `Stage`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageOutput` | `Stage` | reshaped | an interface with one type per mode (`AutonomousStage`, `OutputStage`, `InteractionPointStage`, `FanOutStage`) |
| `StageOutput.name` | `Stage.name` | kept |  |
| `StageOutput.mode` | `__typename` | reshaped | the `Stage` type: `AutonomousStage`, `OutputStage`, `InteractionPointStage`, `FanOutStage` |
| `StageOutput.description` | `Stage.description` | kept |  |
| `StageOutput.transitionPrompt` | `Stage.transitionPrompt` | kept |  |
| `StageOutput.model` | `Stage.model` | kept |  |
| `StageOutput.availableTools` | `StageTools.grants` | reshaped | typed `ToolSelector`s on `Stage.tools`, not strings with `@` tokens |
| `StageOutput.requiredTools` | `StageTools.requiredTools` | reshaped | `ToolByName`s |
| `StageOutput.availableConnectors` | `ToolsOfMcpServer` | reshaped | a `ToolSelector` in `StageTools.grants` |
| `StageOutput.maxIterations` | `AutonomousStage.maxIterations`, `OutputStage.maxIterations`, `InteractionPointStage.maxIterations` | reshaped | does not apply to a fan-out stage |
| `StageOutput.maxRevisits` | `Stage.maxRevisits` | kept |  |
| `StageOutput.outputRequirement` | `Stage.outputRequired` | reshaped | a boolean; the re-ask count was always 3 |
| `StageOutput.output` | `Stage.output` | kept |  |
| `StageOutput.input` | `Stage.input` | kept |  |
| `StageOutput.acceptsMessages` | `Stage.acceptsMessages` | kept |  |
| `StageOutput.allowComplete` | `Stage.mayEndRun` | renamed | says what it allows; old name kept one release, deprecated |
| `StageOutput.allowAsWorker` | `Stage.mayRunAsWorker` | renamed | says what it allows; old name kept one release, deprecated |
| `StageOutput.requiresChildren` | `Stage.waitsForChildren` | renamed | says what it does; old name kept one release, deprecated |
| `StageOutput.declaresBlockingTools` | `StageTools.offersBlockingToolsOnPurpose` | reshaped | on `Stage.tools` |
| `StageOutput.toolGuidance` | `SettingOverrides.suggestsBatchingCalls`, `SettingOverrides.suggestsShellForMultiStepWork` | merged | on `Stage.overrides` |
| `StageOutput.toolPermissions` | `StageTools.permissions` | reshaped | on `Stage.tools` |
| `StageOutput.toolAccepts` | `StageTools.accepts` | reshaped | on `Stage.tools` |
| `StageOutput.toolRouting` | `StageTools.resultRouting` | reshaped | a `ToolResultRouting`, on `Stage.tools` |
| `StageOutput.outputRouting` | `Stage.partRoutes` | reshaped | `PartRoute`s: it routes parts, not the output |
| `StageOutput.context` | `Stage.context` | kept |  |
| `StageOutput.nudge` | `SettingOverrides.nudge` | reshaped | on `Stage.overrides` |
| `StageOutput.sandbox` | `SettingOverrides.sandbox` | reshaped | a `Sandbox`, on `Stage.overrides` |
| `StageOutput.security` | `SettingOverrides.tracksTaint` | merged | on `Stage.overrides` |
| `StageOutput.hooks` | `Stage.hooks` | kept |  |
| `StageOutput.interactionPoints` | `InteractionPointStage.interactionPoints` | reshaped | only that stage type has them |
| `StageOutput.fanOut` | `FanOutStage` | merged | the settings are the fan-out stage's own fields |
| `StageOutput.effective` | `Stage.effective` | kept |  |
| `StageOutput.transitions` | `Stage.transitions` | reshaped | one type per condition; a stage with no table has a `FallThroughEdge`, not `[]` |

### `StagePartsInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StagePartsInput` | — | dropped | nothing filters on `StageInput` in v2 |
| `StagePartsInput.accepts` | — | dropped | follows `StageInput.accepts`, which v2 does not filter on |
| `StagePartsInput.asText` | — | dropped | follows `StageInput.asText`, which v2 does not filter on |
| `StagePartsInput.and` | — | dropped | follows its filter |
| `StagePartsInput.or` | — | dropped | follows its filter |
| `StagePartsInput.not` | — | dropped | follows its filter |
| `StagePartsInput.isNull` | — | dropped | follows its filter |

### `StagePartsOutput` → `StageInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StagePartsOutput` | `StageInput` | renamed | what a stage takes |
| `StagePartsOutput.accepts` | `StageInput.accepts` | kept |  |
| `StagePartsOutput.asText` | `StageInput.asText` | kept |  |

### `StageRecordConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageRecordConnection` | — | dropped | a plain list in v2, bounded by what holds it |
| `StageRecordConnection.results` | — | dropped | follows its connection |
| `StageRecordConnection.cursor` | — | dropped | follows its connection |
| `StageRecordConnection.total` | — | dropped | follows its connection |

### `StageRecordInput` → `RunStageFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageRecordInput` | `RunStageFilter` | renamed | follows `RunStage` |
| `StageRecordInput.name` | `RunStageFilter.name` | renamed | follows `RunStage.name` |
| `StageRecordInput.index` | `RunStageFilter.position` | renamed | follows `RunStage.position` |
| `StageRecordInput.status` | — | dropped | follows `RunStage.state`, which v2 does not filter on |
| `StageRecordInput.entered` | `RunStageFilter.visitCount` | reshaped | now `IntFilter`; follows `RunStage.visitCount` |
| `StageRecordInput.usage` | — | dropped | follows `RunStage.usage`, which v2 does not filter on |
| `StageRecordInput.cost` | — | dropped | follows `RunStage.cost`, which v2 does not filter on |
| `StageRecordInput.models` | — | dropped | follows `RunStage.modelsUsed`, which v2 does not filter on |
| `StageRecordInput.visitCount` | `RunStageFilter.visitCount` | renamed | follows `RunStage.visitCount` |
| `StageRecordInput.visits` | — | dropped | follows `RunStage.visits`, which v2 does not filter on |
| `StageRecordInput.regionPeaks` | — | dropped | follows `RunStage.regionPeaks`, which v2 does not filter on |
| `StageRecordInput.runawayWarned` | `RunStageFilter.runawayDetected` | renamed | follows `RunStage.runawayDetected` |
| `StageRecordInput.startedAt` | `RunStageFilter.firstEnteredAt` | renamed | follows `RunStage.firstEnteredAt` |
| `StageRecordInput.endedAt` | — | dropped | follows `StageFinished.since`, which v2 does not filter on |
| `StageRecordInput.active` | `RunStageFilter.workingSeconds` | reshaped | now `BigIntFilter`; follows `RunStage.workingSeconds` |
| `StageRecordInput.and` | `RunStageFilter.and` | reshaped | now `[RunStageFilter]` |
| `StageRecordInput.or` | `RunStageFilter.or` | reshaped | now `[RunStageFilter]` |
| `StageRecordInput.not` | `RunStageFilter.not` | reshaped | now `RunStageFilter` |
| `StageRecordInput.isNull` | `RunStageFilter.isNull` | kept |  |

### `StageRecordListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageRecordListInput` | — | dropped | no v2 listing filters a list of `StageRecord` (now `RunStage`) |
| `StageRecordListInput.some` | — | dropped | follows its list filter |
| `StageRecordListInput.every` | — | dropped | follows its list filter |
| `StageRecordListInput.none` | — | dropped | follows its list filter |
| `StageRecordListInput.isNull` | — | dropped | follows its list filter |

### `StageRecordOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageRecordOrder` | — | dropped | the listing it sorted went or became a plain list |
| `StageRecordOrder.field` | — | dropped | the listing it sorted went or became a plain list |
| `StageRecordOrder.direction` | — | dropped | the listing it sorted went or became a plain list |

### `StageRecordOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageRecordOrderField` | — | dropped | the listing it sorted went or became a plain list |
| `StageRecordOrderField.INDEX` | — | dropped | not a sort key in v2 |

### `StageRecordOutput` → `RunStage`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageRecordOutput` | `RunStage` | renamed | one stage as this run went through it |
| `StageRecordOutput.name` | `RunStage.name` | kept |  |
| `StageRecordOutput.index` | `RunStage.position` | renamed | position in declared order; old name kept one release, deprecated |
| `StageRecordOutput.status` | `RunStage.state` | reshaped | the `StageState` types |
| `StageRecordOutput.entered` | `RunStage.visitCount` | dropped | restated `visitCount > 0` |
| `StageRecordOutput.usage` | `RunStage.usage` | kept |  |
| `StageRecordOutput.cost` | `RunStage.cost` | kept |  |
| `StageRecordOutput.models` | `RunStage.modelsUsed` | reshaped | empty rather than null |
| `StageRecordOutput.visitCount` | `RunStage.visitCount` | kept |  |
| `StageRecordOutput.visits` | `RunStage.visits` | kept |  |
| `StageRecordOutput.regionPeaks` | `RunStage.regionPeaks` | kept |  |
| `StageRecordOutput.runawayWarned` | `RunStage.runawayDetected` | renamed | says what happened; old name kept one release, deprecated |
| `StageRecordOutput.startedAt` | `RunStage.firstEnteredAt` | renamed | says which entry; old name kept one release, deprecated |
| `StageRecordOutput.endedAt` | `StageFinished.since` | reshaped | on the state |
| `StageRecordOutput.active` | `RunStage.workingSeconds` | reshaped | seconds working |

### `StageStatus` → `StageState`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageStatus` | `StageState` | reshaped | states that carry data are types (`StageActive` has its visit) |
| `StageStatus.PENDING` | `StageNotReached` | reshaped | a `StageState` type each; what the run waits on is `Run.state` |
| `StageStatus.ACTIVE` | `StageActive` | reshaped | a `StageState` type each; what the run waits on is `Run.state` |
| `StageStatus.WAITING_INPUT` | `StageActive` | reshaped | a `StageState` type each; what the run waits on is `Run.state` |
| `StageStatus.COMPLETE` | `StageFinished` | reshaped | a `StageState` type each; what the run waits on is `Run.state` |
| `StageStatus.ERROR` | `StageFailed` | reshaped | a `StageState` type each; what the run waits on is `Run.state` |
| `StageStatus.SKIPPED` | `StageSkipped` | reshaped | a `StageState` type each; what the run waits on is `Run.state` |

### `StageStatusFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageStatusFilter` | — | dropped | nothing filters on `StageState` in v2 |
| `StageStatusFilter.eq` | — | dropped | follows `StageStatus` |
| `StageStatusFilter.ne` | — | dropped | follows `StageStatus` |
| `StageStatusFilter.in` | — | dropped | follows `StageStatus` |
| `StageStatusFilter.notIn` | — | dropped | follows `StageStatus` |
| `StageStatusFilter.isNull` | — | dropped | follows its filter |

### `StageTransitionedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageTransitionedEvent` | `StageTransitionedEvent` | kept |  |
| `StageTransitionedEvent.seq` | `StageTransitionedEvent.seq` | kept |  |
| `StageTransitionedEvent.at` | `StageTransitionedEvent.at` | kept |  |
| `StageTransitionedEvent.runId` | `StageTransitionedEvent.runId` | kept |  |
| `StageTransitionedEvent.agentId` | `RunEvent.runId` | dropped | the run's live agent id is not a `Run` fact; frames carry `runId` and `run` |
| `StageTransitionedEvent.from` | `StageTransitionedEvent.from` | reshaped | now `RunStage`; now nullable |
| `StageTransitionedEvent.to` | `StageTransitionedEvent.to` | reshaped | now `RunStage` |
| `StageTransitionedEvent.iteration` | `StageVisit.number` | merged | on `StageTransitionedEvent.visit` |
| `StageTransitionedEvent.run` | `StageTransitionedEvent.run` | kept |  |

### `StageVisitInput` → `StageVisitFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageVisitInput` | `StageVisitFilter` | kept |  |
| `StageVisitInput.id` | `StageVisitFilter.id` | kept |  |
| `StageVisitInput.ordinal` | `StageVisitFilter.number` | renamed | follows `StageVisit.number` |
| `StageVisitInput.enteredAt` | `StageVisitFilter.enteredAt` | kept |  |
| `StageVisitInput.leftAt` | `StageVisitFilter.leftAt` | kept |  |
| `StageVisitInput.inProgress` | `StageVisitFilter.leftAt` | reshaped | now `DateTimeFilter`; follows `StageVisit.leftAt` |
| `StageVisitInput.usage` | — | dropped | follows `StageVisit.usage`, which v2 does not filter on |
| `StageVisitInput.cost` | — | dropped | follows `StageVisit.cost`, which v2 does not filter on |
| `StageVisitInput.and` | `StageVisitFilter.and` | kept |  |
| `StageVisitInput.or` | `StageVisitFilter.or` | kept |  |
| `StageVisitInput.not` | `StageVisitFilter.not` | kept |  |
| `StageVisitInput.isNull` | `StageVisitFilter.isNull` | kept |  |

### `StageVisitListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageVisitListInput` | — | dropped | no v2 listing filters a list of `StageVisit` (now `StageVisit`) |
| `StageVisitListInput.some` | — | dropped | follows its list filter |
| `StageVisitListInput.every` | — | dropped | follows its list filter |
| `StageVisitListInput.none` | — | dropped | follows its list filter |
| `StageVisitListInput.isNull` | — | dropped | follows its list filter |

### `StageVisitOutput` → `StageVisit`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StageVisitOutput` | `StageVisit` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `StageVisitOutput.id` | `StageVisit.id` | kept |  |
| `StageVisitOutput.ordinal` | `StageVisit.number` | renamed | plain English; old name kept one release, deprecated |
| `StageVisitOutput.enteredAt` | `StageVisit.enteredAt` | kept |  |
| `StageVisitOutput.leftAt` | `StageVisit.leftAt` | kept |  |
| `StageVisitOutput.inProgress` | `StageVisit.leftAt` | dropped | restated `leftAt` being null |
| `StageVisitOutput.usage` | `StageVisit.usage` | kept |  |
| `StageVisitOutput.cost` | `StageVisit.cost` | kept |  |

### `StartRunExportRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StartRunExportRequest` | `StartRunExportRequest` | kept |  |
| `StartRunExportRequest.filter` | `StartRunExportRequest.filter` | kept |  |
| `StartRunExportRequest.fields` | `StartRunExportRequest.fields` | reshaped | now `[RunExportField]` |

### `StartRunExportResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StartRunExportResult` | `StartRunExportResult` | kept |  |
| `StartRunExportResult.export` | `StartRunExportResult.export` | kept |  |

### `StartUpdateRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StartUpdateRequest` | `StartUpdateRequest` | kept |  |
| `StartUpdateRequest.steps` | `StartUpdateRequest.steps` | kept |  |

### `StartUpdateResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StartUpdateResult` | `StartUpdateResult` | kept |  |
| `StartUpdateResult.job` | `StartUpdateResult.job` | kept |  |

### `StringFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StringFilter` | `StringFilter` | kept |  |
| `StringFilter.eq` | `StringFilter.eq` | kept |  |
| `StringFilter.ne` | `StringFilter.ne` | kept |  |
| `StringFilter.in` | `StringFilter.in` | kept |  |
| `StringFilter.notIn` | `StringFilter.notIn` | kept |  |
| `StringFilter.contains` | `StringFilter.contains` | kept |  |
| `StringFilter.startsWith` | `StringFilter.startsWith` | kept |  |
| `StringFilter.endsWith` | `StringFilter.endsWith` | kept |  |
| `StringFilter.isNull` | `StringFilter.isNull` | kept |  |

### `StringListFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StringListFilter` | `StringListFilter` | kept |  |
| `StringListFilter.has` | `StringListFilter.has` | kept |  |
| `StringListFilter.hasEvery` | `StringListFilter.hasEvery` | kept |  |
| `StringListFilter.hasSome` | `StringListFilter.hasSome` | kept |  |
| `StringListFilter.isEmpty` | `StringListFilter.isEmpty` | kept |  |
| `StringListFilter.isNull` | `StringListFilter.isNull` | kept |  |

### `StuckThresholdsInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StuckThresholdsInput` | — | dropped | nothing filters on `StuckThresholds` in v2 |
| `StuckThresholdsInput.afterIterations` | — | dropped | follows `StuckThresholds.afterIterations`, which v2 does not filter on |
| `StuckThresholdsInput.afterMinutes` | — | dropped | follows `StuckThresholds.afterMinutes`, which v2 does not filter on |
| `StuckThresholdsInput.afterSameFileEdits` | — | dropped | follows `StuckThresholds.afterSameFileEdits`, which v2 does not filter on |
| `StuckThresholdsInput.afterToolCalls` | — | dropped | follows `StuckThresholds.afterToolCalls`, which v2 does not filter on |
| `StuckThresholdsInput.and` | — | dropped | follows its filter |
| `StuckThresholdsInput.or` | — | dropped | follows its filter |
| `StuckThresholdsInput.not` | — | dropped | follows its filter |
| `StuckThresholdsInput.isNull` | — | dropped | follows its filter |

### `StuckThresholdsOutput` → `StuckThresholds`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `StuckThresholdsOutput` | `StuckThresholds` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `StuckThresholdsOutput.afterIterations` | `StuckThresholds.afterIterations` | kept |  |
| `StuckThresholdsOutput.afterMinutes` | `StuckThresholds.afterMinutes` | kept |  |
| `StuckThresholdsOutput.afterSameFileEdits` | `StuckThresholds.afterSameFileEdits` | kept |  |
| `StuckThresholdsOutput.afterToolCalls` | `StuckThresholds.afterToolCalls` | kept |  |

### `SubmitOutputArgsOutput` → `SubmitOutputArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SubmitOutputArgsOutput` | `SubmitOutputArguments` | renamed | argument types are named `XArguments` |
| `SubmitOutputArgsOutput.content` | `SubmitOutputArguments.content` | kept |  |
| `SubmitOutputArgsOutput.artifacts` | `SubmitOutputArguments.artifacts` | kept |  |

### `SubmitOutputCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SubmitOutputCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `SubmitOutputCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `SubmitOutputCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `SubmitOutputCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `SubmitOutputCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `SubmitOutputArguments` for `submit_output` |

### `SubmittedArtifact` → `ArtifactArgument`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SubmittedArtifact` | `ArtifactArgument` | merged | the two members differed only in optionality |
| `SubmittedArtifact.ArtifactByPathOutput` | `ArtifactArgument` | merged | one type |
| `SubmittedArtifact.ArtifactDescribedOutput` | `ArtifactArgument` | merged | one type |

### `Subscription`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `Subscription` | `Subscription` | kept |  |
| `Subscription.runEvents` | `Subscription.runEvents` | kept |  |
| `Subscription.runEvents(filter:)` | `Subscription.runEvents(filter:)` | kept |  |
| `Subscription.runEvents(types:)` | `Subscription.runEvents(frames:)` | reshaped | a `RunEventFrameFilter`, one field per frame type |
| `Subscription.runEvents(includeDescendants:)` | `Subscription.runEvents(includeDescendants:)` | kept |  |
| `Subscription.machineEvents` | `Subscription.machineEvents` | kept |  |
| `Subscription.machineEvents(types:)` | `Subscription.machineEvents(frames:)` | reshaped | a `MachineEventFrameFilter` |
| `Subscription.updateJobEvents` | `Subscription.updateJobEvents` | kept |  |
| `Subscription.updateJobEvents(id:)` | `Subscription.updateJobEvents(id:)` | kept |  |

### `SubscriptionOpenedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SubscriptionOpenedEvent` | `SubscriptionOpenedEvent` | kept |  |
| `SubscriptionOpenedEvent.seq` | `SubscriptionOpenedEvent.seq` | kept |  |
| `SubscriptionOpenedEvent.at` | `SubscriptionOpenedEvent.at` | kept |  |
| `SubscriptionOpenedEvent.serverInstance` | `SubscriptionOpenedEvent.serverInstance` | kept |  |
| `SubscriptionOpenedEvent.daemon` | `SubscriptionOpenedEvent.daemon` | kept |  |

### `SystemInfoCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `SystemInfoCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `SystemInfoCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `SystemInfoCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `SystemInfoCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |

### `TaintTracking`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TaintTracking` | — | dropped | a boolean (`SettingOverrides.tracksTaint`, null inherits) |
| `TaintTracking.INHERIT` | `SettingOverrides.tracksTaint` null | dropped | a boolean; `INHERIT` was mislabelled |
| `TaintTracking.TRACK` | `SettingOverrides.tracksTaint` true | dropped | a boolean; `INHERIT` was mislabelled |

### `TaintTrackingFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TaintTrackingFilter` | — | dropped | `TaintTracking` went |
| `TaintTrackingFilter.eq` | — | dropped | follows `TaintTracking` |
| `TaintTrackingFilter.ne` | — | dropped | follows `TaintTracking` |
| `TaintTrackingFilter.in` | — | dropped | follows `TaintTracking` |
| `TaintTrackingFilter.notIn` | — | dropped | follows `TaintTracking` |
| `TaintTrackingFilter.isNull` | — | dropped | follows its filter |

### `Timestamp` → `DateTime`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `Timestamp` | `DateTime` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |

### `TimestampFilter` → `DateTimeFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TimestampFilter` | `DateTimeFilter` | kept |  |
| `TimestampFilter.eq` | `DateTimeFilter.eq` | kept |  |
| `TimestampFilter.ne` | `DateTimeFilter.ne` | kept |  |
| `TimestampFilter.in` | `DateTimeFilter.in` | kept |  |
| `TimestampFilter.notIn` | `DateTimeFilter.notIn` | kept |  |
| `TimestampFilter.lt` | `DateTimeFilter.lt` | kept |  |
| `TimestampFilter.lte` | `DateTimeFilter.lte` | kept |  |
| `TimestampFilter.gt` | `DateTimeFilter.gt` | kept |  |
| `TimestampFilter.gte` | `DateTimeFilter.gte` | kept |  |
| `TimestampFilter.isNull` | `DateTimeFilter.isNull` | kept |  |

### `TodoAddArgsOutput` → `TodoAddArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TodoAddArgsOutput` | `TodoAddArguments` | renamed | argument types are named `XArguments` |
| `TodoAddArgsOutput.region` | `TodoAddArguments.regionName` | renamed | a region name |
| `TodoAddArgsOutput.item` | `TodoAddArguments.item` | kept |  |

### `TodoAddCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TodoAddCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `TodoAddCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `TodoAddCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `TodoAddCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `TodoAddCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `TodoAddArguments` for `todo_add` |

### `TodoDoneArgsOutput` → `TodoDoneArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TodoDoneArgsOutput` | `TodoDoneArguments` | renamed | argument types are named `XArguments` |
| `TodoDoneArgsOutput.region` | `TodoDoneArguments.regionName` | renamed | a region name |
| `TodoDoneArgsOutput.id` | `TodoDoneArguments.itemNumber` | renamed | the item's number, not an id |

### `TodoDoneCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TodoDoneCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `TodoDoneCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `TodoDoneCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `TodoDoneCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `TodoDoneCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `TodoDoneArguments` for `todo_done` |

### `TodoNoteArgsOutput` → `TodoNoteArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TodoNoteArgsOutput` | `TodoNoteArguments` | renamed | argument types are named `XArguments` |
| `TodoNoteArgsOutput.region` | `TodoNoteArguments.regionName` | renamed | a region name |
| `TodoNoteArgsOutput.id` | `TodoNoteArguments.itemNumber` | renamed | the item's number, not an id |
| `TodoNoteArgsOutput.note` | `TodoNoteArguments.note` | kept |  |

### `TodoNoteCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TodoNoteCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `TodoNoteCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `TodoNoteCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `TodoNoteCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `TodoNoteCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `TodoNoteArguments` for `todo_note` |

### `TokensUpdatedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TokensUpdatedEvent` | `TokensUpdatedEvent` | kept |  |
| `TokensUpdatedEvent.seq` | `TokensUpdatedEvent.seq` | kept |  |
| `TokensUpdatedEvent.at` | `TokensUpdatedEvent.at` | kept |  |
| `TokensUpdatedEvent.runId` | `TokensUpdatedEvent.runId` | kept |  |
| `TokensUpdatedEvent.agentId` | `RunEvent.runId` | dropped | the run's live agent id is not a `Run` fact; frames carry `runId` and `run` |
| `TokensUpdatedEvent.promptTokens` | `TokenUsage.inputTokens` | merged | one `TokenUsage` on `TokensUpdatedEvent.usage` |
| `TokensUpdatedEvent.completionTokens` | `TokenUsage.outputTokens` | merged |  |
| `TokensUpdatedEvent.cachedTokens` | `TokenUsage.cacheReadTokens` | merged |  |
| `TokensUpdatedEvent.cacheWriteTokens` | `TokenUsage.cacheWriteTokens` | merged |  |
| `TokensUpdatedEvent.run` | `TokensUpdatedEvent.run` | kept |  |

### `TokenUsageInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TokenUsageInput` | — | dropped | nothing filters on `TokenUsage` in v2 |
| `TokenUsageInput.promptTokens` | — | dropped | follows `TokenUsage.inputTokens`, which v2 does not filter on |
| `TokenUsageInput.completionTokens` | — | dropped | follows `TokenUsage.outputTokens`, which v2 does not filter on |
| `TokenUsageInput.cachedTokens` | — | dropped | follows `TokenUsage.cacheReadTokens`, which v2 does not filter on |
| `TokenUsageInput.cacheWriteTokens` | — | dropped | follows `TokenUsage.cacheWriteTokens`, which v2 does not filter on |
| `TokenUsageInput.and` | — | dropped | follows its filter |
| `TokenUsageInput.or` | — | dropped | follows its filter |
| `TokenUsageInput.not` | — | dropped | follows its filter |
| `TokenUsageInput.isNull` | — | dropped | follows its filter |

### `TokenUsageOutput` → `TokenUsage`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TokenUsageOutput` | `TokenUsage` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `TokenUsageOutput.promptTokens` | `TokenUsage.inputTokens` | renamed | input and output, not prompt and completion; old name kept one release, deprecated |
| `TokenUsageOutput.completionTokens` | `TokenUsage.outputTokens` | renamed | old name kept one release, deprecated |
| `TokenUsageOutput.cachedTokens` | `TokenUsage.cacheReadTokens` | renamed | reads, beside `cacheWriteTokens`; old name kept one release, deprecated |
| `TokenUsageOutput.cacheWriteTokens` | `TokenUsage.cacheWriteTokens` | kept |  |

### `Tool`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `Tool` | `Tool` | reshaped | four implementers, a `Node` and `Versioned`; description and schema move to `ToolRevision`, which tool calls record |
| `Tool.name` | `Tool.name` | kept |  |
| `Tool.description` | `ToolRevision.description` | reshaped | on `Tool.revision`, which a tool call records |
| `Tool.arguments` | `ToolRevision.arguments` | reshaped | on `Tool.revision` |
| `Tool.origin` | `__typename` | dropped | a kind field: `__typename` says it |

### `ToolAcceptRuleInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolAcceptRuleInput` | — | dropped | nothing filters on `ToolAcceptRule` in v2 |
| `ToolAcceptRuleInput.tool` | — | dropped | follows `ToolAcceptRule.tool`, which v2 does not filter on |
| `ToolAcceptRuleInput.patterns` | — | dropped | follows `ToolAcceptRule.mimePatterns`, which v2 does not filter on |
| `ToolAcceptRuleInput.and` | — | dropped | follows its filter |
| `ToolAcceptRuleInput.or` | — | dropped | follows its filter |
| `ToolAcceptRuleInput.not` | — | dropped | follows its filter |
| `ToolAcceptRuleInput.isNull` | — | dropped | follows its filter |

### `ToolAcceptRuleListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolAcceptRuleListInput` | — | dropped | no v2 listing filters a list of `ToolAcceptRule` (now `ToolAcceptRule`) |
| `ToolAcceptRuleListInput.some` | — | dropped | follows its list filter |
| `ToolAcceptRuleListInput.every` | — | dropped | follows its list filter |
| `ToolAcceptRuleListInput.none` | — | dropped | follows its list filter |
| `ToolAcceptRuleListInput.isNull` | — | dropped | follows its list filter |

### `ToolAcceptRuleOutput` → `ToolAcceptRule`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolAcceptRuleOutput` | `ToolAcceptRule` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `ToolAcceptRuleOutput.tool` | `ToolAcceptRule.tool` | reshaped | a `ToolByName` |
| `ToolAcceptRuleOutput.patterns` | `ToolAcceptRule.mimePatterns` | renamed | says what they match; old name kept one release, deprecated |

### `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolCall` | `ToolCall` | reshaped | one object type; the arguments are the typed `ToolArguments` union and the tool a `ToolRevision` |
| `ToolCall.toolName` | `ToolCall.toolName` | kept |  |
| `ToolCall.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `ToolCall.rawArguments` | `ToolCall.rawArguments` | kept |  |

### `ToolCallFinishedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolCallFinishedEvent` | `ToolCallFinishedEvent` | kept |  |
| `ToolCallFinishedEvent.seq` | `ToolCallFinishedEvent.seq` | kept |  |
| `ToolCallFinishedEvent.at` | `ToolCallFinishedEvent.at` | kept |  |
| `ToolCallFinishedEvent.runId` | `ToolCallFinishedEvent.runId` | kept |  |
| `ToolCallFinishedEvent.agentId` | `RunEvent.runId` | dropped | the run's live agent id is not a `Run` fact; frames carry `runId` and `run` |
| `ToolCallFinishedEvent.callId` | `ToolCall.providerCallId` | dropped | on `ToolCallFinishedEvent.execution`; frames pair by `executionId` |
| `ToolCallFinishedEvent.executionId` | `ToolCallFinishedEvent.executionId` | kept |  |
| `ToolCallFinishedEvent.tool` | `ToolCallFinishedEvent.toolName` | renamed | a name; old name kept one release, deprecated |
| `ToolCallFinishedEvent.ok` | `ToolCallFinishedEvent.tookEffect` | renamed | says what it reads; old name kept one release, deprecated |
| `ToolCallFinishedEvent.summary` | `ToolCallFinishedEvent.summary` | kept |  |
| `ToolCallFinishedEvent.run` | `ToolCallFinishedEvent.run` | kept |  |

### `ToolCallStartedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolCallStartedEvent` | `ToolCallStartedEvent` | kept |  |
| `ToolCallStartedEvent.seq` | `ToolCallStartedEvent.seq` | kept |  |
| `ToolCallStartedEvent.at` | `ToolCallStartedEvent.at` | kept |  |
| `ToolCallStartedEvent.runId` | `ToolCallStartedEvent.runId` | kept |  |
| `ToolCallStartedEvent.agentId` | `RunEvent.runId` | dropped | the run's live agent id is not a `Run` fact; frames carry `runId` and `run` |
| `ToolCallStartedEvent.callId` | `ToolCall.providerCallId` | dropped | on `ToolCallStartedEvent.execution` |
| `ToolCallStartedEvent.executionId` | `ToolCallStartedEvent.executionId` | kept |  |
| `ToolCallStartedEvent.tool` | `ToolCallStartedEvent.toolName` | renamed | a name; old name kept one release, deprecated |
| `ToolCallStartedEvent.run` | `ToolCallStartedEvent.run` | kept |  |

### `ToolConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolConnection` | `ToolConnection` | kept |  |
| `ToolConnection.results` | `ToolConnection.results` | kept |  |
| `ToolConnection.cursor` | `ToolConnection.cursor` | kept |  |
| `ToolConnection.total` | `ToolConnection.total` | kept |  |
| `ToolConnection.skipped` | — | dropped | follows its connection |

### `ToolExecutionConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolExecutionConnection` | `ToolExecutionConnection` | kept |  |
| `ToolExecutionConnection.results` | `ToolExecutionConnection.results` | kept |  |
| `ToolExecutionConnection.cursor` | `ToolExecutionConnection.cursor` | kept |  |
| `ToolExecutionConnection.total` | `ToolExecutionConnection.total` | kept |  |

### `ToolExecutionInput` → `ToolExecutionFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolExecutionInput` | `ToolExecutionFilter` | kept |  |
| `ToolExecutionInput.id` | `ToolExecutionFilter.id` | kept |  |
| `ToolExecutionInput.callId` | `ToolCallFilter.providerCallId` | renamed | follows `ToolCall.providerCallId` |
| `ToolExecutionInput.outcome` | `ToolExecutionFilter.state` | reshaped | now `ExecutionStateFilter`; follows `ToolExecution.state` |
| `ToolExecutionInput.stageIndex` | `ToolExecutionFilter.stage` | reshaped | now `RunStageFilter`; follows `ToolExecution.stage` |
| `ToolExecutionInput.iteration` | — | dropped | follows `ToolExecution.turn`, which v2 does not filter on |
| `ToolExecutionInput.visit` | `ToolExecutionFilter.visit` | kept |  |
| `ToolExecutionInput.requestedBy` | `ToolExecutionFilter.requestedBy` | reshaped | now `AnsweredAttemptFilter` |
| `ToolExecutionInput.contextChanges` | — | dropped | follows `ToolExecution.contextChanges`, which v2 does not filter on |
| `ToolExecutionInput.dispatchedAt` | `ToolExecutionFilter.dispatchedAt` | kept |  |
| `ToolExecutionInput.endedAt` | `ExecutionEndedFilter.endedAt` | renamed | follows `ExecutionEnded.endedAt` |
| `ToolExecutionInput.journalPosition` | — | dropped | follows `ToolExecution.journalPosition`, which v2 does not filter on |
| `ToolExecutionInput.result` | — | dropped | follows `ExecutionEnded.result`, which v2 does not filter on |
| `ToolExecutionInput.and` | `ToolExecutionFilter.and` | kept |  |
| `ToolExecutionInput.or` | `ToolExecutionFilter.or` | kept |  |
| `ToolExecutionInput.not` | `ToolExecutionFilter.not` | kept |  |
| `ToolExecutionInput.isNull` | — | dropped | follows its filter |

### `ToolExecutionOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolExecutionOrder` | `ToolExecutionOrder` | kept |  |
| `ToolExecutionOrder.field` | `ToolExecutionOrder.field` | kept |  |
| `ToolExecutionOrder.direction` | `ToolExecutionOrder.direction` | kept |  |

### `ToolExecutionOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolExecutionOrderField` | `ToolExecutionOrderField` | kept |  |
| `ToolExecutionOrderField.JOURNAL_POSITION` | `ToolExecutionOrderField.JOURNAL_POSITION` | kept |  |

### `ToolExecutionOutput` → `ToolExecution`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolExecutionOutput` | `ToolExecution` | reshaped | `state` is a union; relations replace the stage index, iteration and call id |
| `ToolExecutionOutput.id` | `ToolExecution.id` | kept |  |
| `ToolExecutionOutput.call` | `ToolExecution.call` | kept |  |
| `ToolExecutionOutput.callId` | `ToolCall.providerCallId` | merged | the call's own provider id, on the call |
| `ToolExecutionOutput.outcome` | `ToolExecution.state` | reshaped | the `ExecutionState` union: `ExecutionRunning`, or `ExecutionEnded` with its outcome |
| `ToolExecutionOutput.stageIndex` | `ToolExecution.stage` | reshaped | a relation to the `RunStage` |
| `ToolExecutionOutput.iteration` | `ToolExecution.turn` | reshaped | a relation to the `Turn` |
| `ToolExecutionOutput.visit` | `ToolExecution.visit` | kept |  |
| `ToolExecutionOutput.requestedBy` | `ToolExecution.requestedBy` | reshaped | now `AnsweredAttempt` |
| `ToolExecutionOutput.contextChanges` | `ToolExecution.contextChanges` | kept |  |
| `ToolExecutionOutput.producedArtifacts` | `ToolExecution.artifacts` | renamed | shorter; old name kept one release, deprecated |
| `ToolExecutionOutput.dispatchedAt` | `ToolExecution.dispatchedAt` | kept |  |
| `ToolExecutionOutput.endedAt` | `ExecutionEnded.endedAt` | reshaped | only an ended execution has an end |
| `ToolExecutionOutput.journalPosition` | `ToolExecution.journalPosition` | kept |  |
| `ToolExecutionOutput.result` | `ExecutionEnded.result` | reshaped | only an ended execution has a result |

### `ToolGroupOutput` → the group selector types

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolGroupOutput` | `AllTools` | reshaped | each group is its own `ToolSelector` type, not an object per token or an enum repeating the tool types |
| `ToolGroupOutput.name` | `AllTools.token` | reshaped | the token is a field of each group type |
| `ToolGroupOutput.description` | `AllTools` | reshaped | the type's description |

### `ToolInput` → `ToolFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolInput` | `ToolFilter` | kept |  |
| `ToolInput.name` | `ToolFilter.name` | kept |  |
| `ToolInput.description` | — | dropped | follows `ToolRevision.description`, which v2 does not filter on |
| `ToolInput.origin` | — | dropped | follows `Tool.origin`, which went |
| `ToolInput.and` | `ToolFilter.and` | kept |  |
| `ToolInput.or` | `ToolFilter.or` | kept |  |
| `ToolInput.not` | `ToolFilter.not` | kept |  |
| `ToolInput.isNull` | `ToolFilter.isNull` | kept |  |

### `ToolKind`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolKind` | — | dropped | a third "where from" vocabulary; `decide` classifies the tool itself now |
| `ToolKind.BUILTIN` | `BuiltinTool` | reshaped | the server classifies the tool; a type each |
| `ToolKind.SUBAGENT` | `SubagentTool` | reshaped | the server classifies the tool; a type each |
| `ToolKind.SCRIPT` | `CustomTool` | reshaped | the server classifies the tool; a type each |
| `ToolKind.MCP` | `McpTool` | reshaped | the server classifies the tool; a type each |

### `ToolOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolOrder` | `ToolOrder` | kept |  |
| `ToolOrder.field` | `ToolOrder.field` | kept |  |
| `ToolOrder.direction` | `ToolOrder.direction` | kept |  |

### `ToolOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolOrderField` | `ToolOrderField` | kept |  |
| `ToolOrderField.NAME` | `ToolOrderField.NAME` | kept |  |

### `ToolOrigin`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolOrigin` | — | dropped | the `Tool` implementer is the origin |
| `ToolOrigin.BUILTIN` | `BuiltinTool` | reshaped | a type each |
| `ToolOrigin.SUBAGENT` | `SubagentTool` | reshaped | a type each |
| `ToolOrigin.BLUEPRINT_SCRIPT` | `CustomTool` with `extension.blueprint` set | dropped | a type each |
| `ToolOrigin.GLOBAL_SCRIPT` | `CustomTool` with `extension.blueprint` null | dropped | a type each |

### `ToolOriginFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolOriginFilter` | — | dropped | `ToolOrigin` went |
| `ToolOriginFilter.eq` | — | dropped | follows `ToolOrigin` |
| `ToolOriginFilter.ne` | — | dropped | follows `ToolOrigin` |
| `ToolOriginFilter.in` | — | dropped | follows `ToolOrigin` |
| `ToolOriginFilter.notIn` | — | dropped | follows `ToolOrigin` |
| `ToolOriginFilter.isNull` | — | dropped | follows its filter |

### `ToolOutcome` → `ExecutionOutcome`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolOutcome` | `ExecutionOutcome` | reshaped | how an ended execution ended, on `ExecutionEnded`; `RUNNING` is `ExecutionRunning` |
| `ToolOutcome.SUCCEEDED` | `ExecutionOutcome.SUCCEEDED` | kept |  |
| `ToolOutcome.FAILED` | `ExecutionOutcome.FAILED` | kept |  |
| `ToolOutcome.BLOCKED` | `ExecutionOutcome.BLOCKED` | kept |  |
| `ToolOutcome.DENIED` | `ExecutionOutcome.DENIED` | kept |  |
| `ToolOutcome.INDETERMINATE` | `ExecutionOutcome.INTERRUPTED` | renamed | says what happened: nobody observed the end |

### `ToolOutcomeFilter` → `ExecutionOutcomeFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolOutcomeFilter` | `ExecutionOutcomeFilter` | renamed | follows `ExecutionOutcome` |
| `ToolOutcomeFilter.eq` | `ExecutionOutcomeFilter.eq` | kept |  |
| `ToolOutcomeFilter.ne` | `ExecutionOutcomeFilter.ne` | kept |  |
| `ToolOutcomeFilter.in` | `ExecutionOutcomeFilter.in` | kept |  |
| `ToolOutcomeFilter.notIn` | `ExecutionOutcomeFilter.notIn` | kept |  |
| `ToolOutcomeFilter.isNull` | `ExecutionOutcomeFilter.isNull` | kept |  |

### `ToolPermissionPolicy` → `ToolPermission`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolPermissionPolicy` | `ToolPermission` | renamed | "policy" is the approval policy's word |
| `ToolPermissionPolicy.ALLOW` | `ToolPermission.ALLOW` | kept |  |
| `ToolPermissionPolicy.ASK` | `ToolPermission.ASK` | kept |  |
| `ToolPermissionPolicy.DENY` | `ToolPermission.DENY` | kept |  |

### `ToolPermissionPolicyFilter` → `ToolPermissionFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolPermissionPolicyFilter` | `ToolPermissionFilter` | renamed | follows `ToolPermission` |
| `ToolPermissionPolicyFilter.eq` | `ToolPermissionFilter.eq` | kept |  |
| `ToolPermissionPolicyFilter.ne` | `ToolPermissionFilter.ne` | kept |  |
| `ToolPermissionPolicyFilter.in` | `ToolPermissionFilter.in` | kept |  |
| `ToolPermissionPolicyFilter.notIn` | `ToolPermissionFilter.notIn` | kept |  |
| `ToolPermissionPolicyFilter.isNull` | `ToolPermissionFilter.isNull` | kept |  |

### `ToolPermissionRuleInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolPermissionRuleInput` | — | dropped | nothing filters on `ToolPermissionRule` in v2 |
| `ToolPermissionRuleInput.tool` | — | dropped | follows `ToolPermissionRule.selector`, which v2 does not filter on |
| `ToolPermissionRuleInput.policy` | — | dropped | follows `ToolPermissionRule.permission`, which v2 does not filter on |
| `ToolPermissionRuleInput.and` | — | dropped | follows its filter |
| `ToolPermissionRuleInput.or` | — | dropped | follows its filter |
| `ToolPermissionRuleInput.not` | — | dropped | follows its filter |
| `ToolPermissionRuleInput.isNull` | — | dropped | follows its filter |

### `ToolPermissionRuleListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolPermissionRuleListInput` | — | dropped | no v2 listing filters a list of `ToolPermissionRule` (now `ToolPermissionRule`) |
| `ToolPermissionRuleListInput.some` | — | dropped | follows its list filter |
| `ToolPermissionRuleListInput.every` | — | dropped | follows its list filter |
| `ToolPermissionRuleListInput.none` | — | dropped | follows its list filter |
| `ToolPermissionRuleListInput.isNull` | — | dropped | follows its list filter |

### `ToolPermissionRuleOutput` → `ToolPermissionRule`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolPermissionRuleOutput` | `ToolPermissionRule` | reshaped | one rule type everywhere a permission is written; the tool is a typed `ToolSelector` |
| `ToolPermissionRuleOutput.tool` | `ToolPermissionRule.selector` | reshaped | a typed `ToolSelector`, not a string |
| `ToolPermissionRuleOutput.policy` | `ToolPermissionRule.permission` | renamed | "policy" is the approval policy's word; old name kept one release, deprecated |

### `ToolRescan`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolRescan` | `ToolRescan` | kept |  |
| `ToolRescan.AT_SPAWN_ONLY` | `ToolRescan.AT_SPAWN_ONLY` | kept |  |
| `ToolRescan.RESCAN_AFTER_WRITES` | `ToolRescan.RESCAN_AFTER_WRITES` | kept |  |
| `ToolRescan.RESCAN_BEFORE_DISPATCH` | `ToolRescan.RESCAN_BEFORE_DISPATCH` | kept |  |

### `ToolRescanFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolRescanFilter` | — | dropped | nothing filters on `ToolRescan` in v2 |
| `ToolRescanFilter.eq` | — | dropped | follows `ToolRescan` |
| `ToolRescanFilter.ne` | — | dropped | follows `ToolRescan` |
| `ToolRescanFilter.in` | — | dropped | follows `ToolRescan` |
| `ToolRescanFilter.notIn` | — | dropped | follows `ToolRescan` |
| `ToolRescanFilter.isNull` | — | dropped | follows its filter |

### `ToolReturnInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolReturnInput` | — | dropped | nothing filters on `ToolResult` in v2 |
| `ToolReturnInput.text` | — | dropped | follows `ToolResult.text`, which v2 does not filter on |
| `ToolReturnInput.bytes` | — | dropped | follows `ToolResult.bytes`, which v2 does not filter on |
| `ToolReturnInput.truncated` | — | dropped | follows `ToolResult.truncated`, which v2 does not filter on |
| `ToolReturnInput.parts` | — | dropped | follows `ToolResult.parts`, which v2 does not filter on |
| `ToolReturnInput.and` | — | dropped | follows its filter |
| `ToolReturnInput.or` | — | dropped | follows its filter |
| `ToolReturnInput.not` | — | dropped | follows its filter |
| `ToolReturnInput.isNull` | — | dropped | follows its filter |

### `ToolReturnOutput` → `ToolResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolReturnOutput` | `ToolResult` | reshaped | parts are `StoredPart`s, not names |
| `ToolReturnOutput.text` | `ToolResult.text` | kept |  |
| `ToolReturnOutput.bytes` | `ToolResult.bytes` | kept |  |
| `ToolReturnOutput.truncated` | `ToolResult.truncated` | kept |  |
| `ToolReturnOutput.parts` | `ToolResult.parts` | reshaped | now `[StoredPart]` |

### `ToolRouteOverrideInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolRouteOverrideInput` | — | dropped | nothing filters on `ToolResultRule` in v2 |
| `ToolRouteOverrideInput.tool` | — | dropped | follows `ToolResultRule.tool`, which v2 does not filter on |
| `ToolRouteOverrideInput.region` | — | dropped | follows `ToolResultRule.region`, which v2 does not filter on |
| `ToolRouteOverrideInput.regionName` | — | dropped | follows `ToolResultRule.region`, which v2 does not filter on |
| `ToolRouteOverrideInput.and` | — | dropped | follows its filter |
| `ToolRouteOverrideInput.or` | — | dropped | follows its filter |
| `ToolRouteOverrideInput.not` | — | dropped | follows its filter |
| `ToolRouteOverrideInput.isNull` | — | dropped | follows its filter |

### `ToolRouteOverrideListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolRouteOverrideListInput` | — | dropped | no v2 listing filters a list of `ToolRouteOverride` (now `ToolResultRule`) |
| `ToolRouteOverrideListInput.some` | — | dropped | follows its list filter |
| `ToolRouteOverrideListInput.every` | — | dropped | follows its list filter |
| `ToolRouteOverrideListInput.none` | — | dropped | follows its list filter |
| `ToolRouteOverrideListInput.isNull` | — | dropped | follows its list filter |

### `ToolRouteOverrideOutput` → `ToolResultRule`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolRouteOverrideOutput` | `ToolResultRule` | merged | one per-tool rule |
| `ToolRouteOverrideOutput.tool` | `ToolResultRule.tool` | reshaped | a `ToolByName` |
| `ToolRouteOverrideOutput.region` | `ToolResultRule.region` | merged |  |
| `ToolRouteOverrideOutput.regionName` | `ToolResultRule.region` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |

### `ToolRoutingInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolRoutingInput` | — | dropped | nothing filters on `ToolResultRouting` in v2 |
| `ToolRoutingInput.defaultRegion` | — | dropped | follows `ToolResultRouting.defaultRegion`, which v2 does not filter on |
| `ToolRoutingInput.defaultRegionName` | — | dropped | follows `ToolResultRouting.defaultRegion`, which v2 does not filter on |
| `ToolRoutingInput.overrides` | — | dropped | follows `ToolResultRouting.perTool`, which v2 does not filter on |
| `ToolRoutingInput.keepResults` | — | dropped | follows `ToolResultRouting.keepsResults`, which v2 does not filter on |
| `ToolRoutingInput.maxResultTokens` | — | dropped | follows `ToolResultRouting.maxResultTokens`, which v2 does not filter on |
| `ToolRoutingInput.maxResultTokensPerTool` | — | dropped | follows `ToolResultRouting.perTool`, which v2 does not filter on |
| `ToolRoutingInput.and` | — | dropped | follows its filter |
| `ToolRoutingInput.or` | — | dropped | follows its filter |
| `ToolRoutingInput.not` | — | dropped | follows its filter |
| `ToolRoutingInput.isNull` | — | dropped | follows its filter |

### `ToolRoutingOutput` → `ToolResultRouting`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolRoutingOutput` | `ToolResultRouting` | reshaped | the two per-tool lists merge into `perTool` |
| `ToolRoutingOutput.defaultRegion` | `ToolResultRouting.defaultRegion` | reshaped | never null: it defaults to `conversation` |
| `ToolRoutingOutput.defaultRegionName` | `ToolResultRouting.defaultRegion` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `ToolRoutingOutput.overrides` | `ToolResultRouting.perTool` | merged | one per-tool rule list |
| `ToolRoutingOutput.keepResults` | `ToolResultRouting.keepsResults` | renamed | a boolean reads as a verb; old name kept one release, deprecated |
| `ToolRoutingOutput.maxResultTokens` | `ToolResultRouting.maxResultTokens` | kept |  |
| `ToolRoutingOutput.maxResultTokensPerTool` | `ToolResultRouting.perTool` | merged | one per-tool rule list |

### `ToolTokenCeilingInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolTokenCeilingInput` | — | dropped | nothing filters on `ToolResultRule` in v2 |
| `ToolTokenCeilingInput.tool` | — | dropped | follows `ToolResultRule.tool`, which v2 does not filter on |
| `ToolTokenCeilingInput.maxResultTokens` | — | dropped | follows `ToolResultRule.maxResultTokens`, which v2 does not filter on |
| `ToolTokenCeilingInput.and` | — | dropped | follows its filter |
| `ToolTokenCeilingInput.or` | — | dropped | follows its filter |
| `ToolTokenCeilingInput.not` | — | dropped | follows its filter |
| `ToolTokenCeilingInput.isNull` | — | dropped | follows its filter |

### `ToolTokenCeilingListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolTokenCeilingListInput` | — | dropped | no v2 listing filters a list of `ToolTokenCeiling` (now `ToolResultRule`) |
| `ToolTokenCeilingListInput.some` | — | dropped | follows its list filter |
| `ToolTokenCeilingListInput.every` | — | dropped | follows its list filter |
| `ToolTokenCeilingListInput.none` | — | dropped | follows its list filter |
| `ToolTokenCeilingListInput.isNull` | — | dropped | follows its list filter |

### `ToolTokenCeilingOutput` → `ToolResultRule`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolTokenCeilingOutput` | `ToolResultRule` | merged | one per-tool rule |
| `ToolTokenCeilingOutput.tool` | `ToolResultRule.tool` | reshaped | a `ToolByName` |
| `ToolTokenCeilingOutput.maxResultTokens` | `ToolResultRule.maxResultTokens` | merged |  |

### `ToolUseGuidanceInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolUseGuidanceInput` | — | dropped | nothing filters on `SettingOverrides` in v2 |
| `ToolUseGuidanceInput.batchIndependentCalls` | — | dropped | follows `SettingOverrides.suggestsBatchingCalls`, which v2 does not filter on |
| `ToolUseGuidanceInput.shellForMultiStepWork` | — | dropped | follows `SettingOverrides.suggestsShellForMultiStepWork`, which v2 does not filter on |
| `ToolUseGuidanceInput.and` | — | dropped | follows its filter |
| `ToolUseGuidanceInput.or` | — | dropped | follows its filter |
| `ToolUseGuidanceInput.not` | — | dropped | follows its filter |
| `ToolUseGuidanceInput.isNull` | — | dropped | follows its filter |

### `ToolUseGuidanceOutput` → `SettingOverrides`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ToolUseGuidanceOutput` | `SettingOverrides` | merged | into the cascade |
| `ToolUseGuidanceOutput.batchIndependentCalls` | `SettingOverrides.suggestsBatchingCalls` | reshaped | now `Boolean`; now nullable; null inherits |
| `ToolUseGuidanceOutput.shellForMultiStepWork` | `SettingOverrides.suggestsShellForMultiStepWork` | reshaped | now `Boolean`; now nullable; null inherits |

### `TransformConfigInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TransformConfigInput` | — | dropped | nothing filters on `EdgeTransform` in v2 |
| `TransformConfigInput.carry` | — | dropped | follows `PerRegionTransform.carry`, which v2 does not filter on |
| `TransformConfigInput.carryNames` | — | dropped | follows `PerRegionTransform.carry`, which v2 does not filter on |
| `TransformConfigInput.compact` | — | dropped | follows `PerRegionTransform.compact`, which v2 does not filter on |
| `TransformConfigInput.compactNames` | — | dropped | follows `PerRegionTransform.compact`, which v2 does not filter on |
| `TransformConfigInput.clear` | — | dropped | follows `PerRegionTransform.clear`, which v2 does not filter on |
| `TransformConfigInput.clearNames` | — | dropped | follows `PerRegionTransform.clear`, which v2 does not filter on |
| `TransformConfigInput.compactPrompt` | — | dropped | follows `TransformConfig.compactPrompt`, which went |
| `TransformConfigInput.and` | — | dropped | follows its filter |
| `TransformConfigInput.or` | — | dropped | follows its filter |
| `TransformConfigInput.not` | — | dropped | follows its filter |
| `TransformConfigInput.isNull` | — | dropped | follows its filter |

### `TransformConfigOutput` → `EdgeTransform`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TransformConfigOutput` | `EdgeTransform` | reshaped | split into `CompactTransform` and `PerRegionTransform` |
| `TransformConfigOutput.carry` | `PerRegionTransform.carry` | reshaped | per-region transforms only |
| `TransformConfigOutput.carryNames` | `PerRegionTransform.carry` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `TransformConfigOutput.compact` | `PerRegionTransform.compact` | reshaped | per-region transforms only |
| `TransformConfigOutput.compactNames` | `PerRegionTransform.compact` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `TransformConfigOutput.clear` | `PerRegionTransform.clear` | reshaped | per-region transforms only |
| `TransformConfigOutput.clearNames` | `PerRegionTransform.clear` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `TransformConfigOutput.compactPrompt` | `CompactTransform.prompt`, `PerRegionTransform.compactPrompt` | reshaped | on each type that summarizes |

### `TransitionCondition`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TransitionCondition` | — | dropped | the `TransitionEdge` implementer is the condition |
| `TransitionCondition.ALWAYS` | `AlwaysEdge` | reshaped | a type per condition |
| `TransitionCondition.LLM_CHOICE` | `ModelChoiceEdge` | reshaped | a type per condition |
| `TransitionCondition.ERROR` | `ErrorEdge` | reshaped | a type per condition |
| `TransitionCondition.MAX_ITERATIONS` | `IterationLimitEdge` | reshaped | a type per condition |
| `TransitionCondition.STUCK` | `StuckEdge` | reshaped | a type per condition |
| `TransitionCondition.DEAD_END` | `DeadEndEdge` | reshaped | a type per condition |

### `TransitionConditionFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TransitionConditionFilter` | — | dropped | `TransitionCondition` went |
| `TransitionConditionFilter.eq` | — | dropped | follows `TransitionCondition` |
| `TransitionConditionFilter.ne` | — | dropped | follows `TransitionCondition` |
| `TransitionConditionFilter.in` | — | dropped | follows `TransitionCondition` |
| `TransitionConditionFilter.notIn` | — | dropped | follows `TransitionCondition` |
| `TransitionConditionFilter.isNull` | — | dropped | follows its filter |

### `TransitionEdgeInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TransitionEdgeInput` | — | dropped | nothing filters on `TransitionEdge` in v2 |
| `TransitionEdgeInput.target` | — | dropped | follows `TransitionEdge.target`, which v2 does not filter on |
| `TransitionEdgeInput.targetName` | — | dropped | follows `TransitionEdge.target`, which v2 does not filter on |
| `TransitionEdgeInput.hint` | — | dropped | follows `TransitionEdge.hint`, which went |
| `TransitionEdgeInput.condition` | — | dropped | follows `TransitionEdge.condition`, which went |
| `TransitionEdgeInput.transform` | — | dropped | follows `TransitionEdge.transform`, which v2 does not filter on |
| `TransitionEdgeInput.transformConfig` | — | dropped | follows `TransitionEdge.transform`, which v2 does not filter on |
| `TransitionEdgeInput.gate` | — | dropped | follows `TransitionEdge.gate`, which v2 does not filter on |
| `TransitionEdgeInput.stuck` | — | dropped | follows `StuckEdge.thresholds`, which v2 does not filter on |
| `TransitionEdgeInput.and` | — | dropped | follows its filter |
| `TransitionEdgeInput.or` | — | dropped | follows its filter |
| `TransitionEdgeInput.not` | — | dropped | follows its filter |
| `TransitionEdgeInput.isNull` | — | dropped | follows its filter |

### `TransitionEdgeListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TransitionEdgeListInput` | — | dropped | no v2 listing filters a list of `TransitionEdge` (now `TransitionEdge`) |
| `TransitionEdgeListInput.some` | — | dropped | follows its list filter |
| `TransitionEdgeListInput.every` | — | dropped | follows its list filter |
| `TransitionEdgeListInput.none` | — | dropped | follows its list filter |
| `TransitionEdgeListInput.isNull` | — | dropped | follows its list filter |

### `TransitionEdgeOutput` → `TransitionEdge`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TransitionEdgeOutput` | `TransitionEdge` | reshaped | one type per condition, plus the implicit `FallThroughEdge` |
| `TransitionEdgeOutput.target` | `TransitionEdge.target` | kept |  |
| `TransitionEdgeOutput.targetName` | `TransitionEdge.target` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `TransitionEdgeOutput.hint` | `AlwaysEdge.hint`, `ModelChoiceEdge.hint` | reshaped | read only where the model chooses |
| `TransitionEdgeOutput.condition` | `__typename` | reshaped | the edge type: `AlwaysEdge`, `ModelChoiceEdge`, `ErrorEdge`, `IterationLimitEdge`, `StuckEdge`, `DeadEndEdge` |
| `TransitionEdgeOutput.transform` | `TransitionEdge.transform` | reshaped | an `EdgeTransform` type |
| `TransitionEdgeOutput.transformConfig` | `TransitionEdge.transform` | merged | the `EdgeTransform` type carries its own settings |
| `TransitionEdgeOutput.gate` | `TransitionEdge.gate` | kept |  |
| `TransitionEdgeOutput.stuck` | `StuckEdge.thresholds` | reshaped | only a stuck edge has thresholds |

### `TransitionGateInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TransitionGateInput` | — | dropped | nothing filters on `TransitionGate` in v2 |
| `TransitionGateInput.requireModifications` | — | dropped | follows `TransitionGate.modifications`, which v2 does not filter on |
| `TransitionGateInput.region` | — | dropped | follows `ModificationRequirement.orFilledRegion`, which v2 does not filter on |
| `TransitionGateInput.regionName` | — | dropped | follows `ModificationRequirement.orFilledRegion`, which v2 does not filter on |
| `TransitionGateInput.tools` | — | dropped | follows `ModificationRequirement.countedTools`, which v2 does not filter on |
| `TransitionGateInput.requireRegions` | — | dropped | follows `TransitionGate.filledRegions`, which v2 does not filter on |
| `TransitionGateInput.requireRegionNames` | — | dropped | follows `TransitionGate.filledRegions`, which v2 does not filter on |
| `TransitionGateInput.requireRegionUpdated` | — | dropped | follows `TransitionGate.changedRegion`, which v2 does not filter on |
| `TransitionGateInput.requireRegionUpdatedName` | — | dropped | follows `TransitionGate.changedRegion`, which v2 does not filter on |
| `TransitionGateInput.requireNoOpenItems` | — | dropped | follows `TransitionGate.finishedChecklist`, which v2 does not filter on |
| `TransitionGateInput.requireNoOpenItemsName` | — | dropped | follows `TransitionGate.finishedChecklist`, which v2 does not filter on |
| `TransitionGateInput.requireRegionEntries` | — | dropped | follows `TransitionGate.minimumEntries`, which v2 does not filter on |
| `TransitionGateInput.message` | — | dropped | follows `TransitionGate.message`, which v2 does not filter on |
| `TransitionGateInput.maxAttempts` | — | dropped | follows `TransitionGate.maxAttempts`, which v2 does not filter on |
| `TransitionGateInput.and` | — | dropped | follows its filter |
| `TransitionGateInput.or` | — | dropped | follows its filter |
| `TransitionGateInput.not` | — | dropped | follows its filter |
| `TransitionGateInput.isNull` | — | dropped | follows its filter |

### `TransitionGateOutput` → `TransitionGate`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TransitionGateOutput` | `TransitionGate` | reshaped | the modification requirement nests; name twins go; `finishedChecklist` is typed |
| `TransitionGateOutput.requireModifications` | `TransitionGate.modifications` | reshaped | a `ModificationRequirement`, null when not required: the boolean switched its siblings |
| `TransitionGateOutput.region` | `ModificationRequirement.orFilledRegion` | reshaped | nested under the requirement it modifies |
| `TransitionGateOutput.regionName` | `ModificationRequirement.orFilledRegion` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `TransitionGateOutput.tools` | `ModificationRequirement.countedTools` | reshaped | `ToolByName`s, nested |
| `TransitionGateOutput.requireRegions` | `TransitionGate.filledRegions` | renamed | says what must hold |
| `TransitionGateOutput.requireRegionNames` | `TransitionGate.filledRegions` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `TransitionGateOutput.requireRegionUpdated` | `TransitionGate.changedRegion` | renamed | says what must hold |
| `TransitionGateOutput.requireRegionUpdatedName` | `TransitionGate.changedRegion` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `TransitionGateOutput.requireNoOpenItems` | `TransitionGate.finishedChecklist` | reshaped | a `ChecklistRegion`, as validation requires |
| `TransitionGateOutput.requireNoOpenItemsName` | `TransitionGate.finishedChecklist` | dropped | a name twin: a `Blueprint` only exists when every name resolves, so the relation is enough |
| `TransitionGateOutput.requireRegionEntries` | `TransitionGate.minimumEntries` | renamed | says what must hold; old name kept one release, deprecated |
| `TransitionGateOutput.message` | `TransitionGate.message` | kept |  |
| `TransitionGateOutput.maxAttempts` | `TransitionGate.maxAttempts` | kept |  |

### `TransitionTransform`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TransitionTransform` | — | dropped | the `EdgeTransform` implementer is the transform |
| `TransitionTransform.DIRECT` | `DirectTransform` | reshaped | a type per transform |
| `TransitionTransform.CLEAR` | `ClearTransform` | reshaped | a type per transform |
| `TransitionTransform.COMPACT` | `CompactTransform` | reshaped | a type per transform |
| `TransitionTransform.CUSTOM` | `PerRegionTransform` | reshaped | a type per transform |

### `TransitionTransformFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `TransitionTransformFilter` | — | dropped | `TransitionTransform` went |
| `TransitionTransformFilter.eq` | — | dropped | follows `TransitionTransform` |
| `TransitionTransformFilter.ne` | — | dropped | follows `TransitionTransform` |
| `TransitionTransformFilter.in` | — | dropped | follows `TransitionTransform` |
| `TransitionTransformFilter.notIn` | — | dropped | follows `TransitionTransform` |
| `TransitionTransformFilter.isNull` | — | dropped | follows its filter |

### `UnattendedPolicy`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UnattendedPolicy` | `UnattendedPolicy` | kept |  |
| `UnattendedPolicy.AUTO_APPROVE` | `UnattendedPolicy.AUTO_APPROVE` | kept |  |
| `UnattendedPolicy.ASK` | `UnattendedPolicy.ASK` | kept |  |

### `UnattendedPolicyFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UnattendedPolicyFilter` | — | dropped | nothing filters on `UnattendedPolicy` in v2 |
| `UnattendedPolicyFilter.eq` | — | dropped | follows `UnattendedPolicy` |
| `UnattendedPolicyFilter.ne` | — | dropped | follows `UnattendedPolicy` |
| `UnattendedPolicyFilter.in` | — | dropped | follows `UnattendedPolicy` |
| `UnattendedPolicyFilter.notIn` | — | dropped | follows `UnattendedPolicy` |
| `UnattendedPolicyFilter.isNull` | — | dropped | follows its filter |

### `UntypedCallReason`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UntypedCallReason` | — | dropped | the two reasons are two `ToolArguments` members |
| `UntypedCallReason.NO_TYPE_FOR_THIS_TOOL` | `UntypedToolArguments` | reshaped | a `ToolArguments` member each |
| `UntypedCallReason.ARGUMENTS_DID_NOT_MATCH` | `MalformedArguments` | reshaped | a `ToolArguments` member each |

### `UntypedToolCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UntypedToolCallOutput` | `ToolCall` | merged | one `ToolCall` type; untyped arguments are `UntypedToolArguments` or `MalformedArguments` |
| `UntypedToolCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `UntypedToolCallOutput.toolDescription` | `ToolRevision.description` | merged | on `ToolCall.tool` |
| `UntypedToolCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `UntypedToolCallOutput.reason` | `UntypedToolArguments`, `MalformedArguments` | reshaped | two `ToolArguments` members |

### `UpdateBlueprintEntryOutput` → `BundledBlueprint`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateBlueprintEntryOutput` | `BundledBlueprint` | reshaped | the change is an enum; the installed copy a relation |
| `UpdateBlueprintEntryOutput.name` | `BundledBlueprint.name` | kept |  |
| `UpdateBlueprintEntryOutput.version` | `BundledBlueprint.version` | kept |  |
| `UpdateBlueprintEntryOutput.change` | `BundledBlueprint.change` | reshaped | a `BundledBlueprintChange`, not display text |
| `UpdateBlueprintEntryOutput.hasChanges` | `BundledBlueprint.change` | dropped | a function of `change` |
| `UpdateBlueprintEntryOutput.preselected` | `BundledBlueprint.change` | dropped | a function of `change` |

### `UpdateBlueprintRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateBlueprintRequest` | `UpdateBlueprintRequest` | kept |  |
| `UpdateBlueprintRequest.blueprint` | `UpdateBlueprintRequest.blueprint` | kept |  |
| `UpdateBlueprintRequest.manifest` | `UpdateBlueprintRequest.manifest` | kept |  |

### `UpdateBlueprintResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateBlueprintResult` | `UpdateBlueprintResult` | kept |  |
| `UpdateBlueprintResult.blueprint` | `UpdateBlueprintResult.blueprint` | kept |  |

### `UpdateConfigRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateConfigRequest` | `UpdateConfigRequest` | kept |  |
| `UpdateConfigRequest.set` | `UpdateConfigRequest.routing`, `UpdateConfigRequest.allowsFileUploads` | reshaped | typed fields |
| `UpdateConfigRequest.clear` | `StringSetting.clear` | reshaped | a parallel enum; each setting clears itself |
| `UpdateConfigRequest.providers` | `UpdateConfigRequest.modelProviders` | reshaped | `ModelProviderWrite`s |
| `UpdateConfigRequest.upsertGateways` | `UpdateConfigRequest.modelProviders` | merged | a custom provider is a `ModelProviderWrite` |
| `UpdateConfigRequest.deleteGateways` | `UpdateConfigRequest.deleteModelProviders` | renamed | custom providers |

### `UpdateConfigResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateConfigResult` | `UpdateConfigResult` | kept |  |
| `UpdateConfigResult.config` | `UpdateConfigResult.config` | kept |  |

### `UpdateFinishedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateFinishedEvent` | `UpdateFinishedEvent` | kept |  |
| `UpdateFinishedEvent.seq` | `UpdateFinishedEvent.seq` | kept |  |
| `UpdateFinishedEvent.at` | `UpdateFinishedEvent.at` | kept |  |
| `UpdateFinishedEvent.jobId` | `UpdateFinishedEvent.job` (`UpdateJob.id`) | merged | the job is carried whole |
| `UpdateFinishedEvent.status` | `Job.state` | merged | on `UpdateFinishedEvent.job` |
| `UpdateFinishedEvent.job` | `UpdateFinishedEvent.job` | kept |  |
| `UpdateFinishedEvent.restartRequired` | `UpdateOutcome.restartRequired` | merged | on the job's outcome, and `Server.restartRequired` |

### `UpdateJobConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateJobConnection` | `UpdateJobConnection` | kept |  |
| `UpdateJobConnection.results` | `UpdateJobConnection.results` | kept |  |
| `UpdateJobConnection.cursor` | `UpdateJobConnection.cursor` | kept |  |
| `UpdateJobConnection.total` | `UpdateJobConnection.total` | kept |  |

### `UpdateJobEventFrame`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateJobEventFrame` | `UpdateJobEventFrame` | kept |  |
| `UpdateJobEventFrame.UpdateStepChangedEvent` | `UpdateJobEventFrame.UpdateStepChangedEvent` | kept |  |
| `UpdateJobEventFrame.UpdateFinishedEvent` | `UpdateJobEventFrame.UpdateFinishedEvent` | kept |  |
| `UpdateJobEventFrame.SubscriptionOpenedEvent` | `UpdateJobEventFrame.SubscriptionOpenedEvent` | kept |  |
| `UpdateJobEventFrame.EventsDroppedEvent` | `UpdateJobEventFrame.EventsDroppedEvent` | kept |  |

### `UpdateJobInput` → `UpdateJobFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateJobInput` | `UpdateJobFilter` | kept |  |
| `UpdateJobInput.id` | `UpdateJobFilter.id` | kept |  |
| `UpdateJobInput.status` | — | dropped | follows `Job.state`, which v2 does not filter on |
| `UpdateJobInput.steps` | — | dropped | follows `UpdateJob.steps`, which v2 does not filter on |
| `UpdateJobInput.and` | `UpdateJobFilter.and` | kept |  |
| `UpdateJobInput.or` | `UpdateJobFilter.or` | kept |  |
| `UpdateJobInput.not` | `UpdateJobFilter.not` | kept |  |
| `UpdateJobInput.isNull` | — | dropped | follows its filter |

### `UpdateJobOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateJobOrder` | `UpdateJobOrder` | kept |  |
| `UpdateJobOrder.field` | `UpdateJobOrder.field` | kept |  |
| `UpdateJobOrder.direction` | `UpdateJobOrder.direction` | kept |  |

### `UpdateJobOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateJobOrderField` | `UpdateJobOrderField` | kept |  |
| `UpdateJobOrderField.ID` | — | dropped | not a sort key in v2 |

### `UpdateJobOutput` → `UpdateJob`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateJobOutput` | `UpdateJob` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `UpdateJobOutput.id` | `UpdateJob.id` | reshaped | tagged: `updateJob:<unix seconds>-<n>` |
| `UpdateJobOutput.status` | `Job.state` | reshaped | the `JobState` union |
| `UpdateJobOutput.steps` | `UpdateJob.steps` | reshaped | null when expired, never `[]` |

### `UpdateJobStatus` → `JobState`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateJobStatus` | `JobState` | reshaped | one `JobState` union for every job, carrying what each state knows |
| `UpdateJobStatus.RUNNING` | `JobRunning` | reshaped | a `JobState` type each |
| `UpdateJobStatus.COMPLETE` | `JobComplete` | reshaped | a `JobState` type each |
| `UpdateJobStatus.FAILED` | `JobFailed` | reshaped | a `JobState` type each |

### `UpdateJobStatusFilter` → `JobStateFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateJobStatusFilter` | `JobStateFilter` | renamed | follows `JobState` |
| `UpdateJobStatusFilter.eq` | — | dropped | follows `UpdateJobStatus` |
| `UpdateJobStatusFilter.ne` | — | dropped | follows `UpdateJobStatus` |
| `UpdateJobStatusFilter.in` | — | dropped | follows `UpdateJobStatus` |
| `UpdateJobStatusFilter.notIn` | — | dropped | follows `UpdateJobStatus` |
| `UpdateJobStatusFilter.isNull` | `JobStateFilter.isNull` | kept |  |

### `UpdateJobStepInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateJobStepInput` | — | dropped | nothing filters on `UpdateJobStep` in v2 |
| `UpdateJobStepInput.step` | — | dropped | follows `UpdateJobStep.step`, which v2 does not filter on |
| `UpdateJobStepInput.status` | — | dropped | follows `UpdateJobStep.status`, which v2 does not filter on |
| `UpdateJobStepInput.detail` | — | dropped | follows `UpdateJobStep.detail`, which v2 does not filter on |
| `UpdateJobStepInput.and` | — | dropped | follows its filter |
| `UpdateJobStepInput.or` | — | dropped | follows its filter |
| `UpdateJobStepInput.not` | — | dropped | follows its filter |
| `UpdateJobStepInput.isNull` | — | dropped | follows its filter |

### `UpdateJobStepListInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateJobStepListInput` | — | dropped | no v2 listing filters a list of `UpdateJobStep` (now `UpdateJobStep`) |
| `UpdateJobStepListInput.some` | — | dropped | follows its list filter |
| `UpdateJobStepListInput.every` | — | dropped | follows its list filter |
| `UpdateJobStepListInput.none` | — | dropped | follows its list filter |
| `UpdateJobStepListInput.isNull` | — | dropped | follows its list filter |

### `UpdateJobStepOutput` → `UpdateJobStep`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateJobStepOutput` | `UpdateJobStep` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `UpdateJobStepOutput.step` | `UpdateJobStep.step` | kept |  |
| `UpdateJobStepOutput.status` | `UpdateJobStep.status` | kept |  |
| `UpdateJobStepOutput.detail` | `UpdateJobStep.detail` | kept |  |

### `UpdateMcpServerRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateMcpServerRequest` | `UpdateMcpServerRequest` | kept |  |
| `UpdateMcpServerRequest.server` | `UpdateMcpServerRequest.server` | kept |  |

### `UpdateMcpServerResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateMcpServerResult` | `UpdateMcpServerResult` | kept |  |
| `UpdateMcpServerResult.mcpServer` | `UpdateMcpServerResult.mcpServer` | kept |  |

### `UpdateMigrationOutput` → `ConfigMigration`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateMigrationOutput` | `ConfigMigration` | renamed | a config migration |
| `UpdateMigrationOutput.name` | `ConfigMigration.name` | kept |  |
| `UpdateMigrationOutput.description` | `ConfigMigration.description` | kept |  |

### `UpdatePlanOutput` → `UpdatePlan`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdatePlanOutput` | `UpdatePlan` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `UpdatePlanOutput.version` | `Server.version` | merged | the server's own fact |
| `UpdatePlanOutput.installMethod` | `Installation.method` | reshaped | on `Server.installation` |
| `UpdatePlanOutput.channel` | `Installation.channel` | reshaped | a `ReleaseChannel`, on `Server.installation` |
| `UpdatePlanOutput.latest` | `Release.version` | reshaped | on `UpdatePlan.latestRelease` |
| `UpdatePlanOutput.updateAvailable` | `Release.isNewer` | reshaped | on `UpdatePlan.latestRelease` |
| `UpdatePlanOutput.checkedAt` | `Release.checkedAt` | reshaped | on `UpdatePlan.latestRelease` |
| `UpdatePlanOutput.binary` | `UpdatePlan.binary` | kept |  |
| `UpdatePlanOutput.blueprints` | `UpdatePlan.blueprints` | kept |  |
| `UpdatePlanOutput.migrations` | `UpdatePlan.migrations` | reshaped | null when the config cannot be read |
| `UpdatePlanOutput.configError` | `Config.problem` | dropped | an untyped copy of the config's problem |

### `UpdateStep`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateStep` | `UpdateStep` | kept |  |
| `UpdateStep.BINARY` | `UpdateStep.BINARY` | kept |  |
| `UpdateStep.BLUEPRINTS` | `UpdateStep.BLUEPRINTS` | kept |  |
| `UpdateStep.KEYS` | `UpdateStep.KEYS` | kept |  |
| `UpdateStep.MIGRATIONS` | `UpdateStep.MIGRATIONS` | kept |  |

### `UpdateStepChangedEvent`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateStepChangedEvent` | `UpdateStepChangedEvent` | kept |  |
| `UpdateStepChangedEvent.seq` | `UpdateStepChangedEvent.seq` | kept |  |
| `UpdateStepChangedEvent.at` | `UpdateStepChangedEvent.at` | kept |  |
| `UpdateStepChangedEvent.jobId` | `UpdateStepChangedEvent.job` (`UpdateJob.id`) | merged | the job is carried whole |
| `UpdateStepChangedEvent.step` | `UpdateStepChangedEvent.step` | kept |  |
| `UpdateStepChangedEvent.status` | `UpdateStepChangedEvent.status` | kept |  |
| `UpdateStepChangedEvent.detail` | `UpdateStepChangedEvent.detail` | kept |  |

### `UpdateStepFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateStepFilter` | — | dropped | nothing filters on `UpdateStep` in v2 |
| `UpdateStepFilter.eq` | — | dropped | follows `UpdateStep` |
| `UpdateStepFilter.ne` | — | dropped | follows `UpdateStep` |
| `UpdateStepFilter.in` | — | dropped | follows `UpdateStep` |
| `UpdateStepFilter.notIn` | — | dropped | follows `UpdateStep` |
| `UpdateStepFilter.isNull` | — | dropped | follows its filter |

### `UpdateStepStatus`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateStepStatus` | `UpdateStepStatus` | kept |  |
| `UpdateStepStatus.PENDING` | `UpdateStepStatus.PENDING` | kept |  |
| `UpdateStepStatus.RUNNING` | `UpdateStepStatus.RUNNING` | kept |  |
| `UpdateStepStatus.DONE` | `UpdateStepStatus.DONE` | kept |  |
| `UpdateStepStatus.SKIPPED` | `UpdateStepStatus.NOT_REQUESTED`, `UpdateStepStatus.NOTHING_TO_DO` | reshaped | it meant two things |
| `UpdateStepStatus.ADVISED` | `UpdateStepStatus.ADVISED` | kept |  |
| `UpdateStepStatus.FAILED` | `UpdateStepStatus.FAILED` | kept |  |

### `UpdateStepStatusFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpdateStepStatusFilter` | — | dropped | nothing filters on `UpdateStepStatus` in v2 |
| `UpdateStepStatusFilter.eq` | — | dropped | follows `UpdateStepStatus` |
| `UpdateStepStatusFilter.ne` | — | dropped | follows `UpdateStepStatus` |
| `UpdateStepStatusFilter.in` | — | dropped | follows `UpdateStepStatus` |
| `UpdateStepStatusFilter.notIn` | — | dropped | follows `UpdateStepStatus` |
| `UpdateStepStatusFilter.isNull` | — | dropped | follows its filter |

### `UpgradeByAdviceOutput` → `UpgradeByAdvice`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpgradeByAdviceOutput` | `UpgradeByAdvice` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `UpgradeByAdviceOutput.message` | `UpgradeByAdvice.message` | kept |  |

### `UpgradeByCommandOutput` → `UpgradeByCommand`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpgradeByCommandOutput` | `UpgradeByCommand` | kept | suffix rule: `XOutput` is `X`, `XInput` is `XFilter` |
| `UpgradeByCommandOutput.commands` | `UpgradeByCommand.commands` | kept |  |
| `UpgradeByCommandOutput.shell` | `UpgradeByCommand.shell` | kept |  |

### `UpsertMimeRowRequest` → `UpsertMimeTypeRuleRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpsertMimeRowRequest` | `UpsertMimeTypeRuleRequest` | renamed | a rule of the mime registry |
| `UpsertMimeRowRequest.row` | `UpsertMimeTypeRuleRequest.rule` | renamed |  |

### `UpsertMimeRowResult` → `UpsertMimeTypeRuleResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpsertMimeRowResult` | `UpsertMimeTypeRuleResult` | renamed | a rule of the mime registry |
| `UpsertMimeRowResult.mimeRow` | `UpsertMimeTypeRuleResult.rule` | reshaped | the `OperatorMimeTypeRule`, plus the resolved `mimeType` |
| `UpsertMimeRowResult.isNew` | `UpsertMimeTypeRuleResult.isNew` | kept |  |

### `UpsertScriptRequest` → `UpsertExtensionRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpsertScriptRequest` | `UpsertExtensionRequest` | renamed | extension, not script |
| `UpsertScriptRequest.script` | `UpsertExtensionRequest.extension` | reshaped | an `ExtensionRef` |
| `UpsertScriptRequest.content` | `UpsertExtensionRequest.content` | kept |  |

### `UpsertScriptResult` → `UpsertExtensionResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpsertScriptResult` | `UpsertExtensionResult` | renamed | extension, not script |
| `UpsertScriptResult.script` | `UpsertExtensionResult.extension` | reshaped | an `Extension` |

### `UpsertYoloProfileRequest` → `UpsertApprovalPolicyRequest`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpsertYoloProfileRequest` | `UpsertApprovalPolicyRequest` | renamed | plain English |
| `UpsertYoloProfileRequest.profile` | `UpsertApprovalPolicyRequest.policy` | renamed |  |

### `UpsertYoloProfileResult` → `UpsertApprovalPolicyResult`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `UpsertYoloProfileResult` | `UpsertApprovalPolicyResult` | renamed | plain English |
| `UpsertYoloProfileResult.yoloProfile` | `UpsertApprovalPolicyResult.approvalPolicy` | renamed | old name kept one release, deprecated |
| `UpsertYoloProfileResult.isNew` | `UpsertApprovalPolicyResult.isNew` | kept |  |

### `ValidationReportOutput` → `Validation`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ValidationReportOutput` | `Validation` | reshaped | typed problems, not `"message [code]"` strings |
| `ValidationReportOutput.valid` | `Validation.valid` | kept |  |
| `ValidationReportOutput.errors` | `Validation.problems` | reshaped | `BlueprintProblem`s with severity `ERROR` |
| `ValidationReportOutput.warnings` | `Validation.problems` | reshaped | `BlueprintProblem`s with severity `WARNING` or `NOTE` |

### `ValidatorErrorPolicy` → `ValidatorFailurePolicy`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ValidatorErrorPolicy` | `ValidatorFailurePolicy` | reshaped | what happens when the validator itself fails; the published description had it backwards |
| `ValidatorErrorPolicy.REJECT` | `ValidatorFailurePolicy.REFUSE_SUBMISSION` | renamed | says what is refused |
| `ValidatorErrorPolicy.ACCEPT` | `ValidatorFailurePolicy.ACCEPT_UNCHECKED` | renamed | says it goes unchecked |

### `ValidatorErrorPolicyFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `ValidatorErrorPolicyFilter` | — | dropped | nothing filters on `ValidatorFailurePolicy` in v2 |
| `ValidatorErrorPolicyFilter.eq` | — | dropped | follows `ValidatorErrorPolicy` |
| `ValidatorErrorPolicyFilter.ne` | — | dropped | follows `ValidatorErrorPolicy` |
| `ValidatorErrorPolicyFilter.in` | — | dropped | follows `ValidatorErrorPolicy` |
| `ValidatorErrorPolicyFilter.notIn` | — | dropped | follows `ValidatorErrorPolicy` |
| `ValidatorErrorPolicyFilter.isNull` | — | dropped | follows its filter |

### `WaitForAgentArgsOutput` → `SubAgentArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `WaitForAgentArgsOutput` | `SubAgentArguments` | merged | same arguments as `check_agent` |
| `WaitForAgentArgsOutput.agentId` | `SubAgentArguments.runId` | renamed | the run id the model wrote, with `run` resolving it |

### `WaitForAgentCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `WaitForAgentCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `WaitForAgentCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `WaitForAgentCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `WaitForAgentCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `WaitForAgentCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `SubAgentArguments` for `wait_for_agent` |

### `WaitReasonInput` → `RunStateFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `WaitReasonInput` | `RunStateFilter` | renamed | follows `RunState` |
| `WaitReasonInput.reason` | — | dropped | follows `WaitReason.reason`, which went |
| `WaitReasonInput.blocker` | `NeedsSetupFilter.blocker` | renamed | follows `NeedsSetup.blocker` |
| `WaitReasonInput.remedy` | `NeedsSetupFilter.remedy` | renamed | follows `NeedsSetup.remedy` |
| `WaitReasonInput.outstanding` | `WaitingOnSubAgentsFilter.outstanding` | renamed | follows `WaitingOnSubAgents.outstanding` |
| `WaitReasonInput.needsAPerson` | — | dropped | follows `WaitReason.needsAPerson`, which went |
| `WaitReasonInput.and` | `RunStateFilter.and` | reshaped | now `[RunStateFilter]` |
| `WaitReasonInput.or` | `RunStateFilter.or` | reshaped | now `[RunStateFilter]` |
| `WaitReasonInput.not` | `RunStateFilter.not` | reshaped | now `RunStateFilter` |
| `WaitReasonInput.isNull` | — | dropped | follows its filter |

### `WaitReasonKind`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `WaitReasonKind` | — | dropped | the `RunState` member is the reason |
| `WaitReasonKind.TOOL_APPROVAL` | `ToolApproval` | reshaped | a person's wait is `WaitingOnPerson` with the question's own type; the rest are `RunState` types |
| `WaitReasonKind.USER_PROMPT` | `TextQuestion`, `ChoiceQuestion`, `ConfirmQuestion`, `DocumentEdit` | dropped | a person's wait is `WaitingOnPerson` with the question's own type; the rest are `RunState` types |
| `WaitReasonKind.TAINT_GATE` | `TaintGateApproval` | reshaped | a person's wait is `WaitingOnPerson` with the question's own type; the rest are `RunState` types |
| `WaitReasonKind.INTERACTION_POINT` | `CheckpointRound` | reshaped | a person's wait is `WaitingOnPerson` with the question's own type; the rest are `RunState` types |
| `WaitReasonKind.FAN_OUT_WORKERS` | `WaitingOnSubAgents` | reshaped | a person's wait is `WaitingOnPerson` with the question's own type; the rest are `RunState` types |
| `WaitReasonKind.CHILDREN` | `WaitingOnSubAgents` | reshaped | a person's wait is `WaitingOnPerson` with the question's own type; the rest are `RunState` types |
| `WaitReasonKind.NEEDS_SETUP` | `NeedsSetup` | reshaped | a person's wait is `WaitingOnPerson` with the question's own type; the rest are `RunState` types |

### `WaitReasonKindFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `WaitReasonKindFilter` | — | dropped | `WaitReasonKind` went |
| `WaitReasonKindFilter.eq` | — | dropped | follows `WaitReasonKind` |
| `WaitReasonKindFilter.ne` | — | dropped | follows `WaitReasonKind` |
| `WaitReasonKindFilter.in` | — | dropped | follows `WaitReasonKind` |
| `WaitReasonKindFilter.notIn` | — | dropped | follows `WaitReasonKind` |
| `WaitReasonKindFilter.isNull` | — | dropped | follows its filter |

### `WaitReasonOutput` → `RunState`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `WaitReasonOutput` | `RunState` | reshaped | folded into the `RunState` types (`WaitingOnPerson`, `WaitingOnSubAgents`, `NeedsSetup`, `WaitingUnexplained`) |
| `WaitReasonOutput.reason` | `__typename` | reshaped | the `RunState` type is the reason |
| `WaitReasonOutput.blocker` | `NeedsSetup.blocker` | reshaped | only a setup wait has a blocker |
| `WaitReasonOutput.remedy` | `NeedsSetup.remedy` | reshaped | only a setup wait has a remedy |
| `WaitReasonOutput.outstanding` | `WaitingOnSubAgents.outstanding` | reshaped | only a wait on sub-agents has one |
| `WaitReasonOutput.needsAPerson` | `WaitingOnPerson`, `WaitingOnSubAgents.questionsBelow` | reshaped | the type says it; the filter asks `or: [{ waitingOnPerson: {} }, { waitingOnSubAgents: { questionsBelow: { some: {} } } }]` |

### `WhichCommandArgsOutput` → `WhichCommandArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `WhichCommandArgsOutput` | `WhichCommandArguments` | renamed | argument types are named `XArguments` |
| `WhichCommandArgsOutput.command` | `WhichCommandArguments.commandName` | renamed | a name, not a command line |

### `WhichCommandCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `WhichCommandCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `WhichCommandCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `WhichCommandCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `WhichCommandCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `WhichCommandCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `WhichCommandArguments` for `which_command` |

### `WorkerFailurePolicy`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `WorkerFailurePolicy` | `WorkerFailurePolicy` | kept |  |
| `WorkerFailurePolicy.CONTINUE` | `WorkerFailurePolicy.CONTINUE` | kept |  |
| `WorkerFailurePolicy.FAIL_ALL` | `WorkerFailurePolicy.FAIL_ALL` | kept |  |

### `WorkerFailurePolicyFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `WorkerFailurePolicyFilter` | — | dropped | nothing filters on `WorkerFailurePolicy` in v2 |
| `WorkerFailurePolicyFilter.eq` | — | dropped | follows `WorkerFailurePolicy` |
| `WorkerFailurePolicyFilter.ne` | — | dropped | follows `WorkerFailurePolicy` |
| `WorkerFailurePolicyFilter.in` | — | dropped | follows `WorkerFailurePolicy` |
| `WorkerFailurePolicyFilter.notIn` | — | dropped | follows `WorkerFailurePolicy` |
| `WorkerFailurePolicyFilter.isNull` | — | dropped | follows its filter |

### `WorkingClockInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `WorkingClockInput` | — | dropped | `WorkingClock` went |
| `WorkingClockInput.bankedSecs` | — | dropped | follows `WorkingClock` |
| `WorkingClockInput.since` | — | dropped | follows `WorkingClock` |
| `WorkingClockInput.and` | — | dropped | follows its filter |
| `WorkingClockInput.or` | — | dropped | follows its filter |
| `WorkingClockInput.not` | — | dropped | follows its filter |
| `WorkingClockInput.isNull` | — | dropped | follows its filter |

### `WorkingClockOutput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `WorkingClockOutput` | — | dropped | two numbers, now `Run.workingSeconds` and `Run.workingSince` |
| `WorkingClockOutput.bankedSecs` | `Run.workingSeconds` | merged | the total, parked time excluded |
| `WorkingClockOutput.since` | `Run.workingSince` | merged |  |

### `WriteFileArgsOutput` → `WriteFileArguments`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `WriteFileArgsOutput` | `WriteFileArguments` | renamed | argument types are named `XArguments` |
| `WriteFileArgsOutput.path` | `WriteFileArguments.path` | kept |  |
| `WriteFileArgsOutput.content` | `WriteFileArguments.content` | kept |  |
| `WriteFileArgsOutput.append` | `WriteFileArguments.append` | kept |  |

### `WriteFileCallOutput` → `ToolCall`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `WriteFileCallOutput` | `ToolCall` | merged | the per-tool call wrappers repeated the same fields 36 times; one `ToolCall`, typed by its `arguments` |
| `WriteFileCallOutput.toolName` | `ToolCall.toolName` | kept |  |
| `WriteFileCallOutput.toolDescription` | `ToolRevision.description` | merged | always null on an execution; the description the model was shown is on `ToolCall.tool` |
| `WriteFileCallOutput.rawArguments` | `ToolCall.rawArguments` | kept |  |
| `WriteFileCallOutput.args` | `ToolCall.arguments` | reshaped | now `ToolArguments`; now nullable; a `WriteFileArguments` for `write_file` |

### `YoloDecisionOutput` → `ApprovalDecision`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloDecisionOutput` | `ApprovalDecision` | renamed | plain English |
| `YoloDecisionOutput.tool` | `ApprovalDecision.toolName` | renamed | a name, with `tool` resolving it |
| `YoloDecisionOutput.configured` | `ApprovalDecision.configured` | kept |  |
| `YoloDecisionOutput.policy` | `ApprovalDecision.decided` | renamed | what the policy decided; old name kept one release, deprecated |
| `YoloDecisionOutput.reason` | `ApprovalDecision.reason` | kept |  |

### `YoloHuman` → `HumanInTheLoop`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloHuman` | `HumanInTheLoop` | renamed | plain English |
| `YoloHuman.ASK` | `HumanInTheLoop.ASK` | kept |  |
| `YoloHuman.AUTO` | `HumanInTheLoop.AUTO` | kept |  |

### `YoloHumanFilter` → `HumanInTheLoopFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloHumanFilter` | `HumanInTheLoopFilter` | renamed | follows `HumanInTheLoop` |
| `YoloHumanFilter.eq` | `HumanInTheLoopFilter.eq` | kept |  |
| `YoloHumanFilter.ne` | `HumanInTheLoopFilter.ne` | kept |  |
| `YoloHumanFilter.in` | `HumanInTheLoopFilter.in` | kept |  |
| `YoloHumanFilter.notIn` | `HumanInTheLoopFilter.notIn` | kept |  |
| `YoloHumanFilter.isNull` | `HumanInTheLoopFilter.isNull` | kept |  |

### `YoloProfileConnection` → `ApprovalPolicyConnection`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloProfileConnection` | `ApprovalPolicyConnection` | renamed | follows `ApprovalPolicy` |
| `YoloProfileConnection.results` | `ApprovalPolicyConnection.results` | renamed |  |
| `YoloProfileConnection.cursor` | `ApprovalPolicyConnection.cursor` | renamed |  |
| `YoloProfileConnection.total` | `ApprovalPolicyConnection.total` | renamed |  |

### `YoloProfileInput` → `ApprovalPolicyFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloProfileInput` | `ApprovalPolicyFilter` | renamed | follows `ApprovalPolicy` |
| `YoloProfileInput.id` | `ApprovalPolicyFilter.id` | renamed | follows `ApprovalPolicy.id` |
| `YoloProfileInput.name` | `ApprovalPolicyFilter.name` | renamed | follows `ApprovalPolicy.name` |
| `YoloProfileInput.default` | `ApprovalPolicyRevisionFilter.unlistedTools` | reshaped | now `ApprovalWaiverFilter`; follows `ApprovalPolicyRevision.unlistedTools` |
| `YoloProfileInput.questions` | `ApprovalPolicyRevisionFilter.questions` | reshaped | now `HumanInTheLoopFilter`; follows `ApprovalPolicyRevision.questions` |
| `YoloProfileInput.checkpoints` | `ApprovalPolicyRevisionFilter.checkpoints` | reshaped | now `HumanInTheLoopFilter`; follows `ApprovalPolicyRevision.checkpoints` |
| `YoloProfileInput.gate` | `ApprovalPolicyRevisionFilter.taintGate` | reshaped | now `HumanInTheLoopFilter`; follows `ApprovalPolicyRevision.taintGate` |
| `YoloProfileInput.toolRules` | — | dropped | follows `ApprovalPolicyRevision.toolRules`, which v2 does not filter on |
| `YoloProfileInput.shellRules` | — | dropped | follows `ApprovalPolicyRevision.shellRules`, which v2 does not filter on |
| `YoloProfileInput.and` | `ApprovalPolicyFilter.and` | reshaped | now `[ApprovalPolicyFilter]` |
| `YoloProfileInput.or` | `ApprovalPolicyFilter.or` | reshaped | now `[ApprovalPolicyFilter]` |
| `YoloProfileInput.not` | `ApprovalPolicyFilter.not` | reshaped | now `ApprovalPolicyFilter` |
| `YoloProfileInput.isNull` | `ApprovalPolicyFilter.isNull` | kept |  |

### `YoloProfileOrder` → `ApprovalPolicyOrder`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloProfileOrder` | `ApprovalPolicyOrder` | renamed | follows `ApprovalPolicy` |
| `YoloProfileOrder.field` | `ApprovalPolicyOrder.field` | reshaped | now `ApprovalPolicyOrderField` |
| `YoloProfileOrder.direction` | `ApprovalPolicyOrder.direction` | renamed |  |

### `YoloProfileOrderField` → `ApprovalPolicyOrderField`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloProfileOrderField` | `ApprovalPolicyOrderField` | renamed | follows `ApprovalPolicy` |
| `YoloProfileOrderField.ID` | `ApprovalPolicyOrderField.ID` | kept |  |
| `YoloProfileOrderField.NAME` | `ApprovalPolicyOrderField.NAME` | kept |  |

### `YoloProfileOutput` → `ApprovalPolicy`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloProfileOutput` | `ApprovalPolicy` | reshaped | plain English (`--yolo=<name>` in the description); the rules are on its revision, which runs record |
| `YoloProfileOutput.id` | `ApprovalPolicy.id` | reshaped | `approvalPolicy:<name>` |
| `YoloProfileOutput.name` | `ApprovalPolicy.name` | kept |  |
| `YoloProfileOutput.default` | `ApprovalPolicyRevision.unlistedTools` | renamed | says what it governs |
| `YoloProfileOutput.questions` | `ApprovalPolicyRevision.questions` | kept | on the revision |
| `YoloProfileOutput.checkpoints` | `ApprovalPolicyRevision.checkpoints` | kept | on the revision |
| `YoloProfileOutput.gate` | `ApprovalPolicyRevision.taintGate` | renamed | says which gate |
| `YoloProfileOutput.toolRules` | `ApprovalPolicyRevision.toolRules` | reshaped | a list of `ToolPermissionRule` |
| `YoloProfileOutput.shellRules` | `ApprovalPolicyRevision.shellRules` | reshaped | a list of `ShellRule` |
| `YoloProfileOutput.decide` | `ApprovalPolicy.decide` | kept |  |
| `YoloProfileOutput.decide(tool:)` | `ApprovalPolicy.decide(toolName:)` | renamed | a name |
| `YoloProfileOutput.decide(kind:)` | `ApprovalPolicy.decide(blueprintName:)` | dropped | the server classifies the tool; the blueprint names its own tools |
| `YoloProfileOutput.decide(args:)` | `ApprovalPolicy.decide(arguments:)` | renamed | no abbreviations |

### `YoloProfileWrite` → `ApprovalPolicyWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloProfileWrite` | `ApprovalPolicyWrite` | reshaped | rules are lists of `ToolPermissionRuleWrite` and `ShellRuleWrite`, each with its permission |
| `YoloProfileWrite.name` | `ApprovalPolicyWrite.name` | kept |  |
| `YoloProfileWrite.default` | `ApprovalPolicyWrite.unlistedTools` | renamed |  |
| `YoloProfileWrite.questions` | `ApprovalPolicyWrite.questions` | kept |  |
| `YoloProfileWrite.checkpoints` | `ApprovalPolicyWrite.checkpoints` | kept |  |
| `YoloProfileWrite.gate` | `ApprovalPolicyWrite.taintGate` | renamed |  |
| `YoloProfileWrite.toolRules` | `ApprovalPolicyWrite.toolRules` | reshaped | a list of `ToolPermissionRuleWrite` |
| `YoloProfileWrite.shellRules` | `ApprovalPolicyWrite.shellRules` | reshaped | a list of `ShellRuleWrite` |

### `YoloShellRulesInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloShellRulesInput` | — | dropped | nothing filters on `ShellRule` in v2 |
| `YoloShellRulesInput.allow` | — | dropped | follows `YoloShellRules.allow`, which went |
| `YoloShellRulesInput.ask` | — | dropped | follows `YoloShellRules.ask`, which went |
| `YoloShellRulesInput.deny` | — | dropped | follows `YoloShellRules.deny`, which went |
| `YoloShellRulesInput.and` | — | dropped | follows its filter |
| `YoloShellRulesInput.or` | — | dropped | follows its filter |
| `YoloShellRulesInput.not` | — | dropped | follows its filter |
| `YoloShellRulesInput.isNull` | — | dropped | follows its filter |

### `YoloShellRulesOutput` → `ShellRule`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloShellRulesOutput` | `ShellRule` | merged | the allow/ask/deny triple becomes one list of rules, each with its `permission` |
| `YoloShellRulesOutput.allow` | `ShellRule.permission` = `ToolPermission.ALLOW` | merged | each rule carries its permission |
| `YoloShellRulesOutput.ask` | `ShellRule.permission` = `ToolPermission.ASK` | merged | each rule carries its permission |
| `YoloShellRulesOutput.deny` | `ShellRule.permission` = `ToolPermission.DENY` | merged | each rule carries its permission |

### `YoloShellRulesWrite` → `ShellRuleWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloShellRulesWrite` | `ShellRuleWrite` | merged | one list of rules, each with its `permission` |
| `YoloShellRulesWrite.allow` | `ShellRuleWrite.permission` = `ToolPermission.ALLOW` | merged | each rule carries its permission |
| `YoloShellRulesWrite.ask` | `ShellRuleWrite.permission` = `ToolPermission.ASK` | merged | each rule carries its permission |
| `YoloShellRulesWrite.deny` | `ShellRuleWrite.permission` = `ToolPermission.DENY` | merged | each rule carries its permission |

### `YoloToolRulesInput`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloToolRulesInput` | — | dropped | nothing filters on `ToolPermissionRule` in v2 |
| `YoloToolRulesInput.allow` | — | dropped | follows `YoloToolRules.allow`, which went |
| `YoloToolRulesInput.ask` | — | dropped | follows `YoloToolRules.ask`, which went |
| `YoloToolRulesInput.deny` | — | dropped | follows `YoloToolRules.deny`, which went |
| `YoloToolRulesInput.and` | — | dropped | follows its filter |
| `YoloToolRulesInput.or` | — | dropped | follows its filter |
| `YoloToolRulesInput.not` | — | dropped | follows its filter |
| `YoloToolRulesInput.isNull` | — | dropped | follows its filter |

### `YoloToolRulesOutput` → `ToolPermissionRule`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloToolRulesOutput` | `ToolPermissionRule` | merged | one list of `ToolPermissionRule`s, each with a typed selector |
| `YoloToolRulesOutput.allow` | `ToolPermissionRule.permission` = `ToolPermission.ALLOW` | merged | each rule carries its permission |
| `YoloToolRulesOutput.ask` | `ToolPermissionRule.permission` = `ToolPermission.ASK` | merged | each rule carries its permission |
| `YoloToolRulesOutput.deny` | `ToolPermissionRule.permission` = `ToolPermission.DENY` | merged | each rule carries its permission |

### `YoloToolRulesWrite` → `ToolPermissionRuleWrite`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloToolRulesWrite` | `ToolPermissionRuleWrite` | merged | one list of rules |
| `YoloToolRulesWrite.allow` | `ToolPermissionRuleWrite.permission` = `ToolPermission.ALLOW` | merged | each rule carries its permission |
| `YoloToolRulesWrite.ask` | `ToolPermissionRuleWrite.permission` = `ToolPermission.ASK` | merged | each rule carries its permission |
| `YoloToolRulesWrite.deny` | `ToolPermissionRuleWrite.permission` = `ToolPermission.DENY` | merged | each rule carries its permission |

### `YoloWaiver` → `ApprovalWaiver`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloWaiver` | `ApprovalWaiver` | renamed | plain English |
| `YoloWaiver.ALLOW` | `ApprovalWaiver.ALLOW` | kept |  |
| `YoloWaiver.ASK` | `ApprovalWaiver.ASK` | kept |  |

### `YoloWaiverFilter` → `ApprovalWaiverFilter`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloWaiverFilter` | `ApprovalWaiverFilter` | renamed | follows `ApprovalWaiver` |
| `YoloWaiverFilter.eq` | `ApprovalWaiverFilter.eq` | kept |  |
| `YoloWaiverFilter.ne` | `ApprovalWaiverFilter.ne` | kept |  |
| `YoloWaiverFilter.in` | `ApprovalWaiverFilter.in` | kept |  |
| `YoloWaiverFilter.notIn` | `ApprovalWaiverFilter.notIn` | kept |  |
| `YoloWaiverFilter.isNull` | `ApprovalWaiverFilter.isNull` | kept |  |

### `YoloWrite` → `ApprovalPolicyRef`

| v1 | v2 | verdict | why |
|---|---|---|---|
| `YoloWrite` | `ApprovalPolicyRef` | merged | bare `--yolo` is the built-in policy named `"default"` |
| `YoloWrite.everything` | `ApprovalPolicyRef.name` `"default"` | merged | a `@oneOf` member whose only value was `true` |
| `YoloWrite.profileName` | `ApprovalPolicyRef.name` | merged |  |
