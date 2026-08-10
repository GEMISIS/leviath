---
title: Gas City
description: Wiring Leviath into Gas City, the multi-agent orchestration SDK, as the coding-agent backend behind its workflows.
group: Integrations
group_order: 5
order: 2
---

# Running Leviath from Gas City

[Gas City](https://github.com/gastownhall/gascity) is an orchestration-builder SDK for multi-agent
coding workflows, from the [Gas Town Hall](https://gastownhall.ai/) project. It manages the parts
Leviath does not: which projects agents may work in, which agent picks up which piece of work, and
how a fleet of them is supervised.

It talks to agents over the **Agent Client Protocol**, which Leviath speaks. So this is a
configuration change on the Gas City side, not an adapter you have to write.

## The vocabulary

Two Gas City words show up below. A **city** is your workspace directory, holding agents, rigs, and
settings. A **rig** is a project directory registered with a city, which is what lets agents work in
it. Gas City's own [tutorial](https://github.com/gastownhall/gascity/blob/main/docs/tutorials/01-cities-and-rigs.md)
covers both properly.

## Declare Leviath as a provider

Gas City providers usually inherit from a builtin preset, as in `base = "builtin:claude"`. Leviath
is not one of those, so declare it standalone with `base = ""` and give it a command to run:

```toml
# city.toml
[providers.leviath]
base         = ""
command      = "lev"
args         = ["agent-client", "--agent", "coder", "--yolo"]
supports_acp = true
```

`supports_acp` is the part that matters. Per Gas City's
[config reference](https://docs.gascity.com/reference/config), it declares that the binary speaks
JSON-RPC 2.0 over stdio, and an agent may only choose the ACP session transport if its provider sets
this.

Then point an agent at it and ask for that transport:

```toml
[agents.builder]
provider = "leviath"
session  = "acp"
```

With `session` left unset, Gas City uses the city-level session provider, which is normally tmux.
That will not work here. Leviath's `lev agent-client` speaks protocol on stdout, not a terminal UI.

## Why `--yolo` is in there

This is the part that catches people, so it is worth being direct about it.

When an ACP host connects, it advertises what it can do during the `initialize` handshake. A host
that implements `session/request_permission` can be asked to approve a tool call, and Leviath will
ask. Gas City sends no client capabilities, so there is nobody to ask.

Leviath does not deadlock on that. It surfaces the approval as output and keeps the turn in
flight, waiting for an answer from somewhere else: `lev respond` or `lev dash` on the same
machine. Unanswered, the approval is denied after `[limits] interaction_timeout_secs`, an hour by
default. A timeout is never read as consent.

`--yolo` approves every tool call so nothing stops to ask. If that is more trust than you want to
extend, name the tools you are happy with instead:

```toml
args = [
  "agent-client", "--agent", "coder",
  "--allow", "read_file",
  "--allow", "list_dir",
  "--allow", "shell",
]
```

Anything not on the list still parks, so allow enough for the agent to finish its job. Note that
neither flag can lift a `deny` in your config. See [Security](/docs/security).

## Choosing which agent runs

`--agent coder` pins one blueprint for every session on that provider. Drop the flag and Leviath
looks for an `agent.leviath` in the session's working directory instead, which is usually the rig
Gas City is running in. That is the better default when different rigs want different agents.

Declare one provider per agent when you want a Gas City agent per Leviath blueprint:

```toml
[providers.leviath-coder]
base = ""
command = "lev"
args = ["agent-client", "--agent", "coder", "--yolo"]
supports_acp = true

[providers.leviath-reviewer]
base = ""
command = "lev"
args = ["agent-client", "--agent", "reviewer", "--yolo"]
supports_acp = true
```

## Three settings worth adjusting

Gas City's ACP session provider has timeouts that assume a fast local agent. A Leviath run doing
real work is slower than that, mostly because it is waiting on models:

| Setting | Default | Why you might raise it |
|---|---|---|
| `handshake_timeout` | `30s` | Only covers the handshake, so the default is usually fine. Raise it if the daemon has to cold-start first |
| `nudge_busy_timeout` | `60s` | How long Gas City waits for the agent to go idle before sending another prompt. A multi-stage run is busy for much longer than a minute |
| `output_buffer_lines` | `1000` | How much output is kept for `Peek`. A long run produces more than this |

## Checking it works

Before wiring anything up, confirm the command runs on its own:

```bash
lev agent-client --agent coder --yolo
```

It should sit there waiting for JSON-RPC on stdin. That is correct. Diagnostics go to stderr,
because stdout belongs to the protocol.

Then open a session in Gas City and prompt the agent. Output streams back as the run progresses. If
it connects but nothing ever happens, the run is most likely waiting on a tool approval, which
means answering with `lev respond`, or running with `--yolo` or a wider `--allow` list.

## What Leviath does not bring

Gas City keeps doing the things it is for, and Leviath does not duplicate them. Leviath has no view
of your issues, no notion of which rig should be worked on next, and no cross-session memory of what
another agent did. Sub-agents and fan-out are within one Leviath run, not across your fleet.

If the hard problem you have is coordinating work across many repos and issues, that is Gas City's
job and it is better at it. Leviath is worth adding when the hard part is what happens inside a
single unit of that work.
