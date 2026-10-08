# graphql-next: a proposed GraphQL contract for Leviath

**Status: a design proposal.** Nothing here is implemented. `lev serve` answers the published,
generated schema in [`../../schema/leviath.graphql`](../../schema/leviath.graphql), and that stays
authoritative until the resolvers and the derive make this proposal true. The checks below are
static: they prove the schema is well formed, follows its own rules, accounts for every published
coordinate and serves a corpus of client operations. They prove nothing about runtime behaviour.

This directory replaces the earlier proposal that was here. Where that proposal recorded runtime
facts this one must respect, [design-notes.md](design-notes.md) carries them over with credit.

## What is here

| File | What it is |
|---|---|
| [`leviath.graphql`](leviath.graphql) | The proposed schema: 798 named types, every element described |
| [`design-notes.md`](design-notes.md) | The proposal: the problems, the design rules, the model area by area, what breaks, what the runtime needs, open questions |
| [`object-versioning.md`](object-versioning.md) | The history model on its own: what is versioned, what a revision holds, what records pin, edits, `changes(since:)`, what the daemon must store |
| [`MIGRATION.md`](MIGRATION.md) | Every published coordinate (4,109) and where it goes: kept, renamed, reshaped, merged or dropped, with the reason |
| [`operations/`](operations/) | 61 named client operations in eight files: the earlier proposal's 45 journeys ported, and 16 more this schema makes possible |
| [`variables.json`](variables.json) | At least one example per operation (84 in all), with placeholder ids |
| [`validation-notes.md`](validation-notes.md) | What the checks establish, the port of every earlier operation, and what the schema cannot yet express |
| `check_schema.py`, `check_kinds.py`, `check_migration.py`, `validate_operations.py` | The checks, below |
| `requirements.txt` | `graphql-core==3.2.6` |

## Running the checks

From this directory. The only file outside it that they read is the published schema,
`../../schema/leviath.graphql`.

```sh
python -m pip install -r requirements.txt
python check_schema.py          # rules, filter mirrors, listings, mutations, orphans; breaking-change report
python check_kinds.py           # no kind fields, no enums that repeat a type's variants
python check_migration.py       # MIGRATION.md accounts for every published coordinate
python validate_operations.py   # operations validate, examples coerce, @oneOf and page caps hold
```

Each exits non-zero on a failure. `check_schema.py --list-breaking` prints every breaking and
dangerous change instead of the counts. If the published schema has moved on since `MIGRATION.md`
was written, `check_migration.py` says so before listing the new coordinates as missing (see
[validation-notes.md](validation-notes.md), "Baseline").

## Suggested PR description

**Title:** docs: propose a GraphQL contract redesign: types instead of kinds, history on the object

**Body:**

> This replaces the earlier proposal in `docs/design/graphql-next/` with a complete target schema
> for Leviath's GraphQL API, the reasoning behind it, a coordinate-by-coordinate migration table,
> a ported operation corpus and self-contained checks. It is a design proposal: nothing in the
> runtime changes, and the generated schema in `docs/schema/leviath.graphql` stays authoritative
> until the derive regenerates this one.
>
> **The problem.** Reading the published schema cold, the same faults recur: names that describe
> storage or the CLI (`Script`, `YoloProfile`); one thing served as two unlinked nodes (a Rhai
> tool is a `ScriptOutput` and a `ScriptToolOutput` that can disagree); four vocabularies for
> where a tool comes from; states that need a second field to read (`WAITING_INPUT`, `COMPLETE`
> before `flags`); `kind` fields that decide which nullable fields mean anything; relations
> spelled as strings; and facts no query can reach (MCP tools, restart-required).
>
> **The rules.** Plain-English names. One concept, one type. Types instead of `kind` fields, and
> states that carry data as unions with no status enum beside them. Filtering by variant through
> one filter field per variant (`runs(filter: { state: { waitingOnPerson: {} } })`). Relations as
> typed fields. History as revisions on the object, with every record pinning the exact revision
> it used (`Run.blueprint: BlueprintRevision!`, `ToolCall.tool` the `ToolRevision` the model was
> shown). One error rule. One listing shape. Idempotency keys and `expectedRevision` on writes
> agents retry. Everything pollable, including a bounded `awaitRun`. An operating guide served by
> the schema itself.
>
> **What breaks.** Deliberately a breaking release: 705 breaking and 66 dangerous changes against
> the published schema, most of them type renames (`RunOutput` to `Run`), plus `Timestamp` to
> `DateTime` and kind enums to types. 141 renamed fields keep a deprecated alias for one release,
> and `MIGRATION.md` maps all 4,109 published coordinates.
>
> **What the runtime needs.** Mostly reshaping in the GraphQL layer; the new data is a revision
> store, per-run snapshots of offered tool definitions and named files, and a handful of ids and
> timestamps on journal records. The derive needs interface mirrors, a new suffix legend and
> deprecated aliases. `design-notes.md` lists it all, with fifteen open questions for maintainers.
>
> **Checks** (static only; `graphql-core==3.2.6`):
> - `check_schema.py`: builds and validates; 100% descriptions; backtick references resolve;
>   filters mirror outputs with one arm per union or interface variant; one listing and mutation
>   shape; no orphans; breaking-change report.
> - `check_kinds.py`: no `kind`-style fields or variant-repeating enums.
> - `check_migration.py`: every published coordinate has one row.
> - `validate_operations.py`: 61 operations validate, 84 examples coerce, `@oneOf` and page caps
>   hold, and a self-test shows the checks catch bad input.
>
> All pass against the published schema `MIGRATION.md` was written for. The published schema has
> since gained four attempt-outcome coordinates (`finishReason`, `stoppedFor`), which
> `check_migration.py` reports as missing on a current checkout and which the proposal does not
> yet carry. Of the earlier proposal's 45 operations, 40 port fully and 5 in part;
> `validation-notes.md` says what each partial one is missing, which are findings about this
> schema.
