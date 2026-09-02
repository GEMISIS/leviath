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
> `agents/`. It is not `$HOME/tools/`. Every `.rhai` file here becomes a tool for every agent, and
> not every file in it was written by you: once a run uses the `install_tool` built-in, the
> directory holds model-authored code as well. `install_tool` is the audited way in: it compiles
> the script, refuses a colliding name, and starts each file with a
> `// installed by leviath: agent run in <workdir> at <unix seconds>` line naming where it came
> from. It is not the only way in. A shell redirect outside the workdir is
> [refused](/docs/tools#where-a-redirect-may-write), but a program the shell runs (`cp`, `tee`) is
> confined only by a [`[sandbox]`](/docs/security#sandboxes), so an unattended run with `shell` and
> no sandbox can still put a `.rhai` file here; a file without a provenance line was not installed
> through `install_tool`. Every call to any tool here is still gated by the tool policy (`ask` by default,
> waived only by `--yolo`). Audit the directory with `lev tools`, which lists each tool with its
> description and parameters and prints its provenance line under it (or says the file has none),
> and remove a tool by deleting its `.rhai` file (and any sibling `.toml`): the next spawn no
> longer sees it.

## Declaring a tool

A tool declares itself with leading `// @` directives and reads its arguments from the `params`
object. The recognized directives:

- `// @tool <name>` is required and names the tool. A stage sees it when its `available_tools`
  lists that name, or when the stage sets `available_global_tools = true` and the script lives in
  `~/.leviath/tools/`.
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

## Installing a tool from a run

A running agent can add to the global inventory itself with the `install_tool` built-in. It takes
the tool's `name`, the complete `.rhai` `source`, and an optional `overwrite` flag, compiles the
script, and writes it to `~/.leviath/tools/<name>.rhai`. This is the persist path for mechanical
learnings: a step an agent worked out by hand once (a parsing routine, a repeated lookup, a fixed
transformation) becomes a tool every later run can call instead of rediscovering it.

The install is refused, and nothing is written, when the script does not compile, has no
`// @tool` or `// @description`, declares a `// @tool` name that differs from `name`, takes the
name of a built-in, sub-agent or MCP tool (a script under one of those is dropped at discovery, so
it would never run), exceeds 256 KiB, or would replace an existing script without `overwrite`.
A sibling `<name>.toml` that declares a different `[tool] name` is refused too, since the TOML
would win and the tool would appear under the other name. The result the model reads back names
the file, the description, the parameters and the required capabilities.

Three things keep the directory yours:

- Every installed file starts with a `// installed by leviath: agent run in <workdir> at <unix
  seconds>` comment, so `cat` and `lev tools` show which run wrote it. The comment carries no `@`
  directive and compiles as an ordinary comment. Only `install_tool` writes it: a file in the
  directory without one was hand-written, or put there by something that bypassed the install
  (a `cp` from a `shell` call in a run with no `[sandbox]`, for instance).
- `install_tool` is `ask` by default, like `write_file` and `shell`. A blueprint or `config.toml`
  can set `install_tool = "allow"` under `[tool_permissions]`, and `--yolo` waives the prompt for
  an unattended run. See [Security](/docs/security) for what an unattended run can persist.
- The script's own calls are still gated when it runs: an installed tool reads the same
  [`[tool_script_permissions]`](/docs/configuration#tool_script_permissions) as a hand-written one.

A tool is meant for repeatable mechanical steps, never for judgement. A script that encodes a
decision the model should be making each time ages badly and is hard to notice from the outside.

To use an installed tool in the same run, the agent must be a `dynamic_tools` agent, which picks
the new tool up on its next turn. Any later run sees it at spawn, and a stage advertises it when its
`available_tools` names it or it sets `available_global_tools = true`. See [Tools](/docs/tools) for
how a stage's tool set is put together.

## Inspecting the inventory

`lev tools` lists the global inventory without starting the daemon. Compiled tools are marked,
files that failed to compile are shown with their reason (they are not advertised at all), and a tool
whose `@requires` capability the platform cannot satisfy is flagged unavailable. Under each tool it
prints the file's `// installed by leviath: ...` line, or
`no provenance line (hand-written, or written outside install_tool)` when the file has none; the
JSON output carries the same line as `provenance`, `null` when absent:

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
