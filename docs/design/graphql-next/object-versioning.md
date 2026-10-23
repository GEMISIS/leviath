# History on the object

**Status: part of a design proposal.** Nothing here is implemented. It describes the history model
in [`leviath.graphql`](leviath.graphql), and what the daemon would have to store to make it true.

Two rules:

1. **Every edited thing keeps its past on itself**: the revision in force now, every earlier
   revision, and the edits between them. There is no separate history service and no second event
   model.
2. **A record of something that happened points at the exact revision it used**, typed and
   non-null wherever the runtime can know it. What the thing is now is one hop away.

## What is versioned

Every implementer of `interface Versioned` has `revision` (now), `revisions(first, after)` (newest
first, typed to its own revision type) and `history(first, after)` (its edits, newest first).

| Owner | Revision type | What a revision holds |
|---|---|---|
| `Blueprint` | `BlueprintRevision` | The manifest text, the parsed graph (stages, regions, edges, settings), and an `ExtensionRevision` for every Rhai file the manifest names |
| `Extension` (eight types) | `ExtensionRevision` | The file's text |
| `Tool` (four types) | `ToolRevision` | The name, description and argument schema as a model was shown them |
| `ApprovalPolicy` | `ApprovalPolicyRevision` | The rules: unlisted-tool waiver, question and checkpoint handling, taint gate, tool and shell rules |
| `McpServer` (three types) | `McpServerRevision` | The entry as written, with every environment and header value redacted |
| `ModelProvider` (seven types) | `ModelProviderRevision` | The provider's settings, a secret reading `"<set>"` or null |
| `Config` | `ConfigRevision` | The file parsed, every secret redacted |
| `OperatorMimeTypeRule` | `OperatorMimeTypeRuleRevision` | The rule as the operator wrote it |

Every revision implements `interface Revision`:

- `id`: the owner's id, `@`, and the first 12 hex digits of the digest
  (`blueprint:coder@3f9a1c0d8e77`). `node(id:)` resolves any revision this machine recorded,
  including one only a run still refers to.
- `number`: from 1, in the order this machine first saw the owner's revisions.
- `digest`: lowercase hex SHA-256 of the content, so the same content always has the same digest.
  For content that holds secrets (an MCP server, a model provider, the config) it is an HMAC under
  the machine's link-signing key, so it reveals nothing about them.
- `createdAt`, and `createdBy`: the name the API caller authenticated as, `"cli"` for the command
  line, or null when the file was edited on disk and Leviath only saw the result.

A revision never changes once recorded.

**What is not versioned**, because it is a record rather than an edited thing: runs, stages as a
run met them, visits, turns, model attempts, tool executions, interactions, context windows and
jobs. A context window is content-addressed (`ContextSnapshot.digest`) and `ContextChange` records
each transaction between two windows with `before` and `after`, but a window is not edited: it is
superseded. `Model` is not versioned either (see the open questions).

## What a record pins

| Record | Field | Points at |
|---|---|---|
| A run | `Run.blueprint` | `BlueprintRevision!`: the manifest and every named file as the run took them at spawn |
| A run | `Run.approvalPolicy` | `ApprovalPolicyRevision`, null for an attended run |
| A stage the run met | `RunStage.definition` | the `Stage` inside the run's own `BlueprintRevision` |
| A stage's hooks | `StageHooks.*` | `ExtensionRevision`, as the revision holds the file |
| A scripted region | `ScriptedRegion.hook` | `ExtensionRevision` |
| A tool call | `ToolCall.tool` | `CalledTool = ToolRevision \| ToolNotOffered`: the definition the model was shown, or the fact that it was shown none by that name |
| A model attempt | `ModelAttempt.provider` | `ModelProviderRevision`: the provider's settings as that attempt used them |
| A model used by a run | `ModelUse.provider` | `ModelProviderRevision` |
| A broken-script finding | `ScriptsBroken.extensions` | `ExtensionRevision`s |

Going from a record to the present is one hop, and the hop is explicit:
`run.blueprint.blueprint.revision` is what is installed under that name now, and
`toolCall.tool.tool.revision` is the tool as it is offered now. Either may be null once the thing
is gone; the record is not.

