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
| `lev run <agent> --workdir <dir>` | Run the agent over a different directory (default: where you ran the command) |
| `lev create <name>` | Scaffold a new [`agent.leviath` blueprint](/docs/agents) |
| `lev validate <path>` | Check a blueprint's graph, seeds, and permissions |
| `lev test <path>` | Dry-run a blueprint |
| `lev models` | List the models available from your configured [providers](/docs/providers) |
| `lev agent-client …` | Serve an agent over the [Agent Client Protocol](/docs/agent-client-protocol) |

## Blueprints and packaging

| Command | Purpose |
|---|---|
| `lev list` | List installed blueprints and agents |
| `lev add <package>` | Install a blueprint from a directory or `.leviath-bundle` |
| `lev remove <name>` | Remove an installed blueprint |
| `lev pack [path]` | Bundle a project for [sharing](/docs/packaging) |

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
| `lev daemon [status\|start\|stop\|restart]` | Run / inspect / control the [shared-world daemon](/docs/daemon) |
| `lev daemon [install\|uninstall]` | Manage it as a launchd / systemd service |
| `lev serve …` | Expose the [HTTP + WebSocket API](/docs/api) |

## Configuration and tools

| Command | Purpose |
|---|---|
| `lev setup` | Interactive [provider](/docs/providers) setup wizard |
| `lev auth [status\|migrate]` | Move keys between `config.toml` and the [OS keychain](/docs/providers) |
| `lev mcp [add\|list\|login\|test\|remove]` | Manage [MCP tool servers](/docs/mcp) |
| `lev tools` | List and validate your global [Rhai tool scripts](/docs/scripting) |
| `lev policy [list\|add\|test]` | Manage [taint policy](/docs/security) rules |

> [!TIP]
> Two flags worth knowing on `lev serve`: `--allow-admin` (mounts the config-write and MCP-write
> routes) and `--cors https://leviath.dev` (lets the browser [console](/app) reach it). See the
> [API](/docs/api) for the full security model.
