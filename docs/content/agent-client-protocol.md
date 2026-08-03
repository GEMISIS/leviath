---
title: Agent Client Protocol
group: Reference
group_order: 3
order: 11
---

# Agent Client Protocol (editor integration)

Editors and orchestrators want to drive an agent themselves rather than have you type at a terminal.
The **Agent Client Protocol** is the common language for that: the host launches the agent as a
child process and talks to it over that process's stdin and stdout.

`lev agent-client` is Leviath speaking it. The host sends protocol messages, and the command turns
those into runs on the shared-world [daemon](/docs/daemon), streaming output back as it happens.

> [!NOTE]
> **Before this page:** [Agent blueprints](/docs/agents).
> **In one line:** the host launches `lev agent-client`, sends prompts as JSON on stdin, and reads
> results as JSON on stdout.

> [!WARNING]
> "ACP" means two unrelated things, so Leviath never writes it unqualified. This is the Agent
> **Client** Protocol, JSON-RPC over stdio. It is not the Agent *Communication* Protocol, a REST
> and SSE API from the BeeAI project.

## What it is

The Agent Client Protocol lets an editor host launch an agent as a subprocess and drive it over the
process's stdin/stdout. Messages are framed as **one compact JSON object per line** (newline-delimited
JSON-RPC 2.0), with a 64 KiB ceiling per frame. The handshake and turn cycle are:

- `initialize`: capability exchange; the protocol version is `1`.
- `session/new`: open a session (carries the working directory).
- `session/prompt`: send a prompt turn; spawns (or, on later prompts, messages) an agent in the daemon.
- `session/update`: notifications streaming the agent's live output back to the host.
- `session/cancel`: cancel the in-flight turn.

The hosts named in the Leviath source are **Zed** and **Gas City**.

## How a host connects

The host launches `lev agent-client` and speaks JSON-RPC over the process's own stdin/stdout. Every
prompt is forwarded to the daemon over its control socket; the daemon's live event stream and each
run's per-stage output are translated into `session/update` notifications until the run finishes or parks.

```mermaid
flowchart LR
  HOST["Editor host<br/>(Zed / Gas City)"]
  HOST -->|"stdin: JSON-RPC requests"| CLI["lev agent-client"]
  CLI -->|"stdout: session/update"| HOST
  CLI -->|"control socket"| DAEMON["Shared-world daemon"]
  DAEMON -->|"WorldEvent stream"| CLI
```

> [!NOTE]
> stdout is reserved for the JSON-RPC channel. All logs and diagnostics go to **stderr** so they
> can't corrupt the protocol stream.

## Starting it

```bash
lev agent-client --agent my-agent
```

With no `--agent`, each session's working directory is searched for an `agent.leviath` blueprint.

Flags (run `lev agent-client --help` for the authoritative list):

| Flag | Purpose |
|---|---|
| `--agent <name-or-path>` | Blueprint to serve: an installed [agent](/docs/agents) name, or a path to one. When omitted, the session's working directory is searched for an `agent.leviath`. |
| `--yolo` | Approve every tool call without prompting. Recommended when the host does not implement `session/request_permission` (e.g. Gas City). |
| `--allow <tool>` | Allow a tool outright. Repeatable. |
| `--max-depth <n>` | Override the blueprint's max sub-agent tree depth. |
| `--no-seed-commands` | Refuse the blueprint's `seed = { command = "..." }` regions, which run a shell command at spawn, before the first inference, and so before any approval prompt. |

## Permission handling

Hosts that implement the client-side methods advertise capabilities at `initialize`, and the agent
surfaces tool approvals as `session/request_permission` requests the host answers. Hosts that send no
capabilities (Gas City sends none) cannot answer such a request, so instead of deadlocking, tool
approvals are surfaced as output and the run **parks**. Use `--yolo` (or scoped `--allow` flags) to run
unattended against such a host.

## Connecting a host

Point the host's agent command at `lev agent-client`, then open a session and prompt it. Output
streams back as the run progresses.

```bash
lev agent-client --agent coder --yolo
```

[Gas City](/docs/gas-city) has a page of its own, with the `city.toml` stanza and the timeouts worth
adjusting.

> [!NOTE]
> Editor integration is a thin front end over the daemon, exactly like `lev run` and `lev serve`. It
> owns no agent world of its own. See the [daemon](/docs/daemon) for what actually hosts the run, and
> the [CLI reference](/docs/cli) for the rest of the `lev` commands.