A run recorded before blueprint snapshots existed has only the installed revision to show.
`Run.blueprintSource` says which case applies: `SNAPSHOT` is exactly what ran;
`INSTALLED_FALLBACK` is the installed revision standing in, which may differ from what ran. A
client that needs certainty must check it.

## Edits

An `Edit` is one write to a `Versioned` thing:

- `subjectId`, kept after the thing is deleted, and `subject`, the thing now (null once deleted);
- `revision`, the revision the edit made (null for a deletion), and its `number`;
- `at`, `by` (as `Revision.createdBy`) and `why`, the `reason` the writer sent;
- `changes: [FieldChange!]!`, each with a dotted `field` path and its `before` and `after`
  values. A deletion lists every field with a null `after`.

`Versioned.history` lists one thing's edits. **`Query.changes(since:)` lists every thing's**,
oldest first, with a filter on the subject, the time and the author, over the same `Edit` type.
"What changed on this machine since this morning" is one call.

## Writes

- **Pin to what you read.** Upserts, updates and deletes of extensions, MCP servers, approval
  policies, operator mime rules and the config take `expectedRevision`, the `digest` of the
  revision the caller read. Blueprints take the same pin as `BlueprintRef.digest`. A stale pin is
  refused whole with `CONFLICT` / `REVISION_CONFLICT`, and `extensions.current` carries the thing
  as it is now. Omitting the pin means last write wins.
- **Say why.** Every write that makes a revision takes an optional `reason`, kept as the edit's
  `why`.
- **Retries are safe.** Writing the content that is already current makes no new revision and
  answers `alreadyInState: true`.
- **Model providers** are written through `updateConfig`, so the config's revision is their pin.

## Blueprint digests

A blueprint digest covers the manifest text and every file it names, in path order. That is what
lets `Run.blueprint` pin the hooks and tool files a run executed, not only its manifest. It also
changes every existing digest once, which is one of the breaking changes.

## What the daemon must store

- **A revision store per owner**, content-addressed: for each revision its content (redacted where
  it holds secrets), number, time, author and reason. Blueprint revisions are already
  content-addressed; approval policies, extensions, MCP servers, providers, config and operator
  mime rules are not today.
- **Observed on-disk edits.** A file edited outside the API becomes a revision when Leviath next
  reads it, with a null author. Edits made and undone between two reads are not seen.
- **Per run:** a snapshot of every file the blueprint names (today a run snapshots only the
  manifest text), the tool definitions the run was offered, the provider revision each attempt
  used, and the approval policy revision at spawn.
- **No secret values, ever.** A secret-bearing revision keeps redacted settings and an HMAC
  digest. From the earlier proposal in this PR: a write that changes only a secret must still
  advance the revision, even though the redacted settings look unchanged.
- **Detect external change before writing.** Also from the earlier proposal: before a config write,
  the server must notice an on-disk change or a read or parse failure since the revision the
  caller read, and refuse rather than overwrite an invalid external edit.

Edits are kept for as long as the revisions they made. How long revisions are kept is not yet
specified.

## Open questions

1. **Coverage.** The earlier proposal in this PR reports how complete a history is
   (`historyCoverage`: complete, partial, not recorded, unknown). This model has no such signal:
   a gap shows only as an edit with a null author. Add one, or document the gap per owner?
2. **Retention.** Should revisions be pruned, and if so, may a revision a retained run still
   points at ever go?
3. **Identity across re-purposing.** An extension's id embeds its type, so a file claimed as a
   stage hook and later as a region hook starts a new history. The earlier proposal kept a
   source file's identity across registration.
4. **Built-in tools.** A built-in tool's definition changes with each build, so its revisions need
   a build-scoped digest or they churn on every upgrade.
5. **Models.** `Model` is not versioned, so a price change is not pinned per attempt. Version it,
   or pin the price on the attempt?
6. **Typed revision content.** MCP server, provider, config and operator mime rule revisions hold
   `settings: JSON`. Typed revisions would let a client diff them without knowing the file format.
7. **Old runs.** A run recorded before snapshots whose blueprint is no longer installed cannot
   satisfy a non-null `Run.blueprint` without a synthesized revision.
