# Rhai custom regions

Leviath's built-in region kinds (`pinned`, `sliding_window`, `temporary`,
`compacting`, `clearable`, `compact_history`, `hashmap`) cover most context
layouts. When they don't, `kind = "custom"` hands one region's behavior to a
Rhai script you write: how the region renders into the model's context, what
happens to each incoming entry, and what gets dropped under budget pressure.

Custom regions mix freely with built-in ones - or, as a single region named
`conversation`, own the entire context window (the
[12-Factor Agents](https://github.com/humanlayer/12-factor-agents) "own your
context window" pattern). Message and tool-result routing in Leviath is by
region *name*, so a custom region named `conversation` receives the full typed
history and its script decides what the model sees.

## Declaring one

```toml
[context.regions.brain]
kind = "custom"
script = "context_hooks/brain.rhai"   # required, relative to the agent dir
persistent = false                    # optional, default false
budget = "40%"                        # budgets work exactly as on built-ins
min_tokens = 10000
```

- `script` resolves relative to the directory containing `agent.leviath`, so
  the script travels with the agent (`lev add`, bundles, and the embedded
  defaults all carry `.rhai` files). Absolute paths pass through unchanged.
- `persistent = true` makes the region Pinned-like: never evicted, immune to
  edge `Clear` transforms, counted as fixed budget, `Always` cache hint.
  `persistent = false` (default) behaves like Temporary: stage-specific,
  evicted under pressure (the script gets first say - see `on_overflow`).
- Percentage budgets resolve at spawn against the stage model's context
  window; the script sees the resolved absolute number in `ctx.region.budget`.
- Per-stage layouts (`[stages.<name>.context.regions.<region>]`) may declare
  custom regions too.

The script is read and compile-checked **once, at spawn**. A missing or broken
file is a hard spawn error (and `lev validate` reports it); editing the script
takes effect on the next run.

## Script contract

Three functions, each receiving one `ctx` argument. Rhai passes arguments by
value, so mutating `ctx` in place does nothing - **return** your result.

### `render(ctx)` - required

Runs on every inference (even when the region is empty, so a script can emit
static scaffolding). Shapes the region's contribution to the assembled
request.

`ctx`:

```jsonc
{
  "region":  { "name": "brain", "budget": 80000, "current_tokens": 1234, "entry_count": 7 },
  "entries": [ {
      "content": "...", "tokens": 12, "timestamp": 1710000000, "key": null,
      "kind": "text" | "user_message" | "assistant_turn" | "tool_result",
      // assistant_turn only:
      "tool_calls": [ { "id": "...", "name": "...", "arguments": { ... } } ],
      // tool_result only:
      "tool_call_id": "...", "tool_name": "...", "is_error": false
  } ],
  "stage_name": "plan", "stage_iterations": 2, "model": "claude-...",
  "window": { "total_tokens": 9000, "max_tokens": 200000 }
}
```

Return either a **string** (one system block; empty string = nothing) or a
**map**:

```rhai
fn render(ctx) {
    #{
        system: "one block",            // or an array of strings
        messages: [
            #{ role: "user", content: "plain text" },
            #{ role: "assistant", content: "thinking", tool_calls: [
                #{ id: "c1", name: "shell", arguments: #{ command: "ls" } },
            ] },
            #{ role: "user", tool_results: [
                #{ tool_call_id: "c1", content: "file_a", is_error: false },
            ] },
        ],
    }
}
```

Typed `tool_calls` / `tool_results` messages are built with the same wire
shapes as the built-in `sliding_window` kind, so a Rhai recreation of it is
byte-identical. The assembler's orphan sanitizer still strips unpaired tool
blocks a buggy script emits - you can't produce a provider-invalid request.

The 12-Factor "everything as one XML user message" case:

```rhai
fn render(ctx) {
    let xml = "<context>";
    for entry in ctx.entries {
        xml += `<event kind="${entry.kind}">${entry.content}</event>`;
    }
    xml += "</context>";
    #{ messages: [ #{ role: "user", content: xml } ] }
}
```

### `on_write(ctx)` - optional

Sees every entry headed into the region.
`ctx = { region, entry: { content, kind, tokens } }`.

Return a **string** to replace the content (tokens re-estimated, entry kind
preserved), `true` or `()` to accept unchanged, `false` to drop the entry
(the write still reports success to the writer - note a dropped tool result
may leave a pointer in `conversation` referencing content that no longer
exists; that's your call to make).

### `on_overflow(ctx)` - optional

Runs when the region must shrink: at window eviction, and when a single write
overflows the region's budget (the write is retried once after your drops).
`ctx = { region, entries, needed_tokens }`. Return an array of entry indices
to drop:

```rhai
fn on_overflow(ctx) {
    // Keep errors, drop successes.
    let drops = [];
    for (entry, i) in ctx.entries {
        if !entry.content.contains("ERROR") { drops.push(i); }
    }
    drops
}
```

Absent hook = oldest-first (Temporary semantics). If your drops don't free
enough, oldest-first eviction makes up the difference.

## Failure semantics

Load-time problems fail fast; runtime problems never break an inference:

| Failure | Behavior |
|---|---|
| Script file missing / doesn't compile / no `fn render(ctx)` | **Hard spawn error** (also caught by `lev validate`) |
| `render` errors or returns an invalid shape | Warning + the region renders as a plain `[name]:` block |
| `on_write` errors or returns an invalid type | Warning + entry accepted unchanged |
| `on_overflow` errors or returns invalid indices | Warning + oldest-first eviction |
| Rendered output exceeds the region's budget | Warning only - sent anyway (enable `[limits] exact_token_counting` for a hard guard) |

Hooks run in the standard Leviath Rhai sandbox: no filesystem, no network, no
`eval`/`import`, operation-bounded. They are pure data transforms.

## Recreating the built-ins

The point of the escape hatch is that you *could* write the built-ins
yourself:

- **pinned** - `persistent = true`, render joins entries. Exact.
- **temporary / clearable** - defaults + a `[name]:`-style render. Exact.
- **sliding_window** - `on_overflow` implementing your retention window,
  render emitting typed messages. Exact (this is why typed message emission
  exists).
- **compacting** - approximable with deterministic condensing in
  `on_overflow`; the LLM summarization lane is not script-accessible.
- **hashmap** - not recreatable yet: keyed upsert is built-in-only
  (`on_write` can't modify existing entries).

## Previewing

`lev test` assembles with your hooks active and real stage metadata, so you
can see exactly what the provider would receive before a live run.
`lev validate` checks the script parses and defines `render`.

## Known limits

- Stage-instruction injection targets the first `pinned` region, never a
  custom one; file tracking (`[context.file_tracking]`) requires a `hashmap`
  region.
- The per-render cache hint is fixed by `persistent` (Always vs UntilChanged);
  render can't override it per call.
- Reordering or reshaping content across inferences can reduce provider
  prompt-cache hits - the script owns that tradeoff.
