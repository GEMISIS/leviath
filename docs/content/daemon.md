---
title: The daemon
group: Start
order: 2
---

# The shared-world daemon

`lev run` doesn't run the agent in your terminal — it hands it to a background **daemon** that
hosts every agent in one shared ECS world. Runs keep going after your terminal closes, and a
dozen agents share a single process instead of a process each.

The daemon starts automatically the first time you need it. You can also run it yourself:

```bash
lev daemon                 # run in the foreground (with logs)
lev daemon status          # is it running?
lev daemon stop
```

## Run it unattended

For an always-on setup, install the daemon under your OS service manager so it starts at login,
restarts if it dies, and reloads interrupted runs on start:

```bash
lev daemon install         # launchd (macOS) / systemd --user (Linux)
lev daemon uninstall
```

## Control surface

The daemon is reached over a local control socket (a Unix socket / Windows pipe, guarded by a
peer-credential check — not a TCP port). The CLI verbs that talk to it:

| Command | Does |
|---|---|
| `lev ps` | List running agents and their status |
| `lev msg <id> <text>` | Send a message to a running agent |
| `lev respond` | Answer a pending `ask_user` question |
| `lev cancel <run-id>` | Cancel a run |
| `lev context <run-id>` | Show a run's context-window history |

To drive the daemon over HTTP instead, run the [API server](/docs/api).
