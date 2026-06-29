<p align="center">
  <h1 align="center">Leviath</h1>
  <p align="center">
    <strong>Hardware-inspired context management for LLM agents</strong>
  </p>
  <p align="center">
    <a href="https://github.com/GEMISIS/leviath/actions"><img src="https://github.com/GEMISIS/leviath/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
    <a href="https://github.com/GEMISIS/leviath/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License: MIT"></a>
    <a href="https://leviath.dev"><img src="https://img.shields.io/badge/docs-leviath.dev-8b5cf6" alt="Docs"></a>
  </p>
</p>

---

Every AI agent framework manages context the same way: a flat message array that gets randomly truncated or summarized when it fills up. After ~20 tool calls, your agent has forgotten half of what it read.

Leviath fixes this with **typed memory regions** — inspired by CPU cache hierarchies, not chat logs. Architecture docs stay pinned. Tool results evict first. Conversation history auto-compacts into summaries. Your agent keeps its understanding of what it's building, even 100+ iterations in.

```
┌─────────────────────── Context Window ───────────────────────┐
│ 🔒 Pinned           │ Architecture, objectives │ NEVER evicted │
│ 📜 SlidingWindow     │ Recent conversation      │ NEVER reduced │
│ 📦 Compacting        │ Implementation history   │ LLM-summarized│
│ 📎 Temporary         │ File contents            │ Oldest first  │
│ 🧹 Clearable         │ Scratch/tool output      │ Wiped first   │
│ 🗂️ CompactHistory    │ Stored summaries         │ Accumulates   │
└──────────────────────────────────────────────────────────────┘
```

## Quick Start

```bash
# Install
cargo install --path crates/leviath-cli

# Set up API keys
lev setup

# Create and run an agent
lev create my-agent
cd my-agent
lev run --task "Build a CLI that converts CSV to JSON"

# Manage running agents
lev dash
```

Or use a pre-built agent:

```bash
lev run agents/software-engineer --task "Add error handling to the API module"
lev run agents/researcher --task "Compare spiking neural networks vs transformers"
```

No Rust code needed — agents are defined in a single TOML file.

## What Else Is Different

