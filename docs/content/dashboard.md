---
title: Dashboard
group: Guides
group_order: 4
order: 1
---

# Dashboard (`lev dash`)

`lev dash` is a full-screen TUI for managing concurrent agents. It's the fastest way to watch a fleet
of runs, answer their questions, and steer them.

```bash
lev dash
```

It reads the same [daemon](/docs/daemon) the browser [console](/app) does, just over the local
control socket instead of HTTP:

```mermaid
flowchart LR
  DASH["lev dash (TUI)"] -->|control socket| D["Daemon"]
  CONSOLE["Browser console"] -->|"HTTP + WS"| SERVE["lev serve"] --> D
  D --> AG["live agent state"]
```

## What's on screen

- **Agent table**: blueprint/title, stage index, status, tokens in/out, context-window occupancy,
  iteration, elapsed time, model, and sub-agent depth. Titles are auto-generated per run.
- **Detail view**: per-stage tabs or a graph view of the workflow, a context-window visualization,
  and content panes for **Output**, **Logs**, and **Context** (JSON). Markdown is rendered.
- **Interactions**: answer an agent's question (free-text, edit, multiple-choice, tool-approval, or
  confirm) or send it a mid-run message.
- **Mouse support**: wheel scroll, click-drag select with copy-on-release, OSC52 copy over SSH,
  `y` to yank a pane, Shift+drag for native selection.
- **`m`** opens the MCP management screen without leaving the dashboard.

## Keys

The dashboard has two screens, and most keys only work on one of them. Press `?` on either for the
same list in the app.

### Main list

| Key | Action |
|---|---|
| `↑` / `↓` | Select an agent. Active runs sort first |
| `Enter` | Open detail view |
| `/` | Filter agents by name or status |
| `c` / `k` | Cancel the selected run |
| `p` / `r` | Pause / resume the selected run |
| `d` | Delete the run. Permanent, and asks `y` to confirm |
| `m` | Manage MCP servers |
| `Esc` | Clear the filter, or quit |

### Detail view

| Key | Action |
|---|---|
| `←` / `→` | Switch stage tab |
| `1`–`9` | Jump to that stage tab |
| `↑` / `↓` | Scroll the pane |
| `b` / `e` | Jump to the beginning / end |
| `l` / `o` / `c` | Switch the pane to Logs / Output / Context |
| `,` / `.` | Step back and forward through context history |
| `/` , then `n` / `N` | Search, then next / previous match |
| `y` | Copy the pane to the clipboard |
| `i` | Respond to or message the agent |
| `k` | Kill the run |
| `p` / `r` | Pause / resume the run |
| `Esc` | Clear the search, or go back to the list |

While you are typing a response, `Enter` sends it, `Alt+Enter` inserts a newline, and `Esc` cancels.

> [!WARNING]
> `c` does two different things depending on where you are. In the main list it cancels the run. In
> detail view it switches the pane to Context. `k` kills the run on both screens.

> [!TIP]
> Prefer a browser, or want to drive Leviath from another machine? The [agent console](/app)
> mirrors the dashboard over the [HTTP API](/docs/api).
