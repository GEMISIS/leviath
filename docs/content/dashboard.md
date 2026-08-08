---
title: Dashboard
description: The `lev dash` terminal UI for watching a fleet of runs, answering their questions, and steering them.
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

It reads the same [daemon](/docs/daemon) that [The Lair](https://leviath.dev/lair), the browser
console, does, just over the local
control socket instead of HTTP:

```mermaid
flowchart LR
  DASH["lev dash (TUI)"] -->|control socket| D["Daemon"]
  CONSOLE["The Lair (browser)"] -->|"HTTP + WS"| SERVE["lev serve"] --> D
  D --> AG["live agent state"]
```

## What's on screen

- **Agent table**: title and run id, blueprint, stage, status, tokens, and start time, with
  sub-agents nested under their parent. Titles are auto-generated per run. The model, iteration,
  and context-window occupancy live in the detail view.
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
| `↑` / `↓` (or `k` / `j`) | Select a run |
| `Home` / `End` (or `g` / `G`) | Jump to the first / last run |
| `Enter` | Open detail view |
| `Tab` | Focus the log panel. Arrows scroll it, `End` resumes tailing, `Esc` comes back |
| `/` | Filter runs by name or status |
| `s` | Cycle the sort: start time (default), recent activity, or status groups |
| `x` | Kill the selected run. Asks first |
| `d` | Delete the run. Permanent, and asks first |
| `p` / `r` | Pause / resume the selected run |
| `m` | Manage MCP servers |
| `Esc` | Clear the filter |
| `q` / `Ctrl-C` | Quit |

By default runs are listed newest first and keep their row for their whole life, so nothing jumps
around when a run finishes. The sort indicator sits in the table's top-right corner.

### Detail view

| Key | Action |
|---|---|
| `←` / `→` | Switch stage tab |
| `1`–`9` | Jump to that stage tab |
| `↑` / `↓` (or `k` / `j`) | Scroll the pane; in the Context view, move the tree cursor |
| `Home` / `End` (or `b` / `e`) | Jump to the beginning / end |
| `l` / `o` / `c` | Switch the pane to Logs / Output / Context |
| `g` | Open the stage explorer (graph agents) |
| `Enter` / Space | Fold or unfold the row under the Context tree's cursor |
| `[` / `]` | Jump to the previous / next region in the Context view |
| `,` / `.` | Step back and forward through context history |
| `/` , then `n` / `N` | Search, then next / previous match |
| `y` | Copy the pane to the clipboard |
| `i` | Respond to or message the agent |
| `x` | Kill the run. Asks first |
| `p` / `r` | Pause / resume the run |
| `Esc` | Clear the search, or go back to the list |

While you are typing a response, `Enter` sends it, `Alt+Enter` inserts a newline, and `Esc` cancels.

Destructive keys always confirm on a dialog with real buttons: `←`/`→` pick an answer, `Enter`
activates it, and a stray keypress does nothing. The safe answer holds focus to start.

### Context view

The Context view is a tree, not one long scroll. Each region is a header row with its token bar;
its entries are one-line stubs with a preview. Move with `↑`/`↓`, fold or unfold with `Enter` or
Space, and jump between regions with `[` and `]`. While a search (`/`) is active everything is
temporarily unfolded so matches inside entries stay reachable. Browsing history with `,`/`.` keeps
your scroll position and fold state, and the context card's title shows which archived point you
are on, in which stage, recorded when.

### Stage explorer

For a graph agent, `g` in the detail view opens the full-screen stage explorer:

- **Graph** lays the stages out on layers (parallel branches share a row), with every transition
  listed under its source stage. Revisit loops are marked with `↺` and drawn dashed. Visited stages
  show a visit count (`×2`) and the time of their last visit; unvisited ones are dimmed, and `u`
  hides them.
- **Timeline** lists each actual visit in order, with when it started, how long it lasted, and how
  many iterations it ran. `Enter` on a visit opens the context window exactly as it was at that
  point.

`Tab` switches between the two, and `Esc` or `g` closes the explorer.

> [!TIP]
> Prefer a browser, or want to drive Leviath from another machine?
> [The Lair](https://leviath.dev/lair) mirrors the dashboard over the [HTTP API](/docs/api).
