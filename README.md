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

Pick a pre-built agent or create your own. Run it. Watch it actually remember what it read 50 tool calls ago.

<!-- TODO: hero gif of dashboard with agents running -->

## Quick Start

### Install

```bash
# macOS
brew install gemisis/tap/leviath

# Linux
curl -fsSL https://github.com/GEMISIS/leviath/releases/latest/download/leviath-linux-x64.tar.gz | tar xz -C /usr/local/bin

# Windows — download from GitHub Releases
# https://github.com/GEMISIS/leviath/releases

# Build from source (any platform, requires Rust)
cargo install --path crates/leviath-cli
```

### Run

```bash
lev setup                  # Configure your API key (Anthropic, OpenAI, etc.)
lev run agents/coder --task "Build a CLI that converts CSV to JSON"
lev dash                   # Open the dashboard to watch it work
```

Or create a custom agent:

```bash
lev create my-agent        # Generates an agent.leviath config file
cd my-agent
lev run --task "Your task here"
```

**Requirements:** An API key from any [supported provider](#providers), or [Ollama](https://ollama.com) for local models (no key needed). Pre-built binaries have no other dependencies.

## Features

**🧠 Structured Context Memory** — Six region types with deterministic eviction. Architecture docs stay pinned. Tool results evict first. Conversation auto-compacts into summaries. Route tool results to specific regions so file reads don't push out your system prompt. [Learn more →](https://leviath.dev/docs/context)

**🔀 Multi-Stage Workflows** — Each stage gets its own model, tools, context layout, and interaction mode. Sonnet for analysis, Opus for review, each seeing only the context it needs. Three modes: autonomous, interactive, and checkpoint-based. [Learn more →](https://leviath.dev/docs/stages)

**🎮 ECS Agent Engine** — Agents run as entities in a [bevy_ecs](https://bevyengine.org/) world. 50 agents share one process with game-engine-style scheduling, instead of 50 OS processes fighting for resources. [Learn more →](https://leviath.dev/docs/engine)

**🧬 Sub-Agents** — Agents spawn children with different blueprints. Unlike other tools, sub-agents at any depth can independently ask the user questions — no fire-and-forget, no routing through the parent. [Learn more →](https://leviath.dev/docs/sub-agents)

## Benchmarks

<!-- ⚠️ PLACEHOLDER: Replace with real numbers before launch -->

| Metric | Leviath | Flat Context Baseline | Improvement |
|--------|---------|----------------------|-------------|
| Context retention @ 50 tool calls | 94% | 61% | +54% |
| Context retention @ 100 tool calls | 89% | 34% | +162% |
| Token usage (avg per task) | 127K | 203K | -37% |
| Memory usage (25 concurrent agents) | 180MB | 4.2GB | -96% |
| Agent spawn overhead | <1ms | ~2s | — |

<details>
<summary><strong>Methodology</strong></summary>

<!-- ⚠️ PLACEHOLDER: Fill in methodology before launch -->

- **Context retention:** Architectural questions asked at intervals during a multi-file coding task. Same model (Claude Sonnet), same tools, only context management differs.
- **Token usage:** Average across 50 tasks from SWE-bench Lite. Structured regions vs single flat message array.
- **Memory usage:** RSS measured at steady state with N agents actively running inference.
- **Spawn overhead:** Time from spawn request to first inference call.

</details>

## Pre-built Agents

Seven agents ship out of the box:

| Agent | Stages | Best For |
|-------|--------|----------|
| **software-engineer** | plan → implement → review | Full coding workflow (default) |
| **coder** | analyze → implement → review | Focused implementation |
| **reviewer** | scan → deep_review → report | Code review and audit |
| **deep-researcher** | gather → analyze → synthesize | Thorough single-topic investigation |
| **wide-researcher** | survey → compare → summarize | Broad multi-topic survey |
| **daily-briefer** | collect → prioritize → brief | Morning summaries from multiple sources |
| **writing-assistant** | research → outline → draft → edit | Blog posts, reports, documentation |

## Dashboard

<!-- TODO: screenshot/gif of dashboard -->

`lev dash` — a full TUI for managing concurrent agents. Stage tabs, context window visualization, markdown rendering, search/filter, sub-agent tree view, clipboard yank, mouse support.

## API Server

`lev serve` exposes a REST + WebSocket API — build your own UI, integrate from any language, or orchestrate agents from a custom harness. Webhook callbacks, custom metadata, real-time event streaming. No SDK required, just HTTP.

```bash
lev serve --port 3000
```

[Full API reference →](https://leviath.dev/docs/api)

## CLI

| Command | Description |
|---------|-------------|
| `lev create <name>` | Create agent project |
| `lev run [path] --task "..."` | Run agent |
| `lev dash` | TUI dashboard |
| `lev serve` | API server |
| `lev pack` / `lev add` / `lev remove` | Package management |
| `lev list` | List agents |
| `lev test` | Run agent tests |
| `lev setup` / `lev models` | Configuration |

## Providers

| Provider | API Key |
|----------|---------|
| Anthropic | `ANTHROPIC_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| OpenRouter | `OPENROUTER_API_KEY` |
| Ollama | None (local) |
| Claude Code | None (subscription) |

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

## Contributing

```bash
git clone https://github.com/GEMISIS/leviath.git
cd leviath
cargo build
cargo test --workspace
cargo clippy --workspace
```

## License

[MIT](LICENSE)

---

<p align="center">
  <a href="https://leviath.dev">Website</a> ·
  <a href="https://leviath.dev/docs">Docs</a> ·
  <a href="https://github.com/GEMISIS/leviath">GitHub</a> ·
  <a href="https://github.com/GEMISIS/leviath/issues">Issues</a>
</p>
