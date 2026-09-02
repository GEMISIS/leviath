<div align="center">

<img src="docs/assets/logo.png" alt="Leviath" width="440">

**A structured runtime for AI agents**

Give a model one flat list of messages and a single big file read pushes your system prompt
out of the window. Leviath gives it structure instead.

**Coherent.** Structured context regions mean an agent still knows what it read 50 tool calls ago.<br>
**Right-sized.** Each phase of a task gets its own model, tools, and context layout, so you aren't paying frontier prices for file reads.<br>
**Light.** Thousands of agents in one [bevy_ecs](https://bevyengine.org/) process, from a single binary. No Node, Python, or Docker.

[![Tests](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/GEMISIS/b35e030175e78fad8e3562e58be21c60/raw/leviath-test-all.json)](https://github.com/GEMISIS/leviath/actions/workflows/ci.yml)
[![Coverage](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/GEMISIS/b35e030175e78fad8e3562e58be21c60/raw/leviath-coverage-lines.json)](https://github.com/GEMISIS/leviath/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/GEMISIS/leviath/blob/main/LICENSE)
[![Docs](https://img.shields.io/badge/docs-leviath.dev-8b5cf6)](https://leviath.dev)
[![stable](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/GEMISIS/b35e030175e78fad8e3562e58be21c60/raw/leviath-channel-stable.json)](https://github.com/GEMISIS/leviath/releases/latest)
[![beta](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/GEMISIS/b35e030175e78fad8e3562e58be21c60/raw/leviath-channel-beta.json)](https://github.com/GEMISIS/leviath/releases/tag/beta)
[![alpha](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/GEMISIS/b35e030175e78fad8e3562e58be21c60/raw/leviath-channel-alpha.json)](https://github.com/GEMISIS/leviath/releases/tag/alpha)

**[Quick Start](#quick-start) · [Agents](#agents) · [Features](#features) · [Dashboard](#dashboard) · [API](#api-server) · [Comparison](#how-it-compares) · [Why not Leviath](#why-you-might-not-want-leviath) · [Contributing](#contributing)**

</div>

---

<p align="center">
  <img src="docs/assets/hero-final.gif" alt="Leviath's terminal dashboard running several agents concurrently" width="900">
</p>

## At a glance

```bash
lev run coder --task "Build a CLI that converts CSV to JSON"    # run a coding agent...
lev run deep-researcher --task "Survey solid-state batteries"   # ...or a research agent
lev ps                           # list running agents, and what each is waiting on
lev msg <agent-id> "..."         # steer a running agent mid-task
lev respond                      # answer questions agents are waiting on
lev dash                         # watch everything in the TUI dashboard
lev serve                        # REST + WebSocket API server
lev agent-client --agent coder   # serve an agent over the Agent Client Protocol
lev create my-agent              # scaffold your own agent
```

## Quick Start

### 1. Install

**macOS and Linux:**

```bash
curl -fsSL https://leviath.dev/install.sh | sh
```

**Windows**, in PowerShell (Windows Terminal opens one; not Command Prompt):

```powershell
irm https://leviath.dev/install.ps1 | iex
```

Both install prebuilt binaries, so no Rust toolchain is needed. Stable is the default; for beta or
alpha, pass the channel as an argument:
`curl -fsSL https://leviath.dev/install.sh | sh -s -- --channel beta`.
[Release channels →](https://leviath.dev/docs/releases)

<details>
<summary><b>Package managers</b>: Homebrew and Scoop, if you would rather manage it that way</summary>

<br/>

```bash
# macOS - what install.sh runs for you
brew tap gemisis/leviath https://github.com/GEMISIS/leviath-dist.git
brew trust gemisis/leviath          # Homebrew 6 requires trusting third-party taps
brew install leviath                # stable - or: leviath-beta, leviath-alpha
```

```powershell
# Windows
scoop bucket add leviath https://github.com/GEMISIS/leviath-dist.git
scoop install leviath
```

</details>

**Cargo** (any platform, requires [Rust](https://rustup.rs/)):

```bash
cargo install leviath-cli                # released version from crates.io
cargo install --git https://github.com/GEMISIS/leviath.git --bin lev   # latest development build
```

Leviath is also a library: add the [`leviath`](https://crates.io/crates/leviath) crate to embed the runtime in your own application. The [embedding guide](https://leviath.dev/docs/embedding) covers building a world, spawning agents, and streaming their events in-process.

### 2. Configure a provider

One provider is all you need: an API key from [Anthropic](https://console.anthropic.com/), [OpenAI](https://platform.openai.com/), [Google AI](https://aistudio.google.com/), or [OpenRouter](https://openrouter.ai/). No key at all? Run a local [Ollama](https://ollama.com), or turn on the [Claude Code transport](https://leviath.dev/docs/providers#claude-code-transport) with `lev setup --claude-code true` to run on your Claude subscription (the wizard does not offer it; read its terms-of-service note first).

```bash
lev setup      # interactive wizard

# scriptable. --install-agents is what puts the bundled blueprints on disk,
# so leaving it off means `lev run coder` has nothing to run.
lev setup --non-interactive --anthropic-key sk-ant-... --install-agents
```

### 3. Run an agent

```bash
lev run coder --task "Add pagination to the /users endpoint"

# ...or try a non-coding agent
lev run log-analyzer --task "Find what caused the error spike in ./logs last night"
```

`lev run` hands the agent to a background **daemon** that hosts every agent in one shared world, so runs keep going after your terminal closes. For unattended agents, `lev daemon install` puts it under launchd/systemd so it starts at login, restarts if it dies, and reloads interrupted runs. [Daemon docs →](https://leviath.dev/docs/daemon)

### 4. Create your own

```bash
lev create my-agent        # scaffolds a new agent directory
cd my-agent
lev run . --task "Your task here"
```

This writes an `agent.leviath` config you can customize: models per stage, context regions and their budgets, tools, and the workflow graph. [Agent configuration →](https://leviath.dev/docs/agents)

## Agents

Eight agents ship out of the box, covering coding, orchestration, review, research, data
gathering, and log analysis. Each is a multi-stage directed graph with structured context regions,
per-stage model fallback, and error recovery, and six of them fan out to cover several things at
once instead of one after another. `coder` is the largest:

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/agents/coder-dark.svg">
    <img src="docs/assets/agents/coder.svg" alt="The coder agent's workflow graph" width="560">
  </picture>
</p>

Diamonds are LLM-routed or human-in-the-loop decisions, and dotted edges fire automatically on a
runtime condition (like the `stuck` detector) rather than by the agent's choice. Every agent's
workflow graph is in the [agent catalog →](https://leviath.dev/docs/agent-catalog)

## Features

<table>
<tr>
<td width="50%" valign="top">
<b>Structured context memory</b>
<br><br>
The window is split into named regions, and you decide what each one drops
first. Architecture stays pinned, tool results go first, conversation compacts
into summaries, so a file dump can only crowd out the region it landed in.
Budgets are percentages of the window, so a blueprint keeps its shape when you
switch models.
<br><br>
<a href="https://leviath.dev/docs/context">Context docs →</a>
</td>
<td width="50%" valign="top">
<b>Multi-stage workflows</b>
<br><br>
Each stage gets its own model, tools, and context layout. Run them linearly or
as a directed graph with conditional transitions, error recovery, and
LLM-driven routing. A <code>stuck</code> edge escapes a stage that is making no
progress, and stuckness is measured by the runtime rather than self-reported by
the model.
<br><br>
<a href="https://leviath.dev/docs/stages">Stage docs →</a>
</td>
</tr>
<tr>
<td width="50%" valign="top">
<b>Human-in-the-loop</b>
<br><br>
<code>lev msg</code> drops a message straight into a running agent's context,
and the model sees it on its next inference call, so you redirect without
restarting. <code>interaction_points</code> force a checkpoint to approve,
revise, or edit the output directly, and <code>ask_user_*</code> tools let the
agent ask on its own judgment.
<br><br>
<a href="https://leviath.dev/docs/interaction">Interaction docs →</a>
</td>
<td width="50%" valign="top">
<b>Sandboxed execution and taint tracking</b>
<br><br>
Shell commands run on your machine by default, with nothing extra to install.
Opt into hardened containers or lighter Linux namespaces per agent or per
stage, or across the whole world in one config block, and an installed agent
can tighten its sandbox but never turn one off. Taint tracking (experimental)
gates exfiltration-capable tool calls before they fire.
<br><br>
<a href="https://leviath.dev/docs/security">Security docs →</a>
</td>
</tr>
<tr>
<td width="50%" valign="top">
<b>ECS agent engine</b>
<br><br>
Agents are entities in a <a href="https://bevyengine.org/">bevy_ecs</a> world,
so thousands share one process with game-engine-style scheduling instead of
that many OS processes. They won't stampede your provider either: a shared
per-model inference pool caps in-flight requests across the world, and an agent
waiting for a slot just sits as data.
<br><br>
<a href="https://leviath.dev/docs/engine">Engine docs →</a>
</td>
<td width="50%" valign="top">
<b>Sub-agents and fan-out</b>
<br><br>
Agents spawn children with different blueprints. A fan-out stage splits a task
into work items, runs one worker per item concurrently, and merges the results
back into the parent, all in the same process. Any sub-agent, at any depth, can
ask you questions directly.
<br><br>
<a href="https://leviath.dev/docs/sub-agents">Sub-agent docs →</a>
</td>
</tr>
</table>

## Dashboard

<p align="center">
  <img src="docs/assets/dashboard-final.png" alt="lev dash - the Leviath terminal dashboard showing the agent list and live activity log" width="900">
</p>

`lev dash` is a full TUI for managing concurrent agents: stage tabs, context-window visualization, markdown rendering, sub-agent tree view, and full mouse support including drag-to-copy (works over SSH). Press **`m`** to manage MCP tool servers without leaving the dashboard. [Dashboard docs →](https://leviath.dev/docs/dashboard)

## API Server

`lev serve` exposes a REST + WebSocket API, so anything that speaks HTTP can integrate with it. No SDK required. It covers agent lifecycle, human-in-the-loop interaction, per-agent streaming, and signed webhook callbacks on completion. [The Lair](https://leviath.dev/lair) is a browser console that drives it, so you get a web UI without writing one. Because the API can spawn tool-executing agents, it refuses to start without a token and binds to `127.0.0.1` by default.

```bash
export LEVIATH_API_TOKEN="$(openssl rand -hex 16)"
lev serve --port 3000

# spawn an agent (with a completion webhook + signing secret)
curl -X POST http://localhost:3000/api/agents \
  -H "Authorization: Bearer $LEVIATH_API_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"blueprint": "coder", "task": "Add input validation",
       "callback_url": "https://example.com/hook",
       "callback_secret": "whsec_…"}'
```

[Full API reference →](https://leviath.dev/docs/api)

## Observability

Production deployments can export structured traces, metrics, and logs over OpenTelemetry. Every run becomes a trace (`agent.run` → `agent.stage` → per-call `agent.inference` / `agent.tool_call` spans) alongside token counters, latency histograms, and log records carrying the run's trace ID. Off by default; one config block turns it on. [Observability docs →](https://leviath.dev/docs/observability)

## Agent Client Protocol

`lev agent-client --agent coder` serves any Leviath agent over the [Agent Client Protocol](https://agentclientprotocol.com) (JSON-RPC 2.0 over stdio), so hosts like [Zed](https://zed.dev) and [Gas City](https://github.com/gastownhall/gascity) can drive a headless agent as a child process. Wiring it into a host is config, not code. A `session/prompt` stays in flight until the run genuinely finishes, and hosts with `session/request_permission` get interactive tool approval in-turn. [Editor integration docs →](https://leviath.dev/docs/agent-client-protocol)

> "ACP" is claimed by two unrelated protocols; Leviath implements the Agent **Client** Protocol (JSON-RPC/stdio), not BeeAI's Agent Communication Protocol.

## How it compares

Leviath is a runtime that agents run *on*. Claude Code, Codex, and OpenHands are agents you work
with, CrewAI and LangGraph are frameworks you build agents *in*, and Gas Town and Gas City are
orchestrators that decide which work happens. Several of those are worth running alongside Leviath
rather than instead of it.

The full breakdown, including what each design buys and costs, when to reach for something else,
a factor-by-factor score against [12-Factor Agents](https://github.com/humanlayer/12-factor-agents),
and why you might not want Leviath at all, is on the docs site:
[Where Leviath sits →](https://leviath.dev/docs/comparison)

## Why you might not want Leviath

- **It's not a replacement for Claude Code, Codex, or your favorite coding agent.** Those are polished interactive products at a different layer, and Leviath can even run on top of Claude Code as a transport.
- **Agents are config, not code.** A Leviath agent is a TOML blueprint plus optional Rhai script tools. If you want to write agent logic in Python or TypeScript against an SDK, other languages drive Leviath through the REST API instead.
- **Agents execute on one machine.** The daemon hosts every agent in a single process on a single box. You can reach it from anywhere over the REST and WebSocket API, and it can call out through signed webhooks, but there is no hosted service and no scheduling work across several machines.
- **Isolation is at the data layer by default.** Every agent has its own state, working directory, tool policy and read-path grants, and a panic fails that agent alone rather than the daemon. The [OS sandbox](https://leviath.dev/docs/security) is opt-in: one `[sandbox]` block turns it on for the whole world and each agent gets its own container or namespace, which a blueprint may tighten per stage but never loosen. Two limits are worth knowing. There is no way to sandbox a single `lev run` on demand, and the boundary covers what an agent *executes* (shell, seed commands, script `shell()`) rather than file tools, `web_fetch`, or MCP servers, which stay on the host behind workdir confinement. [Widening it](https://github.com/GEMISIS/leviath/issues/326) is the intended end state.
- **You need a model provider**: an API key, a local Ollama, or the Claude Code transport (with its terms-of-service caveat).

## CLI

The [At a glance](#at-a-glance) block above covers the daily commands; the full surface (packaging, testing, policy, auth, daemon control) is in `lev --help` and the [CLI reference](https://leviath.dev/docs/cli). Every `config.toml` key and environment variable is in the [configuration reference](https://leviath.dev/docs/configuration).

Leviath also connects to [Model Context Protocol](https://modelcontextprotocol.io) tool servers over stdio or HTTP: `lev mcp add` detects OAuth servers and opens your browser to log in, and tokens are stored with `0600` permissions and refreshed automatically. [MCP docs →](https://leviath.dev/docs/mcp)

## Providers

Anthropic, OpenAI, Google (Gemini), OpenRouter, local [Ollama](https://ollama.com) with no key, the Claude Code subscription transport, and any OpenAI-compatible endpoint (llama.cpp, LM Studio, vLLM, an enterprise gateway) as a `kind = "openai-compatible"` entry in `[model_providers]`, with a [Rhai script](https://leviath.dev/docs/rhai-providers) for a wire format that is not OpenAI's. Per-stage model fallback and optional client-side rate limits enforced before each call. [Provider docs →](https://leviath.dev/docs/providers)

## Security

Leviath runs LLM-driven tools on your machine. [SECURITY.md](SECURITY.md) states plainly what it defends against and what it does not, and covers vulnerability reporting, hardening a `lev serve` deployment, and verifying a release's signed build provenance.

## Contributing

```bash
git clone https://github.com/GEMISIS/leviath.git
cd leviath
cargo build
cargo test --workspace
```

The workspace is gated at a hard **100% coverage on lines, regions, and functions**, with no opt-outs and coverage-suppression markers banned by lint; CI enforces it on Linux, macOS, and Windows. The only exclusion is the thin `lev` binary entrypoint, guarded by a CI check. [CONTRIBUTING.md](CONTRIBUTING.md) covers the rest.

<details>
<summary><b>Crate map</b></summary>

<br/>

Every platform-specific system call lives in one crate, `leviath-sys`, behind a cross-platform API, so the rest of the workspace is free of scattered per-OS branches.

```mermaid
graph TD
    CLI["leviath-cli"]
    LIB["leviath"]
    RT["leviath-runtime"]
    TOOLS["leviath-tools"]
    PROV["leviath-providers"]
    CORE["leviath-core"]
    MCP["leviath-mcp"]
    ACP["leviath-agent-client"]
    PKG["leviath-package"]
    SCRIPT["leviath-scripting"]
    TELEM["leviath-telemetry"]
    NET["leviath-net"]
    SYS["leviath-sys"]

    CLI --> RT
    CLI --> MCP
    CLI --> ACP
    CLI --> PKG
    CLI --> NET
    LIB --> RT
    LIB --> MCP
    LIB --> ACP
    LIB --> PKG
    LIB --> TELEM
    RT --> TOOLS
    RT --> PROV
    RT --> SCRIPT
    TOOLS --> CORE
    TOOLS --> SYS
    PROV --> CORE
    PROV --> SYS
    MCP --> CORE
    MCP --> SYS
    ACP --> CORE
    PKG --> CORE
    SCRIPT --> CORE
    TELEM --> CORE
```

| Crate | What it holds |
|---|---|
| `leviath-cli` | The `lev` binary: args, TUI, daemon, serve |
| `leviath` | Library facade for embedding the runtime |
| `leviath-runtime` | ECS engine (bevy_ecs) and stage-run orchestration |
| `leviath-core` | Regions, layouts, blueprints, manifest, run metadata |
| `leviath-tools` | Built-in tool implementations |
| `leviath-providers` | Anthropic, OpenAI, Google, OpenRouter, Ollama, Claude Code |
| `leviath-mcp` | MCP tool servers over stdio and HTTP/SSE |
| `leviath-agent-client` | Agent Client Protocol wire types (JSON-RPC over stdio) |
| `leviath-package` | Agent bundling and install |
| `leviath-scripting` | Rhai sandbox |
| `leviath-telemetry` | OpenTelemetry export |
| `leviath-net` | Outbound request policy and the shared HTTP client |
| `leviath-sys` | Every OS-specific syscall (permissions, signals, TTY) |
| `leviath-alloc` | One audited mimalloc option call for the binary |
| `leviath-testkit` | Shared test support |

</details>

## License

[MIT](LICENSE) © Gerald McAlister

---

<p align="center">
  <a href="https://leviath.dev">Website</a> ·
  <a href="https://leviath.dev/docs">Docs</a> ·
  <a href="https://github.com/GEMISIS/leviath">GitHub</a> ·
  <a href="https://github.com/GEMISIS/leviath/issues">Issues</a>
</p>