**🎮 ECS Agent Runtime** — Leviath runs agents as entities in a [bevy_ecs](https://bevyengine.org/) world, not as separate OS processes. Spin up 50 agents and they share one process with game-engine-style scheduling. Other tools spawn a process per agent — 50 Claude Code instances means 50 node processes fighting for your CPU and RAM.

**🗣️ Sub-Agents That Talk to Users** — Other tools have sub-agents (Claude Code, Codex), but they're fire-and-forget: do work, return a summary. In Leviath, a sub-agent at any depth can independently pause and ask the user a question through the dashboard — no routing through the parent. The human stays in the loop at every level.

**🔀 Multi-Stage Agents** — Each stage gets its own model, tool permissions, context layout, and interaction mode. Use Sonnet for analysis (fast/cheap), Sonnet for implementation (workhorse), Opus for review (catches what Sonnet missed). Per-stage context layouts mean your review stage doesn't inherit the implementation stage's scratch data.

## Why Leviath?

<table>
<tr>
<td><strong>Flat context (everyone else)</strong></td>
<td><strong>Structured regions (Leviath)</strong></td>
</tr>
<tr>
<td>

```
[system prompt          ]
[message 1              ]
[message 2              ]
[tool result (huge)     ]  ← pushes out
[message 3              ]     important
[tool result            ]     context
[message 4              ]
[...truncated or        ]
[   randomly summarized ]
```

</td>
<td>

```
🔒 [architecture: always here  ]
📜 [conversation: last 15 turns]
📦 [impl history: auto-compacts]
📎 [files: evicts oldest first ]
🧹 [scratch: wipes when full   ]
🗂️ [summaries: accumulates     ]
```

</td>
</tr>
</table>

When context fills up, eviction follows a deterministic cascade:

**Clearable** (wipe scratch) → **Temporary** (evict oldest files) → **Compacting** (LLM-summarize to history) → **Error** (nothing left to free)

Pinned and SlidingWindow regions are **never** touched. Your agent's core understanding survives.

## How It Works

### Define an Agent

```toml
# agent.leviath
[agent]
name = "my-agent"
version = "0.1.0"

# Stages execute sequentially, each with its own model and tools
[stages.analyze]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-5" }
available_tools = ["read_file", "list_dir", "search"]

[stages.implement]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-5" }
available_tools = ["read_file", "write_file", "edit_file", "bash"]

[stages.review]
mode = "interactive"  # Pauses for user approval
model = { provider = "anthropic", model = "claude-opus-4" }

# Context regions — the memory architecture
[context.regions]
architecture = { kind = "pinned", max_tokens = 4000 }
codebase     = { kind = "temporary", max_tokens = 30000 }
conversation = { kind = "sliding_window", max_items = 15, max_tokens = 15000 }
impl_history = { kind = "compacting", threshold_tokens = 8000, max_tokens = 15000 }
history      = { kind = "compact_history", source_region = "impl_history", max_tokens = 10000 }
scratch      = { kind = "clearable", max_tokens = 10000 }
```

### Route Tool Results

Control where tool outputs land in memory:

```toml
[stages.implement.tool_routing]
default_region = "scratch"
max_result_tokens = 5000

[stages.implement.tool_routing.overrides]
read_file = "codebase"    # File contents → temporary region (evicts oldest)
search = "codebase"       # Search results → same region
bash = "scratch"          # Shell output → clearable (wipes first)
```

### Spawn Sub-Agents

Agents can spawn child agents with different blueprints:

```toml
[agent]
max_child_depth = 3  # How deep the sub-agent tree can go

[stages.analyze]
requires_children = true  # Don't advance until children complete
available_tools = ["spawn_agent", "check_agent", "wait_for_agent", "send_to_agent", "kill_agent"]
```

Sub-agents are just agents — they get their own context window, their own stages, and appear in the dashboard tree:

```
● coder-01          ACTIVE    implement   iter 12
  ├─ researcher-01  COMPLETE
  ├─ researcher-02  ACTIVE    analyze     iter 3
  └─ researcher-03  ◆WAITING              (input needed)
```

## Stage Modes

| Mode | Behavior |
|------|----------|
| `autonomous` | Runs without user input until complete |
| `interactive` | Pauses after each inference for user input |
| `interactive_points` | Runs autonomously but pauses at named checkpoints |

```toml
[stages.implement]
mode = "interactive_points"

[[stages.implement.interaction_points]]
name = "design_review"
prompt = "Here's the proposed design. Approve or suggest changes:"
required = true
```

## CLI Reference

| Command | Description |
|---------|-------------|
| `lev create <name>` | Create agent project (`--template software-engineer\|coder\|researcher`) |
| `lev run [path] --task "..."` | Run agent in background (`--model`, `--yolo`, `--max-depth`) |
| `lev dash` | TUI dashboard for managing all agents |
| `lev serve` | REST + WebSocket API server (`--port`, `--host`) |
| `lev pack [path]` | Bundle agent for distribution |
| `lev add <bundle>` | Install agent from bundle |
| `lev remove <name>` | Uninstall agent |
| `lev list` | List installed agents |
| `lev test [path]` | Run agent tests (`--dry-run`) |
| `lev setup` | Configure API keys and providers |
| `lev models` | List available models |

## Dashboard

`lev dash` provides a full terminal UI for managing concurrent agents:

```
┌─ Agents ─────────────────────────────────────────────────────┐
│ ID           │ Stage       │ Status     │ Tokens   │ Iter    │
│ coder-01     │ implement   │ ●ACTIVE    │ 45k/80k  │ 12      │
│ coder-02     │ review      │ ◆WAITING   │ 32k/80k  │ 8       │
│ reviewer-01  │ deep_review │ ●ACTIVE    │ 28k/50k  │ 5       │
├─ Detail ─────────────────────────────────────────────────────┤
│ [coder-02] Waiting for input at: review                      │
│ The implementation looks good. Ready to commit?              │
│ > _                                                          │
├─ Log ────────────────────────────────────────────────────────┤
│ 09:15:32 coder-01 → read_file(src/main.rs)                  │
│ 09:15:35 coder-02 → Waiting for user input                  │
└──────────────────────────────────────────────────────────────┘
 [↑↓]select  [Enter]respond  [c]ancel  [k]ill  [d]elete  [q]uit
```

Features: stage tabs, context window visualization, markdown rendering, search/filter, clipboard yank, mouse support.

## API Server

`lev serve` exposes a REST + WebSocket API for programmatic control:

```bash
lev serve --port 3000
```

**REST endpoints:**

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/blueprints` | List available blueprints |
| `POST` | `/api/blueprints` | Create new blueprint |
| `POST` | `/api/blueprints/validate` | Validate manifest |
| `POST` | `/api/agents` | Spawn agent (with metadata, webhook callback) |
| `GET` | `/api/agents` | List agents (filter by status) |
| `GET` | `/api/agents/tree` | Agent hierarchy with cumulative tokens |
| `GET` | `/api/agents/:id/context` | Context window snapshot |
| `GET` | `/api/agents/:id/result` | Final output of completed agent |
| `DELETE` | `/api/agents/:id` | Kill agent (cascades to children) |

**WebSocket:** Connect to `/ws` for real-time events (status changes, context updates, logs, interaction prompts).

Spawn with a webhook to get notified on completion:

```bash
curl -X POST http://localhost:3000/api/agents \
  -H "Content-Type: application/json" \
  -d '{
    "blueprint": "coder",
    "task": "Build a REST API",
    "callback_url": "https://your-server.com/hook",
    "metadata": { "project_id": "abc123" }
  }'
