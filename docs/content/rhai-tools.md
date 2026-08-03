---
title: Rhai tools
group: Reference
group_order: 3
order: 10
---

# Global tools and policy rules

Every `.rhai` file in `~/.leviath/tools/` is compiled at spawn and offered as an executable tool to
**every** agent. This is how you give all agents a shared capability without editing each blueprint.
Per-agent tools live in that agent's own `tools/` directory and are validated by
`lev validate <agent>`; a per-agent tool of the same name shadows the global one.

> [!WARNING]
> The directory is `~/.leviath/tools/`, inside Leviath's data root next to `providers/` and
> `agents/`. It is not `$HOME/tools/`. Every `.rhai` file here becomes a tool for every agent, so
> treat it as a trusted location.

## Declaring a tool

A tool declares itself with leading `// @` directives and reads its arguments from the `params`
object. The recognized directives:

- `// @tool <name>` is required and names the tool (must match a stage's `available_tools` entry).
- `// @description <text>` is an optional one-liner shown to the model.
- `// @param <name> <type> <required|optional> "<description>"` is repeatable. `<type>` is a JSON
  schema type: `string`, `integer`, `number`, `boolean`, `array`, `object`. A typo here produces a
  schema that does not compile, which switches off
  [argument validation](/docs/tools#argument-validation) for the tool (the daemon logs a warning);
  calls still run, just unchecked.
- `// @requires <cap> [<cap>...]` lists platform capabilities the tool needs (`network`, `shell`,
  `filesystem`), comma or space separated and repeatable. Leviath drops the tool where the platform
  cannot provide one.

The script's return value becomes the tool result: a string is returned verbatim, anything else is
JSON-encoded, and a bare `()` is an empty string. A missing optional param reads as `()`.

## Host functions inside a tool

Tools get a different host surface than providers, because they act on behalf of a running agent:
`http_get(url [, headers])`, `http_post(url, body [, headers])`, `shell(cmd)`, `read_file(path)`,
`write_file(path, content)`, and `env_var(name)` do the I/O (each gated by the tool's `@requires` and
the agent's permissions). The pure helpers are `parse_json`, `to_json`, `encode_uri`, and
`html_to_text`. Shared string/content helpers are also available: `contains`, `starts_with`,
`ends_with`, `trim`, `join`, `split`, `count_tokens`, `is_json`, `is_markdown`, `is_mermaid`,
`is_empty`, `content_format`.

## A complete tool

A minimal transform tool, `~/.leviath/tools/upper.rhai`:

```rhai
// @tool upper
// @description Upper-case text
// @param text string required "input to transform"
params.text.to_upper()
```

A tool that does real I/O, `~/.leviath/tools/web_fetch.rhai`. It declares the `network` capability,
fetches a URL, and hands the model readable prose instead of raw HTML:

```rhai
// @tool web_fetch
// @description Fetch a URL and return its readable text
// @param url string required "the URL to fetch"
// @requires network
let body = http_get(params.url);
html_to_text(body)
```

For parameter shapes that directives cannot express (enums, array `items`, numeric bounds), drop a
sibling `tool.toml` next to the script. When present it overrides the annotations entirely:

```toml
# ~/.leviath/tools/export.toml   (beside export.rhai)
[tool]
name        = "export"
description = "Export in a chosen format"
requires    = ["filesystem"]

[[tool.params]]
name     = "format"
required = true
schema   = { type = "string", enum = ["json", "yaml"], description = "output format" }
```

## Inspecting the inventory

`lev tools` lists the global inventory without starting the daemon. Compiled tools are marked,
files that failed to compile are shown with their reason (they are simply not advertised), and a tool
whose `@requires` capability the platform cannot satisfy is flagged unavailable:

```bash
lev tools           # human-readable inventory, params, requires, and skipped files
lev tools --json    # machine-readable, including param schemas and required capabilities
```

See [Tools](/docs/tools) for how a stage's `available_tools` and `tool_permissions` gate which tools
an agent may actually call.

## Policy rules

The [taint-gate](/docs/security) blocks any exfiltration-capable tool whose data taint exceeds its
clearance. Beyond the static allowlist in `policy.toml`, you can relax the gate with scripted rules:
`.rhai` files in the `leviath/rules/` directory under your OS config dir - `~/.config/leviath/rules/`
on Linux, `~/Library/Application Support/leviath/rules/` on macOS. They are consulted **after** the
static allowlist, and the first script that allows a call wins. The filename stem becomes the rule
name in decisions.

Each rule receives a `context` map with `tool`, `target`, and `taint_level` (a string: `"public"`,
`"internal"`, or `"private"`), and evaluates to a boolean. `true` allows the call.

```rhai
// <config dir>/leviath/rules/company.rhai
context.tool == "send_email"
    && context.target == "ops@corp"
    && context.taint_level == "internal"
```

A script that errors or does not evaluate to a boolean is treated as no match, so a broken rule can
never accidentally open the gate. Inspect and dry-run rules with the CLI:

```bash
lev policy list                                              # static + scripted rules
lev policy test send_email --target ops@corp --taint internal
```

> [!IMPORTANT]
> Scripted rules only ever **allow** calls the gate would otherwise block. They cannot tighten the
> gate or override a deny. See [Security](/docs/security) for the taint model and the full gate
> decision flow.
