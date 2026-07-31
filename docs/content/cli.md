---
title: CLI reference
group: Reference
group_order: 3
order: 2
---

# CLI reference (`lev`)

Everything Leviath does is one binary, `lev`. This is a map of the commands the docs reference;
run `lev <command> --help` for the full, authoritative flag list.

## Running agents

| Command | Purpose |
|---|---|
| `lev run <agent> --task "…"` | Start an agent (built-in name or a blueprint path) |
| `lev create <name>` | Scaffold a new [`agent.leviath` blueprint](/docs/agents) |
| `lev validate <path>` | Check a blueprint's graph, seeds, and permissions |
| `lev test <path>` | Dry-run a blueprint |

## Watching and steering

| Command | Purpose |
|---|---|
| `lev dash` | Full-screen TUI [dashboard](/docs/dashboard) |
| `lev ps` | List running agents and their status |
| `lev msg <id> "…"` | Send a message to a running agent |
| `lev respond` | Answer a pending `ask_user` question |
| `lev cancel <run-id>` | Cancel a run |
| `lev context <run-id>` | Show a run's context-window history |

## The daemon and API

| Command | Purpose |
|---|---|
| `lev daemon [status\|stop]` | Run / inspect / stop the [shared-world daemon](/docs/daemon) |
| `lev daemon install` | Install it as a launchd / systemd service |
| `lev serve …` | Expose the [HTTP + WebSocket API](/docs/api) |

## Configuration and tools

| Command | Purpose |
|---|---|
| `lev setup` | Interactive [provider](/docs/providers) setup wizard |
| `lev mcp [add\|list\|login\|test\|remove]` | Manage [MCP tool servers](/docs/mcp) |
| `lev policy [list\|add\|test]` | Manage [taint policy](/docs/security) rules |

> [!TIP]
> Two flags worth knowing on `lev serve`: `--allow-admin` (mounts the config-write and MCP-write
> routes) and `--cors https://leviath.dev` (lets the browser [console](/app) reach it). See the
> [API](/docs/api) for the full security model.
