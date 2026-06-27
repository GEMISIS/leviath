# Leviath

**Hardware-inspired context window management for LLM agents.**

Leviath gives LLM agents structured, tiered memory instead of flat conversation arrays. Inspired by hardware memory architectures — think CPU cache hierarchies, not chat logs — it provides explicit control over what stays in context, what gets summarized, and what gets evicted when memory fills up.

## Why Leviath?

Every LLM agent framework today manages context the same way: a flat array of messages with uniform compaction when it gets too long. This is like running a computer with no memory hierarchy — just one big pool that gets randomly wiped when full.

Leviath replaces this with **typed memory regions**, each with its own lifecycle policy:

| Region Type | Behavior | Use Case |
|---|---|---|
| **Pinned** | Never evicted | System prompts, architecture docs |
| **SlidingWindow** | Keeps last N entries | Conversation history |
| **Temporary** | First to be evicted | Tool outputs, file contents |
| **Clearable** | Cleared entirely when space needed | Scratch space |
| **Compacting** | LLM-summarized when full | Long-running context |
| **CompactHistory** | Stores compaction summaries | Compressed knowledge |

The eviction cascade is deterministic: **Clearable → Temporary → Compacting → error**. Pinned and SlidingWindow regions are never touched.

## Quick Start

### Install

```bash
# From source
git clone https://github.com/GEMISIS/leviath.git
cd leviath
cargo install --path crates/leviath-cli
```

### Configure API Keys

Set an API key via environment variable or config file:

```bash
# Option 1: Environment variables (easiest)
export ANTHROPIC_API_KEY="sk-ant-..."
# or
export OPENAI_API_KEY="sk-..."
# or
export OPENROUTER_API_KEY="sk-or-..."

# Option 2: Config file (~/.leviath/config.toml)
mkdir -p ~/.leviath
cat > ~/.leviath/config.toml << 'EOF'
default_provider = "anthropic"

[providers]
anthropic_api_key = "sk-ant-..."
openai_api_key = "sk-..."

# Optional
# openrouter_api_key = "sk-or-..."
# ollama_base_url = "http://localhost:11434"
EOF
```

Environment variables are checked as fallback when the config file doesn't have a key:
- `ANTHROPIC_API_KEY`
- `OPENAI_API_KEY`
- `OPENROUTER_API_KEY`
- `OLLAMA_HOST`

### Create and Run an Agent

```bash
# Create a new agent project
lev init my-agent
cd my-agent

# Run it
lev run --task "Explain how memory hierarchies work"

# Or with a model override
lev run --task "Explain caching" --model claude-sonnet-4
```

### Templates

`lev init` supports three templates:

```bash
lev init my-coder --template coding      # analyze → implement stages
lev init my-researcher --template research  # gather → analyze → synthesize stages
lev init my-agent                          # single-stage default
```

## Agent Definition (`agent.leviath`)

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
max_iterations = 10
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

# LLM-based compaction config (optional — uses defaults if omitted)
[compaction]
provider = "anthropic"
model = "claude-sonnet-4"
max_summary_tokens = 2000
temperature = 0.2
# system_prompt = "Custom summarization prompt..."  # optional
```

### Stage Modes

- **`autonomous`** — runs without user input
- **`interactive`** — pauses after each inference for user input (Claude Code-style UX)
- **`interactive_points`** — runs autonomously but pauses at named checkpoints:

```toml
[stages.implement]
mode = "interactive_points"
model = { provider = "anthropic", model = "claude-sonnet-4-5" }

[[stages.implement.interaction_points]]
name = "design_review"
prompt = "Here's the proposed design. Approve or suggest changes:"
required = true

[[stages.implement.interaction_points]]
name = "pre_commit"
prompt = "Ready to commit. Any final changes?"
required = false
```

### Tool Result Routing

Control where tool outputs are stored in the context window:

```toml
[stages.implement.tool_routing]
default_region = "tool_results"
persist = true
max_result_tokens = 5000

[stages.implement.tool_routing.overrides]
read_file = "codebase"      # read_file results go to "codebase" region
search = "findings"          # search results go to "findings" region
```

## CLI Reference

| Command | Description |
|---|---|
| `lev init <name>` | Create a new agent project (`--template default\|coding\|research`) |
| `lev run [path] --task <task>` | Run an agent (`--model` to override) |
| `lev dashboard [path] --task <task>` | Interactive TUI for managing multiple agents |
| `lev spawn <blueprint>` | Spawn agent(s) from an installed blueprint (`--count N`) |
| `lev list` | List installed and available agents |
| `lev install <package>` | Install from `.leviath-bundle` file or registry |
| `lev pack [path]` | Bundle agent project for distribution (`--output` to override) |
| `lev test [path]` | Run agent tests (`--dry-run` for structure validation only) |
| `lev context [agent_id]` | Inspect context window state (`--detailed` for entries) |

## Dashboard

`lev dashboard` provides a terminal UI for managing multiple concurrent agents:

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

## Testing Agents

Create a `tests/` directory in your agent project:

**TOML test cases** (`tests/basic.toml`) — run the agent and check assertions:
```toml
[[test]]
name = "greeting"
input = "Say hello"
expect_contains = "hello"

