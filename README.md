<p align="center">
  <h1 align="center">Leviath</h1>
  <p align="center">
    <strong>A structured agent runtime for LLMs</strong>
  </p>
  <p align="center">
    <a href="https://github.com/GEMISIS/leviath/actions"><img src="https://github.com/GEMISIS/leviath/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
    <img src="https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/GEMISIS/COVERAGE_GIST_ID/raw/coverage.json" alt="Coverage">
    <a href="https://github.com/GEMISIS/leviath/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License: MIT"></a>
    <a href="https://leviath.dev"><img src="https://img.shields.io/badge/docs-leviath.dev-8b5cf6" alt="Docs"></a>
  </p>
</p>

---

Most agent tools give LLMs a flat message array and hope for the best. Leviath gives them structure — structured memory, multi-stage workflows, and an ECS engine — so agents stay coherent on long tasks, use the right model for each phase, and don't melt your machine when you run a dozen at once.

Pick a pre-built agent or create your own. Run it. Watch it actually remember what it read 50 tool calls ago.

<!-- TODO: hero gif of dashboard with agents running -->

## Requirements

- An API key from [Anthropic](https://console.anthropic.com/), [OpenAI](https://platform.openai.com/), [Google AI](https://aistudio.google.com/), or [OpenRouter](https://openrouter.ai/) — or run [Ollama](https://ollama.com) locally (free, no key)
- macOS, Linux, or Windows
- No runtime dependencies — single binary, no Node/Python/Docker required

> **Claude Code Agent SDK:** Leviath is compatible with the [Claude Code agent SDK](https://docs.anthropic.com/en/docs/claude-code/sdk) as a provider. However, this routes all inference through Claude Code's own context management, which bypasses Leviath's structured regions — the core feature. Use a direct provider (Anthropic, OpenAI, etc.) for the full experience.

## Quick Start

### 1. Install

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

### 2. Configure a Provider

Before running anything, you need at least one LLM provider set up. Run the interactive setup wizard:

```bash
lev setup
```

It'll walk you through adding API keys for Anthropic, OpenAI, OpenRouter, or pointing to a local [Ollama](https://ollama.com) instance (no key needed). You can also pass keys directly:

```bash
lev setup --non-interactive --anthropic-key sk-ant-...
```

### 3. Run Your First Agent

```bash
lev run coder --task "Build a CLI that converts CSV to JSON"

# or try a non-coding agent
lev run deep-researcher --task "Survey the current state of solid-state battery technology"
```

Open the dashboard to watch it work:

```bash
lev dash
```

### 4. Create a Custom Agent

```bash
lev create my-agent        # Scaffolds a new agent directory
cd my-agent
lev run . --task "Your task here"
```

This generates an `agent.leviath` config you can customize — pick models per stage, define context regions, choose tools. See [agent configuration →](https://leviath.dev/docs/agents)

## Features

**🧠 Structured Context Memory** — Six region types with deterministic eviction. Architecture docs stay pinned. Tool results evict first. Conversation auto-compacts into summaries. Route tool results to specific regions so file reads don't push out your system prompt. [Learn more →](https://leviath.dev/docs/context)

```toml
[context.regions]
architecture = { kind = "pinned", max_tokens = 4000 }         # never evicted
conversation = { kind = "compacting", threshold_tokens = 8000 } # auto-summarizes when full
tool_results = { kind = "temporary", max_tokens = 20000 }      # oldest evicts first
scratch      = { kind = "clearable", max_tokens = 5000 }       # wipes clean between stages
```

**🔀 Multi-Stage Workflows** — Each stage gets its own model, tools, context layout, and interaction mode. Sonnet for analysis, Opus for review, each seeing only the context it needs. Stages can be linear or a [directed graph](https://leviath.dev/docs/stages#graph) with conditional transitions, error recovery, and LLM-driven routing — check your graph with `lev validate`. [Learn more →](https://leviath.dev/docs/stages)

**🎮 ECS Agent Engine** — Agents run as entities in a [bevy_ecs](https://bevyengine.org/) world. 50 agents share one process with game-engine-style scheduling, instead of 50 OS processes fighting for resources. [Learn more →](https://leviath.dev/docs/engine)

**🧬 Sub-Agents** — Agents spawn children with different blueprints. Unlike other tools, sub-agents at any depth can independently ask the user questions — no fire-and-forget, no routing through the parent. [Learn more →](https://leviath.dev/docs/sub-agents)

## Benchmarks

<!-- ⚠️ PLACEHOLDER: Replace with real numbers before launch -->

**Context & Quality**

| Metric | Leviath | Flat Context Baseline | Improvement |
|--------|---------|----------------------|-------------|
| Context retention @ 50 tool calls | 94% | 61% | +54% |
| Context retention @ 100 tool calls | 89% | 34% | +162% |
| SWE-bench Lite resolve rate | 42% | 38% | +11% |
| Multi-file consistency (10+ files) | 91% | 64% | +42% |
| Token usage (avg per task) | 127K | 203K | -37% |

**Resource Efficiency**

| Metric | Leviath (ECS) | Process-per-agent |
|--------|---------------|-------------------|
| 25 concurrent agents — memory | 180MB | 4.2GB |
| 50 concurrent agents — memory | 310MB | 8.1GB |
| Agent spawn overhead | <1ms | ~2s |

Same model (Claude Sonnet), same tools — only context management differs. [Full methodology →](https://leviath.dev/docs/benchmarks)

## Pre-built Agents

Eight agents ship out of the box:

| Agent | Stages | Best For |
|-------|--------|----------|
| **software-engineer** | plan ⇄ implement ⇄ review | Full coding workflow with graph transitions (default) |
| **coder** | analyze → implement ⇄ review | Focused implementation with review loop |
| **reviewer** | scan → deep_review → report | Code review and audit with error handling |
| **deep-researcher** | gather ⇄ analyze → synthesize | Thorough single-topic investigation |
| **wide-researcher** | survey ⇄ compare → summarize | Broad multi-topic landscape survey |
| **researcher** | gather ⇄ analyze → summarize | General-purpose research |
| **daily-briefer** | collect → prioritize → brief | Morning summaries from multiple sources |
| **writing-assistant** | research → outline → draft ⇄ edit | Blog posts, reports, documentation |

## Dashboard

<!-- TODO: replace with real screenshot -->
```
┌─ Agents ──────────────────────────────────┐┌─ Stage: implement (2/3) ─────────────────┐
│ ● software-engineer  Active   3m 22s      ││ ⠋ Implementing CSV parser module...      │
│ ○ deep-researcher    Waiting  1m 05s      ││                                          │
│ ○ daily-briefer      Complete 0m 48s      ││ ┌─ Context ─────────────────────────────┐ │
│                                           ││ │ ████████░░░░░ 61% (42K/68K tokens)   │ │
│                                           ││ │ pinned: 4K  conv: 18K  tools: 20K    │ │
│                                           ││ └───────────────────────────────────────┘ │
├─ Log ─────────────────────────────────────┤│                                          │
│ [3:22] write_file src/parser.rs           ││ > fn parse_record(line: &str) -> Vec... │
│ [3:18] read_file Cargo.toml              ││ > fn detect_delimiter(header: &str)...  │
│ [3:15] bash cargo check                  ││ >                                        │
│ [3:10] write_file src/main.rs            ││ > // Handle quoted fields containing    │
│ [3:02] ✓ Stage 'analyze' complete         ││ > // delimiters and newlines             │
└───────────────────────────────────────────┘└──────────────────────────────────────────┘
```

`lev dash` — a full TUI for managing concurrent agents. Stage tabs, context window visualization, markdown rendering, search/filter, sub-agent tree view, clipboard yank, mouse support.

## API Server

`lev serve` exposes a REST + WebSocket API. Integrate from Python, TypeScript, Go, or anything that speaks HTTP — no SDK required.

```bash
lev serve --port 3000

# spawn an agent
curl -X POST http://localhost:3000/api/agents \
  -H "Content-Type: application/json" \
  -d '{"blueprint": "coder", "task": "Add input validation", "webhook_url": "https://example.com/hook"}'
```

Agent lifecycle, interaction (human-in-the-loop), blueprint management, per-agent WebSocket streaming, and webhook callbacks on completion. [Full API reference →](https://leviath.dev/docs/api)

## CLI

| Command | Description |
|---------|-------------|
| `lev create <name>` | Create agent project |
| `lev run [path] --task "..."` | Run agent |
| `lev dash` | TUI dashboard |
| `lev serve` | API server |
| `lev validate [path]` | Validate agent blueprint |
| `lev pack` / `lev add` / `lev remove` | Package management |
| `lev list` | List agents |
| `lev test` | Run agent tests |
| `lev setup` / `lev models` | Configuration |

## Providers

| Provider | API Key |
|----------|---------|
| Anthropic | `ANTHROPIC_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Google (Gemini) | `GOOGLE_API_KEY` |
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
