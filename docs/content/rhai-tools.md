---
title: Rhai tools & policy rules
description: Declare new agent tools in Rhai, and write policy rules deciding whether a tool call may fire.
group: Reference
group_order: 3
order: 10
---

# Rhai tools and policy rules

This page covers two things that both live in Rhai scripts. [Writing a tool](#declaring-a-tool)
gives your agents a new capability. [Policy rules](#policy-rules) decide whether a tool call is
allowed to fire. They are unrelated jobs, so read whichever half you came for.

Every `.rhai` file in `~/.leviath/tools/` is compiled at spawn and offered as a tool to **every**
agent. That is how you give all your agents a shared capability without editing each blueprint.

Per-agent tools live in that agent's own `tools/` directory instead, and are checked by
`lev validate <agent>`. A per-agent tool with the same name shadows the global one.

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
  calls still run, with no argument check.
- `// @requires <cap> [<cap>...]` lists platform capabilities the tool needs (`network`, `shell`,
  `filesystem`), comma or space separated and repeatable. Leviath drops the tool where the platform
  cannot provide one.

The script's return value becomes the tool result: a string is returned verbatim, anything else is
JSON-encoded, and a bare `()` is an empty string. A missing optional param reads as `()`.

## Host functions inside a tool

A tool gets a wider host surface than a provider script, because it acts on behalf of a running
agent. The functions come in two kinds.

**These reach the outside world**, and each one is gated per function by
[`[tool_script_permissions]`](/docs/configuration#tool_script_permissions), resolved at spawn. A
tool's `@requires` line is not a gate: it only filters which platforms discover the tool at all.

| Function | Does |
|---|---|
| `http_get(url [, headers])` | An HTTP GET |
| `http_post(url, body [, headers])` | An HTTP POST |
| `shell(cmd)` | Runs a shell command |
| `read_file(path)` | Reads a file, always confined to the workdir |
| `write_file(path, content)` | Writes a file |
| `env_var(name)` | Reads an environment variable. Credential-shaped names need [`allow_env_vars`](/docs/configuration#security) |

**These are pure** and need no permission, because they only transform values you already have:

| Group | Functions |
|---|---|
| JSON and encoding | `parse_json`, `to_json`, `encode_uri`, `encode_base64`, `decode_base64`, `html_to_text` |
| Strings | `contains`, `starts_with`, `ends_with`, `trim`, `join`, `split` |
| Content | `count_tokens`, `is_json`, `is_markdown`, `is_mermaid`, `is_empty`, `content_format` |

`decode_base64` fails rather than returning something wrong, in two ways worth telling apart. Input
that is not valid base64 says so. Input that is valid base64 but decodes to bytes that are not UTF-8
says *that* - base64 carries any bytes, a Rhai string holds text, so a script decoding an image has
asked for something the function cannot return. Both reach the model as an `[error]` line naming
your tool, so a script that hits one stops rather than carrying on with an empty string.

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
sibling `.toml` named after the script (`export.toml` beside `export.rhai`). When present it
overrides the annotations entirely:

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
files that failed to compile are shown with their reason (they are not advertised at all), and a tool
whose `@requires` capability the platform cannot satisfy is flagged unavailable:

```bash
lev tools           # human-readable inventory, params, requires, and skipped files
lev tools --json    # machine-readable, including param schemas and required capabilities
```

See [Tools](/docs/tools) for how a stage's `available_tools` and `tool_permissions` gate which tools
an agent may actually call.

## Policy rules

The [taint gate](/docs/security) blocks any tool that could send data off the machine when that data
is more sensitive than the tool is cleared for. Sometimes a specific case is fine and you want to say
so.

`policy.toml` handles the simple cases with a static allowlist. For anything that needs a decision
rather than a list, write a rule as a `.rhai` file in the `leviath/rules/` directory under your OS
config dir. That is `~/.config/leviath/rules/` on Linux and
`~/Library/Application Support/leviath/rules/` on macOS.

Rules are consulted after the static allowlist, and the first script that allows a call wins. The
filename becomes the rule's name in any decision it makes.

Each rule receives a `context` map with `tool`, `target`, and `taint_level` (a string: `"public"`,
`"internal"`, or `"private"`), and evaluates to a boolean. `true` allows the call.

```rhai
// <config dir>/leviath/rules/company.rhai
context.tool == "send_email"
    && context.target == "ops@corp"
    && context.taint_level == "internal"
```

Rules are re-read when they change. Add a file, edit one, or delete it, and the next run is gated
against what the directory holds now, with no daemon restart. `policy.toml` beside it reloads the
same way. A run already going keeps the rules it started under.

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