[[test]]
name = "tool_usage"
input = "List the files"
expect_tool_call = "list_files"
```

**Rhai scripts** (`tests/validate.rhai`) — test validators and transforms:
```rhai
// Test that token counting works
let tokens = count_tokens("Hello world");
tokens > 0 && tokens < 100
```

Run tests:
```bash
lev test                  # Run all tests (requires API key)
lev test --dry-run        # Validate test structure only (no API calls)
lev test --filter greet   # Run only matching tests
```

## Packaging & Distribution

```bash
# Bundle your agent
lev pack
# → my-agent-0.1.0.leviath-bundle

# Install a bundled agent
lev install my-agent-0.1.0.leviath-bundle

# Now you can spawn it by name
lev spawn my-agent
```

## Architecture

```
leviath/
├── crates/
│   ├── leviath-core        # Types, regions, layouts, blueprints (no I/O)
│   ├── leviath-runtime     # bevy_ecs engine, scheduling, messaging
│   ├── leviath-providers   # Anthropic, OpenAI, OpenRouter, Ollama
│   ├── leviath-scripting   # Rhai scripting sandbox
│   ├── leviath-mcp         # MCP tool integration (JSON-RPC 2.0)
│   ├── leviath-package     # Manifests, bundling, registry client
│   └── leviath-cli         # `lev` binary
├── agents/                 # Pre-built agent definitions
│   ├── coder/              # Multi-stage coding agent
│   ├── reviewer/           # Code review agent
│   └── researcher/         # Research assistant
└── examples/               # Example code
```

## Development Setup

### Prerequisites

- **Rust 1.75+** (stable)
- **Git** (for worktree support)

### Building from Source

```bash
git clone https://github.com/GEMISIS/leviath.git
cd leviath
cargo build
cargo test
```

### Running During Development

```bash
# Run the CLI directly without installing
cargo run --bin lev -- init my-agent
cargo run --bin lev -- run --task "hello" my-agent/

# Or install locally for easier iteration
cargo install --path crates/leviath-cli
```

### Working with Git Worktrees

If you're developing multiple features or versions in parallel, git worktrees let you have multiple checked-out branches simultaneously, each with its own build and installed binary:

```bash
# Main development tree (you already have this)
cd ~/dev/leviath          # main branch

# Create worktrees for parallel work
git worktree add ../leviath-feat-serve feat/serve
git worktree add ../leviath-feat-google feat/google-provider
git worktree add ../leviath-v1 v1

# Each worktree has its own target/ directory and can build independently
cd ../leviath-feat-serve
cargo build                # builds in its own target/
cargo test                 # runs its own tests

# Install a specific version to a custom location
cargo install --path crates/leviath-cli --root ~/.local/leviath-feat-serve
# Binary at: ~/.local/leviath-feat-serve/bin/lev

# Or use aliases to switch between versions
alias lev-main='~/dev/leviath/target/debug/lev'
alias lev-serve='~/dev/leviath-feat-serve/target/debug/lev'
alias lev-v1='~/dev/leviath-v1/target/debug/lev'
```

**Tips for worktree development:**

- Each worktree gets its own `target/` directory — builds don't interfere
- Use `cargo install --root <path>` to install different versions to different locations
- Share the same `~/.leviath/config.toml` across all versions (API keys are version-independent)
- Worktree-specific config: set `LEVIATH_CONFIG=./local-config.toml` if you need different settings per branch
- List worktrees: `git worktree list`
- Clean up: `git worktree remove ../leviath-feat-serve`

### Running Tests

```bash
cargo test                           # All tests
cargo test -p leviath-core           # Just core crate
cargo test -p leviath-runtime        # Just runtime crate
cargo test -- --nocapture            # See test output
```

### Code Quality

```bash
cargo clippy                         # Lint check (should be zero warnings)
cargo fmt --check                    # Format check
```

### Project Conventions

- **No TODOs in code** — if it's not implemented, it's not merged
- **Zero warnings** — both `cargo build` and `cargo clippy` must be clean
- **All providers use real HTTP calls** — no mock implementations
- **Leviath owns all compaction** — never delegate to provider APIs (no OpenAI Responses API)
- **SlidingWindow is sacred** — never evicted, never shrunk

## Pre-built Agents

Three agents ship with Leviath in the `agents/` directory:

### Coder
Multi-stage coding agent: `analyze → implement → review` (review is interactive). Architecture is pinned, files are temporary, implementation history compacts automatically.

### Reviewer
Code review agent: `scan → deep_review → report`. Guidelines and diff are pinned, findings are temporary, analysis slides.

### Researcher
Research assistant: `gather → analyze → summarize`. Non-coding agent that proves Leviath is domain-agnostic. Findings compact into history automatically.

Copy any of these to start customizing:
```bash
cp -r agents/coder my-custom-coder
# Edit my-custom-coder/agent.leviath to your needs
lev run my-custom-coder/ --task "Build a REST API"
```

## Roadmap

### v1
- `lev serve` — HTTP/gRPC API for external orchestrators
- Gas City / Solar City integration
- Additional providers (Google, Azure)
- Agent marketplace

### v2
- Agent eval framework
- Package registry web UI
- Web UI for context visualization
- Agent-built context structures (self-organizing)

## License

MIT OR Apache-2.0

## Links

- **Website:** [leviath.dev](https://leviath.dev)
- **Repository:** [github.com/GEMISIS/leviath](https://github.com/GEMISIS/leviath)
