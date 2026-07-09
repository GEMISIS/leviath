<p align="center">
  <h1 align="center">Leviath</h1>
  <p align="center">
    <strong>A structured agent runtime for LLMs</strong>
  </p>
</p>

<div align="center">

| Linux | macOS | Windows | Coverage |
| :-: | :-: | :-: | :-: |
| [![Linux](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/GEMISIS/b35e030175e78fad8e3562e58be21c60/raw/leviath-test-ubuntu-latest.json)](https://github.com/Sun-Forge-AI/leviath/actions/workflows/ci.yml) | [![macOS](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/GEMISIS/b35e030175e78fad8e3562e58be21c60/raw/leviath-test-macos-latest.json)](https://github.com/Sun-Forge-AI/leviath/actions/workflows/ci.yml) | [![Windows](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/GEMISIS/b35e030175e78fad8e3562e58be21c60/raw/leviath-test-windows-latest.json)](https://github.com/Sun-Forge-AI/leviath/actions/workflows/ci.yml) | [![Coverage](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/GEMISIS/b35e030175e78fad8e3562e58be21c60/raw/leviath-coverage-lines.json)](https://github.com/Sun-Forge-AI/leviath/actions/workflows/ci.yml) |

</div>

<p align="center">
  <a href="https://github.com/Sun-Forge-AI/leviath/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License: MIT"></a>
  <a href="https://leviath.dev"><img src="https://img.shields.io/badge/docs-leviath.dev-8b5cf6" alt="Docs"></a>
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

> **Private alpha:** the repo and its releases are currently private, so
> installing requires a GitHub Personal Access Token (`repo` scope) — exported
> as `GITHUB_TOKEN` for the install scripts, and `HOMEBREW_GITHUB_API_TOKEN`
> for Homebrew. The [distribution repo](https://github.com/Sun-Forge-AI/leviath-dist)
> has the one-time token setup and the full instructions.

**macOS (Homebrew):**

```bash
brew tap sun-forge-ai/leviath https://github.com/Sun-Forge-AI/leviath-dist.git
brew trust sun-forge-ai/leviath          # newer Homebrew requires trusting third-party taps
brew install leviath                     # stable — or: leviath-beta, leviath-alpha
```

**Linux:**

```bash
curl -fsSL -H "Authorization: token $GITHUB_TOKEN" \
  https://raw.githubusercontent.com/Sun-Forge-AI/leviath-dist/main/install.sh | bash -s -- --channel stable
# Channels: alpha (default), beta, stable
```

**Windows (PowerShell):**

```powershell
irm -Headers @{Authorization="token $env:GITHUB_TOKEN"} `
  https://raw.githubusercontent.com/Sun-Forge-AI/leviath-dist/main/install.ps1 | iex
# For a specific channel, download the script and run: .\install.ps1 -Channel stable
```

**Build from source (any platform, requires [Rust](https://rustup.rs/)):**

```bash
cargo install --git https://github.com/Sun-Forge-AI/leviath.git --bin lev
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

<table>
<tr>
<td width="33%" valign="top">

**🧠 Structured Context Memory**

Six region types with deterministic eviction — architecture stays pinned, tool results evict first, conversation auto-compacts into summaries. Route reads to specific regions so a file dump can't push out your system prompt.

[Learn more →](https://leviath.dev/docs/context)

</td>
<td width="33%" valign="top">

**🔀 Multi-Stage Workflows**

Each stage gets its own model, tools, and context layout. Run them linearly or as a [directed graph](https://leviath.dev/docs/stages#graph) with conditional transitions, error recovery, and LLM-driven routing — check it with `lev validate`.

[Learn more →](https://leviath.dev/docs/stages)

</td>
<td width="33%" valign="top">

**🎮 ECS Agent Engine**

Agents run as entities in a [bevy_ecs](https://bevyengine.org/) world. Dozens share one process with game-engine-style scheduling, instead of that many OS processes fighting for resources.

[Learn more →](https://leviath.dev/docs/engine)

</td>
</tr>
<tr>
<td width="33%" valign="top">

**🧬 Sub-Agents**

Agents spawn children with different blueprints. Any sub-agent, at any depth, can ask the user questions directly — no fire-and-forget, no routing through the parent.

[Learn more →](https://leviath.dev/docs/sub-agents)

</td>
<td width="33%" valign="top">

**💬 Mid-Run Collaboration**

Message agents while they work, from the terminal or the dashboard. Input is injected between inference calls, so you can redirect, answer a question, or add constraints without restarting. On by default.

</td>
<td width="33%" valign="top">

**🙋 Human-in-the-Loop**

Force a checkpoint every time a stage runs with `interaction_points` (plus `followups` for detail), or grant the agent `ask_user_*` tools so it asks on its own judgment. Both gated by the same per-stage tool permissions.

</td>
</tr>
</table>

Context regions are the core primitive — declare them per agent, and each stage inherits or overrides the layout:

```toml
[context.regions]
architecture = { kind = "pinned", max_tokens = 4000 }         # never evicted
conversation = { kind = "compacting", threshold_tokens = 8000 } # auto-summarizes when full
tool_results = { kind = "temporary", max_tokens = 20000 }      # oldest evicts first
scratch      = { kind = "clearable", max_tokens = 5000 }       # wipes clean between stages
```

## Benchmarks

<!-- ⚠️ Numbers below are targets — replace with actuals from benchmark runs before launch -->

**Context Retention** — same model, same tools, only context management differs:

| Metric | Leviath | Flat Context | Improvement |
|--------|---------|--------------|-------------|
| Retention @ 50 tool calls | 94% | 61% | +54% |
| Retention @ 100 tool calls | 89% | 34% | +162% |
| Multi-file consistency (10+ files) | 91% | 64% | +42% |
| Token usage (avg per task) | 127K | 203K | -37% |

**Prompt Caching** — regions ordered by volatility form a stable prefix that providers cache automatically:

| Provider | Cache Hit Rate | Cost Savings | Mechanism |
|----------|---------------|-------------|-----------|
| Anthropic | 70-85% | 55-65% | Explicit breakpoints, 90% discount |
| OpenAI | 50-70% | 25-35% | Auto prefix matching, 50% discount |
| Google | 50-70% | 35-50% | Auto prefix matching |

**Resource Efficiency** — ECS engine vs process-per-agent:

| Concurrent Agents | Leviath | Process-per-agent |
|-------------------|---------|-------------------|
| 25 | 180MB | 4.2GB |
| 50 | 310MB | 8.1GB |
| Spawn overhead | <1ms | ~2s |

[Full methodology →](https://leviath.dev/docs/benchmarks)

## Pre-built Agents

Nine agents ship out of the box:

| Agent | Stages | Best For |
|-------|--------|----------|
| **software-engineer** | plan ⇄ implement ⇄ review | Full coding workflow with graph transitions (default) |
| **coder** | analyze → implement ⇄ review | Focused implementation with review loop |
| **reviewer** | scan → deep_review → report | Code review and audit with error handling |
| **deep-researcher** | gather ⇄ analyze → synthesize | Thorough single-topic investigation |
| **wide-researcher** | survey ⇄ compare → summarize | Broad multi-topic landscape survey |
| **researcher** | gather ⇄ analyze → summarize | General-purpose research |
| **log-analyzer** | ingest → analyze ⇄ script → report | Log file analysis with scripted aggregation |
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

```mermaid
graph TD
    CLI["leviath-cli<br/><i>CLI binary (lev)</i>"]
    RT["leviath-runtime<br/><i>ECS engine (bevy_ecs)</i>"]
    CORE["leviath-core<br/><i>Regions, layouts, blueprints</i>"]
    PROV["leviath-providers<br/><i>Anthropic · OpenAI · Google<br/>OpenRouter · Ollama · Claude Code</i>"]
    MCP["leviath-mcp<br/><i>MCP tool integration (JSON-RPC)</i>"]
    SCRIPT["leviath-scripting<br/><i>Rhai sandbox</i>"]
    PKG["leviath-package<br/><i>Bundling & registry</i>"]
    TOOLS["leviath-tools<br/><i>Built-in tool implementations</i>"]

    CLI --> RT
    CLI --> SCRIPT
    CLI --> PKG
    CLI --> TOOLS
    RT --> CORE
    RT --> PROV
    RT --> MCP
```

## Contributing

```bash
git clone https://github.com/Sun-Forge-AI/leviath.git
cd leviath
cargo build
cargo test --workspace
cargo clippy --workspace
```

No manual setup step needed for the pre-commit hook — `cargo test`/`cargo build` above pulls in `xtask`'s dev-dependencies, which includes [`cargo-husky`](https://github.com/rhysd/cargo-husky), and it installs the hook script from `.cargo-husky/hooks/pre-commit` into `.git/hooks/pre-commit` automatically the first time you build or test the workspace. The hook enforces formatting, clippy (warnings-as-errors), all tests passing, the coverage-suppression-marker lint (`cargo xtask check-exclusions`), and that the coverage ceiling in `xtask/src/coverage.rs` wasn't silently raised (`cargo xtask check-ceiling`) before every commit — it does *not* run the full `cargo xtask coverage` check (too slow for a local commit gate, several minutes); that runs in CI on every push instead, enforcing that same ceiling for real. If the hook script itself is ever updated (e.g. a new commit changes `.cargo-husky/hooks/pre-commit`), `cargo-husky` only reinstalls it on a *fresh* compile of the `cargo-husky` crate, not on ordinary incremental builds — run `cargo clean -p cargo-husky && cargo test -p xtask` to force it to pick up the change.

### Running coverage locally

Use **`cargo xtask coverage`** — it runs `cargo-llvm-cov` across the whole workspace and reports region/line/function percentages. Output is written to the gitignored `coverage/` folder.

```bash
cargo xtask coverage                     # full workspace
cargo llvm-cov --package <crate> --lib   # coverage for a single crate
cargo llvm-cov --package <crate> --lib --html --open   # browsable per-crate report
```

Branch coverage isn't collected: `cargo llvm-cov --branch` reliably crashes with SIGSEGV inside LLVM's own coverage-mapping code, an [open, unresolved upstream bug](https://github.com/llvm/llvm-project/issues/119558) — see the doc comment at the top of `xtask/src/coverage.rs` for the full investigation.

## Releases

Leviath ships on three rolling channels, published automatically from CI:

| Channel | Cadence | GitHub release/tag | Stability |
|---|---|---|---|
| **Alpha** | Nightly | `alpha` | Bleeding edge |
| **Beta** | Weekly (Monday) | `beta` | Tested |
| **Stable** | Weekly (Thursday, approval-gated) | `latest` | Production |

Each channel tag is a *rolling* release — it's deleted and recreated on every
publish so it always points at the newest build for that channel. That's what
the shell/PowerShell installers resolve (`--channel alpha|beta|stable` →
`alpha`/`beta`/`latest`).

Separately, every **stable** deploy also cuts an **immutable versioned release**
(`vX.Y.Z`, which carries GitHub's "Latest" badge). These never change, so they're
what Homebrew and Scoop pin to and what `cargo install --tag vX.Y.Z` fetches. So
if you see a `vX.Y.Z` release *and* a `latest` release for the same version, that's
by design — one is the permanent archive, the other is the moving pointer.

Install commands for each channel are in the
[distribution repo](https://github.com/Sun-Forge-AI/leviath-dist).

## License

[MIT](LICENSE)

---

<p align="center">
  <a href="https://leviath.dev">Website</a> ·
  <a href="https://leviath.dev/docs">Docs</a> ·
  <a href="https://github.com/Sun-Forge-AI/leviath">GitHub</a> ·
  <a href="https://github.com/Sun-Forge-AI/leviath/issues">Issues</a>
</p>
