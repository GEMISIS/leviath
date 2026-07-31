---
title: Getting Started
group: Start
order: 1
---

# Getting Started

Leviath is a structured agent runtime for LLMs. It gives an agent **structure** — context
that stays coherent across hundreds of tool calls, the right model for each phase of a task,
and a dozen agents running at once in a single process.

## Install

> **Private alpha.** Installing needs a GitHub Personal Access Token (`repo` scope):
> `GITHUB_TOKEN` for the scripts and `HOMEBREW_GITHUB_API_TOKEN` for Homebrew. One-time setup
> lives in the [distribution repo](https://github.com/Sun-Forge-AI/leviath-dist).

**macOS (Homebrew)**

```bash
brew tap sun-forge-ai/leviath https://github.com/Sun-Forge-AI/leviath-dist.git
brew trust sun-forge-ai/leviath
brew install leviath            # or: leviath-beta, leviath-alpha
```

**Linux**

```bash
curl -fsSL -H "Authorization: token $GITHUB_TOKEN" \
  https://raw.githubusercontent.com/Sun-Forge-AI/leviath-dist/main/install.sh \
  | bash -s -- --channel stable
```

**Windows (PowerShell)**

```powershell
irm -Headers @{Authorization="token $env:GITHUB_TOKEN"} `
  https://raw.githubusercontent.com/Sun-Forge-AI/leviath-dist/main/install.ps1 | iex
```

**From source** (any platform, needs [Rust](https://rustup.rs/)):

```bash
cargo install --git https://github.com/Sun-Forge-AI/leviath.git --bin lev
```

## Configure a provider

One provider is all you need — an API key from Anthropic, OpenAI, Google AI, or OpenRouter,
or a local [Ollama](https://ollama.com) with no key at all.

```bash
lev setup                                            # interactive wizard
lev setup --non-interactive --anthropic-key sk-ant-… # scriptable
```

See [Providers](/docs/providers) for the full list and the Claude Code transport.

## Run an agent

```bash
lev run coder --task "Build a CLI that converts CSV to JSON"
lev run deep-researcher --task "Survey the state of solid-state batteries"
```

`lev run` hands the agent to a background [daemon](/docs/daemon), so runs keep going after your
terminal closes. Watch everything with the TUI [dashboard](/docs/dashboard):

```bash
lev dash
```

## Create your own

```bash
lev create my-agent        # scaffolds an agent directory
cd my-agent
lev run . --task "Your task here"
```

This writes an `agent.leviath` config you can customize — see [Agents](/docs/agents).
