---
title: Rhai scripting
description: Why Leviath embeds the Rhai scripting language, and what you can extend with it.
group: Reference
group_order: 3
order: 7
---

# Rhai scripting

Eventually you want something Leviath does not ship: a tool for your internal API, a model provider
nobody has added yet, a rule about which calls are allowed. You should not have to fork the project
and rebuild it for that.

So Leviath embeds **Rhai**, a small scripting language that looks a lot like Rust. Drop a `.rhai`
file in the right directory and it becomes part of the runtime, with no recompile and usually no
restart.

Each script stays small, because Leviath keeps the hard parts. Transport, budgets, sandboxing, and
retries stay with the runtime, and the script gets only the one decision that is specific to you.

> [!NOTE]
> **Before this page:** [Agent blueprints](/docs/agents).
> **In one line:** four places you can drop a script, each with a narrow job and its own page.

You do not need to know Rhai to read the pages below. Each one has a complete, working script you
can copy and change. [rhai.rs](https://rhai.rs) has the language reference if you want it.

## The four extension points

| What | Where it goes | What the script decides |
|---|---|---|
| [**Model providers**](/docs/rhai-providers) | `~/.leviath/providers/<name>.rhai` | How to map a request onto some HTTP API, and the response back |
| [**Context regions**](/docs/rhai-regions) | beside the agent, referenced by `script =` | How one region renders, accepts writes, and sheds content under pressure |
| [**Stage hooks**](/docs/rhai-hooks) | beside the agent, referenced by `[stages.<name>.hooks]` | What happens at seven points in an agent's lifecycle - entering a stage, either side of an inference, before a tool call, and at the end |
| [**Global tools**](/docs/rhai-tools) | `~/.leviath/tools/*.rhai`, or an agent's own `tools/` | A new tool, its schema, and what it does |
| [**Policy rules**](/docs/rhai-tools#policy-rules) | `rules/*.rhai` in your OS config dir, see [configuration](/docs/configuration#policytoml) | Whether a given tool call is allowed to fire |

Each page walks its point end to end with a complete, copy-pasteable example.

## The sandbox they all share

Every script runs in the same hardened engine: no `eval`, no `import`, no ambient filesystem or
network access, bounded operations and expression depth, a capped call depth, and `print`/`debug`
muted. The host functions each extension point offers are the only way out of it, and they differ
by point:

- Provider scripts get HTTP, JSON, SSE parsing, and encoding helpers, because mapping an API is
  their whole job.
- Tool scripts get HTTP, shell, file, and environment access, each independently gated by
  [`[tool_script_permissions]`](/docs/configuration#tool_script_permissions).
- Region hooks and policy rules get **nothing**. They are pure data transforms over the `ctx` they
  are handed.

Function and host-API names are matched exactly, so a typo is a hook that never fires rather than
an error. `lev validate` and `lev tools` catch that before a run does.

> [!WARNING]
> A script tool can read environment variables, but a name that looks like a credential is refused
> unless you list it in [`[security] allow_env_vars`](/docs/configuration#security). Without that,
> a two-line tool reading `ANTHROPIC_API_KEY` and POSTing it elsewhere was a working exfiltration
> path with no prompt anywhere in it.

## Hot reload

Provider scripts are recompiled when the file's mtime changes, so an edit takes effect on the next
run with no daemon restart. Region scripts are read and compile-checked once, at spawn, so an edit
applies to the next run rather than an in-flight one.

Neither is scanned or executed until something actually references it, so dropping a file into a
directory does not by itself run it.
