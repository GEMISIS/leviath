---
title: Dashboard
group: Guides
order: 5
---

# Dashboard (`lev dash`)

`lev dash` is a full TUI for managing concurrent agents.

- **Agent table** — blueprint/title, stage index, status, tokens in/out, context-window occupancy,
  iteration, elapsed time, model, and sub-agent depth. Auto-generated run titles.
- **Detail view** — per-stage tabs or a graph view of the workflow, a context-window visualization,
  and content panes for **Output**, **Logs**, and **Context** (JSON). Markdown is rendered.
- **Interactions** — answer an agent's question (free-text, edit, multiple-choice, tool-approval, or
  confirm) or send it a mid-run message.
- **Mouse support** — wheel scroll, click-drag select with copy-on-release, OSC52 copy over SSH,
  `y` to yank a pane, Shift+drag for native selection.
- **`m`** opens the MCP management screen without leaving the dashboard.

Common keys: `1`–`9` select · Enter/Esc open/close detail · `/` search · `l`/`o`/`c` switch
Logs/Output/Context · `i` respond or message · `k` kill · `?` help.

Prefer a browser? The [agent console](/app) mirrors the dashboard over the [HTTP API](/docs/api).
