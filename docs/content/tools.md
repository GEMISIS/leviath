---
title: Built-in tools
group: Reference
group_order: 3
order: 4
---

# Built-in tools

Every [agent](/docs/agents) advertises a set of tools to its LLM. The runtime ships a fixed catalog
of **built-in tools** (file access, a shell, context memory, human-in-the-loop prompts, review
surfaces, and sub-agent management) that need no configuration to exist. A [stage](/docs/stages)
decides which of them the model may actually call via `available_tools`, and `tool_permissions`
gates each call at `allow` / `ask` / `deny`. For tools beyond this catalog, connect an
[MCP server](/docs/mcp).

## Files

Read and modify files relative to the agent's working directory.

| Tool | Purpose | Arguments |
| --- | --- | --- |
| `read_file` | Read one file's complete contents. | `path` |
| `read_files` | Read several files in one call, separated by path headers. | `paths` (array) |
| `write_file` | Write content to a file, creating parent directories as needed. | `path`, `content` |
| `edit_file` | Replace an exact string that occurs exactly once in a file. | `path`, `old_str`, `new_str` |
| `list_dir` | List a directory's contents. | `path` (optional; defaults to the working root) |

> [!TIP]
> `read_files` is cheaper than repeated `read_file` calls; reach for it after `list_dir` when you
> already know several paths you need.

## Shell

| Tool | Purpose | Arguments |
| --- | --- | --- |
| `shell` | Run a shell command in the working directory using the system shell (bash/zsh on Unix, cmd on Windows). Has a 60-second timeout. | `command` |

`bash` is an accepted alias for `shell`: a stage's `available_tools` may name either, and both
resolve to the same tool advertised to the model.

> [!WARNING]
> `shell` requires the platform's process-spawn capability. On platforms that don't provide it, the
> tool (and its `bash` alias) is filtered out and never advertised.

## Context

Read and write the agent's own [context window](/docs/context): named sections the runtime folds
back into the system prompt on later turns.

| Tool | Purpose | Arguments |
| --- | --- | --- |
| `context_write` | Store or replace a keyed entry in a named section. | `region`, `content`, `key` (optional) |
| `context_append` | Add to a section without replacing existing content. | `region`, `content`, `key` (optional) |
| `context_read` | Read a section, or a specific keyed entry within it. | `region`, `key` (optional) |
| `context_delete` | Remove a specific keyed entry from a section. | `region`, `key` |
| `context_list` | List sections with their token counts and entry counts. | `region` (optional) |

## Human-in-the-loop

Pause the run and hand control to the user. Each of these blocks until the user responds.

| Tool | Purpose | Arguments |
| --- | --- | --- |
| `ask_user_text` | Ask a free-form question and wait for a written answer. | `prompt` |
| `ask_user_choice` | Ask the user to pick one option from a list. | `prompt`, `options` (array, at least two) |
| `ask_user_confirm` | Ask a yes/no question. | `prompt` |

## Review

Present work to the user for approval or direct editing.

| Tool | Purpose | Arguments |
| --- | --- | --- |
| `present_for_review` | Pause and display a markdown document (plan, design, report) for the user to read and approve. | `title`, `markdown` |
| `edit_document` | Show a document in an editable field pre-filled with its current text; the returned text is the user's authoritative edit. | `content`, `prompt` (optional) |

## Sub-agents

Spawn and coordinate [child agents](/docs/sub-agents) from within a run. These are advertised to the
model but executed by the engine's tool registry, since they act on the shared agent world.

| Tool | Purpose | Arguments |
| --- | --- | --- |
| `spawn_agent` | Spawn a sub-agent from a blueprint; returns its ID (blocks and returns the result when `wait` is true). | `blueprint`, `task`, `wait` (default false), `seed_context` (optional), `max_child_depth` (optional) |
| `check_agent` | Non-blocking status check; returns the result if complete. | `agent_id` |
| `wait_for_agent` | Block until a sub-agent completes, then return its final result. | `agent_id` |
| `send_to_agent` | Send a message into a running sub-agent's context. | `agent_id`, `message`, `target_region` (optional; defaults to the conversation) |
| `kill_agent` | Kill a sub-agent and all its descendants. | `agent_id` |

## Web access

Web tools are not built in. They ship as [Rhai script tools](/docs/rhai-tools) beside the agents
that use them, which is why they are editable and why an agent without them cannot reach the
network at all.

