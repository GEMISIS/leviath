<p align="center">
  <h1 align="center">Leviath</h1>
  <p align="center">
    <strong>A structured agent runtime for LLMs</strong>
  </p>
  <p align="center">
    <a href="https://github.com/GEMISIS/leviath/actions"><img src="https://github.com/GEMISIS/leviath/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
    <a href="https://github.com/GEMISIS/leviath/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License: MIT"></a>
    <a href="https://leviath.dev"><img src="https://img.shields.io/badge/docs-leviath.dev-8b5cf6" alt="Docs"></a>
  </p>
</p>

---

Most agent tools give LLMs a flat message array and hope for the best. Leviath gives them structure — structured memory, multi-stage workflows, and an ECS engine — so agents stay coherent on long tasks, use the right model for each phase, and don't melt your machine when you run a dozen at once.

Define an agent in TOML. Run it. Watch it actually remember what it read 50 tool calls ago.

<!-- TODO: hero image/gif of dashboard with agents running -->

## Quick Start

```bash
# Install (macOS)
brew install leviath

# Or download a binary from GitHub Releases
# Or build from source: cargo install --path crates/leviath-cli
```

```bash
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
lev run agents/deep-researcher --task "How do spiking neural networks compare to transformers?"
lev run agents/daily-briefer --task "What happened in AI research this week?"
```

No Rust code needed — agents are defined in a single TOML file.

## Features

### 🧠 Structured Context Memory

Every other agent framework manages context as a flat message array that gets randomly truncated when full. Leviath replaces this with **typed memory regions**, each with its own lifecycle — inspired by CPU cache hierarchies, not chat logs.

<!-- TODO: screenshot of context window visualization in dashboard -->

When context fills up, eviction follows a **deterministic cascade** — not random truncation:

| Region | Behavior | Eviction Order |
|--------|----------|----------------|
| 🔒 **Pinned** | Architecture, objectives | Never evicted |
| 📜 **SlidingWindow** | Recent conversation | Never reduced |
| 🗂️ **CompactHistory** | Stored summaries | Accumulates |
| 📦 **Compacting** | Implementation history | LLM-summarized → CompactHistory |
| 📎 **Temporary** | File contents, search results | Oldest entries first |
| 🧹 **Clearable** | Scratch, tool output | Wiped first |

Pinned and SlidingWindow regions are **never** touched. Your agent's core understanding survives no matter how many tool calls it makes.

You control where every piece of information lands — and what happens when memory gets tight:

```toml
# Context regions — the memory architecture
[context.regions]
architecture = { kind = "pinned", max_tokens = 4000 }
codebase     = { kind = "temporary", max_tokens = 30000 }
conversation = { kind = "sliding_window", max_items = 15, max_tokens = 15000 }
impl_history = { kind = "compacting", threshold_tokens = 8000, max_tokens = 15000 }
history      = { kind = "compact_history", source_region = "impl_history", max_tokens = 10000 }
scratch      = { kind = "clearable", max_tokens = 10000 }

# Route tool results to specific regions
[stages.implement.tool_routing]
default_region = "scratch"
max_result_tokens = 5000

[stages.implement.tool_routing.overrides]
read_file = "codebase"    # File contents → temporary (evicts oldest)
search = "codebase"       # Search results → same region
bash = "scratch"          # Shell output → clearable (wipes first)
```

### 🔀 Multi-Stage Workflows

Each stage of an agent gets its own model, tool permissions, context layout, and interaction mode. Use a fast model for research, a powerful one for synthesis, and a different one for review — each seeing only the context it needs.

```toml
[stages.gather]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-5" }
available_tools = ["read_file", "list_dir", "web_search"]

[stages.analyze]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-5" }

[stages.review]
mode = "interactive"  # Pauses for user approval
model = { provider = "anthropic", model = "claude-opus-4" }
```

Stages support three interaction modes:

| Mode | Behavior |
|------|----------|
| `autonomous` | Runs without user input until complete |
| `interactive` | Pauses after each inference for user input |
| `interactive_points` | Runs autonomously but pauses at named checkpoints |

```toml
[stages.draft]
mode = "interactive_points"

[[stages.draft.interaction_points]]
name = "outline_review"
prompt = "Here's the proposed outline. Approve or suggest changes:"
required = true
```

### 🎮 ECS Agent Engine

