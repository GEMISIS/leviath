# Leviath

**Hardware-inspired context window management for LLM agents.**

Leviath gives LLM agents structured, tiered memory instead of flat conversation arrays. Inspired by hardware memory architectures — think CPU cache hierarchies, not chat logs — it provides explicit control over what stays in context, what gets summarized, and what gets evicted when memory fills up.

## The Problem

Every LLM agent framework manages context the same way: a flat array of messages with uniform compaction when it gets too long. This is like running a computer with one big memory pool that gets randomly wiped when full.

Leviath replaces this with **typed memory regions**, each with its own lifecycle:

```mermaid
graph LR
    subgraph Context Window
        P[🔒 Pinned<br><i>Never evicted</i>]
        SW[📜 SlidingWindow<br><i>Last N entries</i>]
        T[📎 Temporary<br><i>First evicted</i>]
        CL[🧹 Clearable<br><i>Wiped when needed</i>]
        CO[📦 Compacting<br><i>LLM-summarized</i>]
        CH[🗂️ CompactHistory<br><i>Stored summaries</i>]
    end

    CO -- summarizes into --> CH

    style P fill:#4a9eff,color:#fff
    style SW fill:#22c55e,color:#fff
    style T fill:#f59e0b,color:#fff
    style CL fill:#ef4444,color:#fff
    style CO fill:#8b5cf6,color:#fff
    style CH fill:#6366f1,color:#fff
```

When context fills up, eviction is deterministic:

```mermaid
flowchart LR
    Full[Context Full] --> CL[Clear<br>Clearable]
    CL -->|still full| T[Evict oldest<br>Temporary]
    T -->|still full| CO[Compact via LLM<br>Compacting → History]
    CO -->|still full| ERR[Error<br>Can't free more]

    P[Pinned] -.-x|NEVER| ERR
    SW[SlidingWindow] -.-x|NEVER| ERR

    style CL fill:#ef4444,color:#fff
    style T fill:#f59e0b,color:#fff
    style CO fill:#8b5cf6,color:#fff
    style ERR fill:#991b1b,color:#fff
    style P fill:#4a9eff,color:#fff
    style SW fill:#22c55e,color:#fff
```

## Quick Start

```bash
# Install
git clone https://github.com/GEMISIS/leviath.git
cd leviath
cargo install --path crates/leviath-cli

# Set your API key
export ANTHROPIC_API_KEY="sk-ant-..."

# Create and run an agent
lev init my-agent
cd my-agent
lev run --task "Explain how memory hierarchies work"
```

Other supported key sources: `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `OLLAMA_HOST`, or `~/.leviath/config.toml`:

```toml
[providers]
anthropic_api_key = "sk-ant-..."
openai_api_key = "sk-..."
```

## How It Works

```mermaid
flowchart TB
    subgraph Agent["agent.leviath"]
        direction TB
        B[Blueprint] --> S1[Stage: analyze<br><i>claude-sonnet-4-5</i>]
        B --> S2[Stage: implement<br><i>claude-sonnet-4-5</i>]
        B --> S3[Stage: review<br><i>claude-opus-4</i>]
        B --> CW[Context Window]
    end

    subgraph CW[" "]
        direction LR
        R1["🔒 architecture<br>4,000 tok"]
        R2["📎 files<br>30,000 tok"]
        R3["📜 conversation<br>15,000 tok"]
        R4["📦 impl_history<br>15,000 tok"]
        R5["🧹 scratch<br>10,000 tok"]
    end

    S1 -->|sequential| S2 -->|sequential| S3

    subgraph Providers
        A[Anthropic]
        O[OpenAI]
        OR[OpenRouter]
        OL[Ollama]
    end

    subgraph Tools["MCP Tools"]
        T1[read_file]
        T2[write_file]
        T3[search]
    end

    S2 --> A
    S2 --> Tools

    style Agent fill:#1e293b,color:#fff
    style R1 fill:#4a9eff,color:#fff
    style R2 fill:#f59e0b,color:#fff
    style R3 fill:#22c55e,color:#fff
    style R4 fill:#8b5cf6,color:#fff
    style R5 fill:#ef4444,color:#fff
```

Agents are defined in a single TOML file — no Rust code needed:

```toml
[agent]
name = "my-agent"
version = "0.1.0"
description = "A research assistant"

# Stages execute sequentially, each with its own model
[stages.gather]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-5" }
available_tools = ["web_search", "read_file"]

[stages.analyze]
mode = "autonomous"
model = { provider = "anthropic", model = "claude-sonnet-4-5" }

[stages.review]
mode = "interactive"  # Pauses for user input
model = { provider = "anthropic", model = "claude-opus-4" }

# Context regions — the memory map
[context.regions]
objective = { kind = "pinned", max_tokens = 2000 }
sources = { kind = "temporary", max_tokens = 40000 }
findings = { kind = "compacting", threshold_tokens = 8000, max_tokens = 15000 }
findings_history = { kind = "compact_history", source_region = "findings", max_tokens = 10000 }
conversation = { kind = "sliding_window", max_items = 15, max_tokens = 12000 }
scratch = { kind = "clearable", max_tokens = 8000 }