| Tool | Purpose | Arguments |
| --- | --- | --- |
| `web_search` | Search the web and return a JSON list of `{title, url, snippet}`. Uses Brave Search when `BRAVE_API_KEY` is set, otherwise a keyless Wikipedia search that works with no configuration. | `query`, `count` (optional, default 5) |
| `web_fetch` | Fetch a URL and return its readable text. HTML is stripped to prose; large pages are truncated, and a blocked or oversized request returns a diagnostic rather than failing the run. | `url` |

Both ship with `researcher`, `deep-researcher`, `wide-researcher`, `daily-briefer`, and
`writing-assistant`. To give another agent web access, copy them into that agent's `tools/`
directory, or drop them in `~/.leviath/tools/` to offer them to every agent.

> [!WARNING]
> Fetches cannot reach loopback, private, or link-local addresses unless you set
> `[security] allow_local_network = true`. That default blocks cloud metadata endpoints, your own
> `lev serve`, and the LAN, because the model chooses the URL out of context an attacker may have
> influenced.

## Granting tools per stage

A tool existing in the catalog does not mean an agent can call it. Two independent settings on each
stage control access:

- **`available_tools`**: the allowlist of tool names advertised to the model in that stage. A tool
  not listed here is invisible to the LLM. Names may use aliases (e.g. `bash` for `shell`).
- **`tool_permissions`**: a per-tool map whose values are `allow`, `ask`, or `deny`. `allow` runs
  the call outright, `ask` requires user approval first, and `deny` blocks it. Stage-level entries
  are narrower than agent-level `[tool_permissions]` and wider than launch-time flags.

```toml
[stages.implement]
available_tools = ["read_file", "read_files", "edit_file", "shell"]

[stages.implement.tool_permissions]
shell     = "ask"      # require approval before running commands
edit_file = "allow"    # apply edits without prompting
```

> [!NOTE]
> `available_tools` and `tool_permissions` are separate gates: a tool must be listed in
> `available_tools` to be offered at all, and its `tool_permissions` value then decides whether a
> call is allowed, prompted, or refused.

## Default permissions

With nothing configured, tools fall back to these:

| Tool | Default |
|---|---|
| `read_file`, `list_dir` | `allow` |
| `write_file`, `edit_file`, `shell` (and its `bash` alias) | `ask` |
| `ask_user_text`, `ask_user_choice`, `ask_user_confirm`, `edit_document` | `allow` |
| `spawn_agent`, `check_agent`, `wait_for_agent`, `send_to_agent`, `kill_agent` | `allow` |
| Everything else, built-in or MCP | `ask` |

Read-only built-ins run freely, mutating ones ask, and human-in-the-loop tools are always allowed
because prompting before a prompt would be circular.

### How a policy is resolved

Narrowest scope wins: **launch flag, then stage, then agent, then `config.toml`, then the built-in
default above.**

Two rules constrain that:

- A blueprint may only **tighten** what your config set. A downloaded agent cannot grant itself
  `shell = "allow"` over your `ask`. To trust one agent with more, name it in
  `[agent_tool_permissions.<agent>]` in your own [config](/docs/configuration#tool-permissions).
- A launch flag (`--allow`, `--yolo`) can turn an `ask` into an `allow`, but it can **never** lift
  a `deny`.

There are four layers in total. `available_tools` is visibility, `tool_permissions` is approval
(alongside the schema check below, which runs before any prompt),
`[tool_script_permissions]` gates what a Rhai script tool may do, and the
[taint gate](/docs/security#taint-tracking-experimental) checks data flow before an
exfiltration-capable call fires.

> [!WARNING]
> A tool set to `ask` in a headless context blocks until someone answers. It never auto-denies. For
> unattended runs, either grant the tools explicitly with `--allow` or use `--yolo`, which cannot
> override a `deny`.

## Argument validation

Before a call is dispatched, its arguments are checked against the exact JSON Schema the tool
advertised to the model. This applies uniformly: built-in tools, Rhai script tools, MCP tools, and
the sub-agent tools all declare schemas, and a call that does not satisfy its schema (a missing
required argument, a number where a string belongs, a value outside a declared enum) is refused
back to the model as an `[error]` result naming the violations. The call never executes and never
reaches a permission prompt, and the refusal does not count as work the agent did; the model reads
the message and corrects itself on the next turn.

A schema that cannot be compiled (a typo'd type in a Rhai `@param` line, an MCP schema fragment the
engine cannot interpret) turns validation off for that tool rather than refusing its calls. The
daemon logs a warning when that happens so the broken schema gets noticed. Schemas with external
`$ref` references fall in the same bucket by design: the validator never fetches anything over the
network or filesystem, so such a schema fails to compile and is skipped.