Leviath runs agents as entities in a [bevy_ecs](https://bevyengine.org/) world — the same Entity Component System architecture used by game engines. Agents are entities. Context windows, state, and message inboxes are components. Inference, eviction, and lifecycle management are systems that run in a game-loop tick.

This means 50 agents share one process with engine-style scheduling, instead of 50 separate OS processes fighting for CPU and RAM.

### 🧬 Sub-Agents

Agents can spawn child agents with different blueprints. Sub-agents are just agents — they get their own context window, their own stages, and their own entry in the dashboard.

What makes Leviath's sub-agents different: a sub-agent at any depth can **independently pause and ask the user a question** through the dashboard. No routing through the parent, no fire-and-forget. The human stays in the loop at every level.

```toml
[agent]
max_child_depth = 3  # How deep the sub-agent tree can go

[stages.research]
requires_children = true  # Don't advance until children complete
available_tools = ["spawn_agent", "check_agent", "wait_for_agent", "send_to_agent", "kill_agent"]
```

<!-- TODO: screenshot of dashboard showing agent tree with sub-agents -->

## Performance

<!-- TODO: benchmarks comparing resource usage and context retention -->

*Benchmarks coming soon. We're measuring:*
- *Resource usage: Leviath (ECS) vs process-per-agent at 10/25/50/100 concurrent agents*
- *Context retention: accuracy on architectural questions after 20/50/100 tool calls*
- *Token efficiency: total tokens consumed for equivalent task completion*
- *SWE-bench Lite: resolve rate comparison with structured vs flat context*

## Dashboard

`lev dash` — a full terminal UI for managing concurrent agents:

<!-- TODO: screenshot/gif of dashboard in action -->

Stage tabs, context window visualization, markdown rendering, search/filter, clipboard yank, mouse support.

## API Server

`lev serve` exposes a REST + WebSocket API for programmatic control. Build your own UI, integrate from any language, or orchestrate agents from a custom harness — no Rust or library imports needed, just HTTP.

```bash
lev serve --port 3000
```

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/agents` | Spawn agent (with metadata, webhook callback) |
| `GET` | `/api/agents` | List agents (filter by status) |
| `GET` | `/api/agents/tree` | Agent hierarchy with cumulative tokens |
| `GET` | `/api/agents/:id/context` | Context window snapshot |
| `DELETE` | `/api/agents/:id` | Kill agent (cascades to children) |
| `GET/POST` | `/api/blueprints` | List, create, validate blueprints |
| `GET/POST` | `/api/agents/:id/interaction` | Handle agent input requests |
| `WS` | `/ws` | Real-time event stream |

```bash
# Spawn an agent with a completion webhook
curl -X POST http://localhost:3000/api/agents \
  -H "Content-Type: application/json" \
  -d '{
    "blueprint": "deep-researcher",
    "task": "Analyze the current state of quantum computing",
    "callback_url": "https://your-server.com/hook",
    "metadata": { "project_id": "abc123" }
  }'
```

Integrate from Python, TypeScript, Go, or anything that speaks HTTP — no SDK required.

## CLI Reference

| Command | Description |
|---------|-------------|
| `lev create <name>` | Create agent project (`--template` to pick a starting blueprint) |
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

## Providers

| Provider | Models | API Key |
|----------|--------|---------|
| Anthropic | Claude Opus, Sonnet, Haiku | `ANTHROPIC_API_KEY` |
| OpenAI | GPT-4o, o1, o3 | `OPENAI_API_KEY` |
| OpenRouter | Any model | `OPENROUTER_API_KEY` |
| Ollama | Local models | None (localhost) |
| Claude Code | Via `claude` CLI | None (subscription) |

**Key priority:** `~/.leviath/config.toml` (chmod 600) > `.env` in project dir > environment variables.

<details>
<summary><strong>Using Claude Code (no API key needed)</strong></summary>

```toml
[stages.main]
model = { provider = "claude-code", model = "claude-sonnet-4-5" }
```

Claude Code shells out to the `claude` CLI — no prompt caching, tool execution handled by Claude Code, not ideal for many concurrent agents. Best for prototyping without API costs.
</details>

## Pre-built Agents

| Agent | Stages | Best For |
|-------|--------|----------|
| **software-engineer** | plan → implement → review | Full coding workflow (default template) |
| **coder** | analyze → implement → review | Focused implementation tasks |
| **reviewer** | scan → deep_review → report | Code review and audit |
| **deep-researcher** | gather → analyze → synthesize | Thorough investigation of a single topic |
| **wide-researcher** | survey → compare → summarize | Broad survey across multiple topics |
| **daily-briefer** | collect → prioritize → brief | Morning summaries from multiple sources |
| **writing-assistant** | research → outline → draft → edit | Blog posts, reports, documentation |

```bash
lev run agents/deep-researcher --task "What are the tradeoffs between CRDT and OT for collaborative editing?"
lev run agents/coder --task "Refactor the auth module to use JWT"
lev run agents/writing-assistant --task "Write a blog post about structured context management"
```

## Testing & Packaging

```bash
# Test your agent
lev test                  # Run all (requires API key)
lev test --dry-run        # Validate structure only

# Package for distribution
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

8 crates · ~18K lines Rust · 185+ tests · zero clippy warnings

## Contributing

```bash
git clone https://github.com/GEMISIS/leviath.git
cd leviath
cargo build
cargo test --workspace
cargo clippy --workspace   # Zero warnings policy
```

## License

[MIT](LICENSE)

---

<p align="center">
  <a href="https://leviath.dev">Website</a> ·
  <a href="https://github.com/GEMISIS/leviath">GitHub</a> ·
  <a href="https://github.com/GEMISIS/leviath/issues">Issues</a>
</p>