```

## Providers

| Provider | Models | API Key |
|----------|--------|---------|
| Anthropic | Claude Opus, Sonnet, Haiku | `ANTHROPIC_API_KEY` |
| OpenAI | GPT-4o, o1, o3 | `OPENAI_API_KEY` |
| OpenRouter | Any model | `OPENROUTER_API_KEY` |
| Ollama | Local models | None (localhost) |
| Claude Code | Via `claude` CLI | None (subscription) |

**Key priority:** `~/.leviath/config.toml` (chmod 600) > `.env` in project dir > environment variables.

### Using Claude Code (No API Key)

```toml
[stages.main]
model = { provider = "claude-code", model = "claude-sonnet-4-5" }
```

> Claude Code shells out to the `claude` CLI — no prompt caching, tool execution handled by Claude Code, not ideal for many concurrent agents. Best for prototyping without API costs.

## Pre-built Agents

| Agent | Stages | Best For |
|-------|--------|----------|
| **software-engineer** | plan → implement → review | Full coding workflow (default template) |
| **coder** | analyze → implement → review | Focused implementation tasks |
| **reviewer** | scan → deep_review → report | Code review and audit |
| **researcher** | gather → analyze → summarize | Research and synthesis |

```bash
lev run agents/coder --task "Refactor the auth module"
```

## Testing

```toml
# tests/basic.toml
[[test]]
name = "greeting"
input = "Say hello"
expect_contains = "hello"
```

```bash
lev test                  # Run all (requires API key)
lev test --dry-run        # Validate structure only
```

## Packaging

```bash
lev pack                  # → my-agent-0.1.0.leviath-bundle
lev add agent.leviath-bundle
```

## Architecture

```
leviath-cli          CLI binary (lev)
├── leviath-runtime  ECS engine (bevy_ecs)
│   ├── leviath-core      Regions, layouts, blueprints
│   ├── leviath-providers  Anthropic, OpenAI, OpenRouter, Ollama, Claude Code
│   └── leviath-mcp       MCP tool integration (JSON-RPC)
├── leviath-scripting      Rhai sandbox for custom validators
└── leviath-package        Bundling and registry
```

8 crates, ~18K lines Rust, 185+ tests.

## Development

```bash
git clone https://github.com/GEMISIS/leviath.git
cd leviath
cargo build
cargo test --workspace     # All tests
cargo clippy --workspace   # Zero warnings policy
```

**Git worktrees** for parallel development:

```bash
git worktree add ../leviath-feat-x feat/x
cd ../leviath-feat-x && cargo build  # Own target dir, no interference
```

## License

[MIT](LICENSE)

---

<p align="center">
  <a href="https://leviath.dev">Website</a> ·
  <a href="https://github.com/GEMISIS/leviath">GitHub</a> ·
  <a href="https://github.com/GEMISIS/leviath/issues">Issues</a>
</p>