# Compaction config (optional — uses defaults if omitted)
[compaction]
provider = "anthropic"
model = "claude-sonnet-4"
```

## Stage Modes

```mermaid
flowchart LR
    subgraph Autonomous
        A1[Infer] --> A2{Tool calls?}
        A2 -->|yes| A3[Execute tools] --> A1
        A2 -->|no| A4[Done]
    end

    subgraph Interactive
        I1[Infer] --> I2[Show response]
        I2 --> I3[Wait for input]
        I3 --> I1
    end

    subgraph InteractivePoints
        IP1[Run N iterations] --> IP2[Pause at checkpoint]
        IP2 --> IP3[Get input]
        IP3 --> IP1
    end
```

- **`autonomous`** — runs without user input until complete
- **`interactive`** — pauses after each inference for user input
- **`interactive_points`** — runs autonomously but pauses at named checkpoints:

```toml
[stages.implement]
mode = "interactive_points"

[[stages.implement.interaction_points]]
name = "design_review"
prompt = "Here's the proposed design. Approve or suggest changes:"
required = true
```

## CLI

| Command | Description |
|---|---|
| `lev init <name>` | Create agent project (`--template default\|coding\|research`) |
| `lev run [path] --task <task>` | Run an agent (`--model` to override) |
| `lev dashboard [path] --task <task>` | TUI for managing multiple concurrent agents |
| `lev pack [path]` | Bundle for distribution → `.leviath-bundle` |
| `lev install <package>` | Install from bundle or registry |
| `lev spawn <name>` | Spawn from installed blueprint (`--count N`) |
| `lev list` | List installed and available agents |
| `lev test [path]` | Run tests (`--dry-run` for no API calls) |
| `lev context [agent_id]` | Inspect context window state |

## Dashboard

`lev dashboard` provides a terminal UI for managing multiple agents at once:

```
┌─ Agents ──────────────────────────────────────────────────┐
│ ID           │ Stage       │ Status     │ Tokens  │ Iter  │
│ coder-1      │ implement   │ ●ACTIVE    │ 45k/80k │ 12    │
│ coder-2      │ review      │ ◆WAITING   │ 32k/80k │ 8     │
│ reviewer-1   │ deep_review │ ●ACTIVE    │ 28k/50k │ 5     │
├─ Agent Detail ────────────────────────────────────────────┤
│ [coder-2] Waiting for input at: review                   │
│ The implementation looks good. Ready to commit?           │
│ > _                                                       │
├─ Log ─────────────────────────────────────────────────────┤
│ 09:15:32 coder-1: Called tool read_file(src/main.rs)      │
│ 09:15:35 coder-2: Waiting for user input                  │
└───────────────────────────────────────────────────────────┘
 [q]uit  [Enter]respond  [↑↓]select  [c]ancel  [k]ill  [n]ew
```

## Tool Result Routing

Control where tool outputs land in the context window:

```toml
[stages.implement.tool_routing]
default_region = "tool_results"
persist = true
max_result_tokens = 5000

[stages.implement.tool_routing.overrides]
read_file = "codebase"
search = "findings"
```

## Testing

Create `tests/` in your agent project:

```toml
# tests/basic.toml — run agent, check assertions
[[test]]
name = "greeting"
input = "Say hello"
expect_contains = "hello"
```

```rhai
// tests/validate.rhai — test validators
let tokens = count_tokens("Hello world");
tokens > 0 && tokens < 100
```

```bash
lev test                  # Run all (requires API key)
lev test --dry-run        # Validate structure only
```

## Packaging

```bash
lev pack                  # → my-agent-0.1.0.leviath-bundle
lev install agent.leviath-bundle
lev spawn my-agent
```

## Architecture

```mermaid
graph TB
    CLI["leviath-cli<br><code>lev</code> binary"]
    CLI --> Runtime
    CLI --> Package

    subgraph Engine
        Runtime["leviath-runtime<br>bevy_ecs engine"]
        Runtime --> Core["leviath-core<br>regions, layouts, blueprints"]
        Runtime --> Providers["leviath-providers<br>Anthropic, OpenAI,<br>OpenRouter, Ollama"]
        Runtime --> MCP["leviath-mcp<br>MCP tools (JSON-RPC)"]
    end

    Scripting["leviath-scripting<br>Rhai sandbox"] --> Core
    Package["leviath-package<br>bundling, registry"] --> Core

    style CLI fill:#4a9eff,color:#fff
    style Runtime fill:#22c55e,color:#fff
    style Core fill:#6366f1,color:#fff
    style Providers fill:#f59e0b,color:#fff
    style MCP fill:#ef4444,color:#fff
    style Scripting fill:#8b5cf6,color:#fff
    style Package fill:#64748b,color:#fff
```

## Pre-built Agents

Three agents ship in `agents/`:

- **Coder** — `analyze → implement → review` (interactive review). Architecture pinned, files temporary, implementation auto-compacts.
- **Reviewer** — `scan → deep_review → report`. Guidelines pinned, findings temporary.
- **Researcher** — `gather → analyze → summarize`. Proves Leviath is domain-agnostic. Findings compact automatically.

```bash
cp -r agents/coder my-coder
lev run my-coder/ --task "Build a REST API"
```

## Development

```bash
cargo build && cargo test    # 93 tests
cargo clippy                 # Zero warnings
```

**Git worktrees** for parallel development:

```bash
git worktree add ../leviath-feat-x feat/x
cd ../leviath-feat-x
cargo build                  # Own target/, no interference

# Version-specific installs
cargo install --path crates/leviath-cli --root ~/.local/leviath-feat-x
```

## License

MIT

**Website:** [leviath.dev](https://leviath.dev) · **Repository:** [github.com/GEMISIS/leviath](https://github.com/GEMISIS/leviath)
