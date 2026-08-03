---
title: Getting Started
group: Get started
group_order: 1
order: 1
---

# Getting Started

Leviath is a structured agent runtime for LLMs. It gives an agent **structure**: context
that stays coherent across hundreds of tool calls, the right model for each phase of a task,
and hundreds of agents running at once in a single process.

You'll go from nothing to a running agent in four steps:

```mermaid
flowchart LR
  A["Install<br/>lev"] --> B["Configure<br/>a provider"]
  B --> C["Run<br/>an agent"]
  C --> D["Daemon hosts<br/>the run"]
  D --> E["Watch in<br/>lev dash"]
```

## Install

**macOS (Homebrew, recommended)**

```bash
brew tap gemisis/leviath https://github.com/GEMISIS/leviath-dist.git
brew trust gemisis/leviath # Homebrew 6 requires trusting third-party taps
brew install leviath            # or: leviath-beta, leviath-alpha
```

**Linux**

```bash
curl -fsSL https://raw.githubusercontent.com/GEMISIS/leviath-dist/main/install.sh \
  | bash -s -- --channel stable
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/GEMISIS/leviath-dist/main/install.ps1 | iex
```

Or with Scoop:

```powershell
scoop bucket add leviath https://github.com/GEMISIS/leviath-dist.git
scoop install leviath           # or: leviath-beta, leviath-alpha
```

Every install method offers three channels. `stable` is the default and is what you want unless you
have a reason to be ahead of it. See [Releases and channels](/docs/releases) for what `beta` and
`alpha` mean and how often each moves.

The three options above install prebuilt binaries; no Rust toolchain needed.

**Cargo** (any platform, needs [Rust](https://rustup.rs/)):

```bash
cargo install leviath-cli                # released version from crates.io
cargo install --git https://github.com/GEMISIS/leviath.git --bin lev   # latest development build
```

To embed the runtime in your own application instead, add the [`leviath`](https://crates.io/crates/leviath) crate as a dependency.

## Configure a provider

One provider is all you need: an API key from Anthropic, OpenAI, Google AI, or OpenRouter,
or a local [Ollama](https://ollama.com) with no key at all.

```bash
lev setup                                            # interactive wizard
lev setup --non-interactive --anthropic-key sk-ant-… # scriptable
```

> [!TIP]
> No API key handy? Point Leviath at a local [Ollama](https://ollama.com) install and run
> entirely offline. `lev setup` will detect it. See [Providers](/docs/providers) for the full list.

## Run an agent

Pick one of the ten [pre-built agents](/docs/agents) and give it a task:

```bash
lev run coder --task "Build a CLI that converts CSV to JSON"
lev run deep-researcher --task "Survey the state of solid-state batteries"
```

Leave `--task` off and your editor opens on a template, which is easier than
fighting shell quoting for anything longer than a sentence. It also takes a
file: `lev run coder --task ./brief.md`.

`lev run` doesn't run the agent in your terminal. It hands it to a background
[daemon](/docs/daemon) that hosts every agent in one shared world:

```mermaid
flowchart LR
  CLI["lev run"] -->|control socket| D
  DASH["lev dash"] -->|control socket| D
  subgraph D["Shared-world daemon (one process)"]
    A1["agent"]
    A2["agent"]
    A3["agent"]
  end
  D -->|provider API| P["LLM provider"]
```

Because the daemon owns the run, it keeps going after your terminal closes. Watch everything
live with the TUI [dashboard](/docs/dashboard):

```bash
lev dash
```

> [!NOTE]
> Prefer a browser or a REST/WebSocket client? `lev serve` exposes the same daemon over HTTP.
> See the [API](/docs/api) and the web console.

## On Windows

`lev` itself is the same on every platform, and the commands above work unchanged in PowerShell.
Two things around it differ.

Environment variables. The shell examples in these docs use the Unix `VAR=value command` prefix,
which PowerShell and `cmd` do not have:

```powershell
$env:ANTHROPIC_API_KEY = "sk-ant-..."     # PowerShell
lev run coder --task "Fix the failing test"
```

```bat
set ANTHROPIC_API_KEY=sk-ant-...
lev run coder --task "Fix the failing test"
```

Quoting. PowerShell strips the outer quotes before `lev` sees the argument, so a task containing a
literal quote needs escaping, and single quotes are safest when the text contains `$`:

```powershell
lev run coder --task 'Handle the $HOME case'
```

The agent's own shell is a separate matter: it runs through `cmd.exe`, not a POSIX shell, and
Leviath tells the model so. See [which shell you get](/docs/tools#which-shell-you-get).

## Create your own

```bash
lev create my-agent        # scaffolds an agent directory
cd my-agent
lev run . --task "Your task here"
```

This writes an `agent.leviath` config you can customize: the stages, the model for each phase,
and the context regions. See [Agents](/docs/agents) to go deeper.

## Where to go next

- [Agent blueprints](/docs/agents) is the natural next page. It covers what goes in an
  `agent.leviath` file.
- [Troubleshooting](/docs/troubleshooting) has the common snags, and `lev doctor` diagnoses most of
  them for you.
- [Glossary](/docs/glossary) defines every term these docs use in a particular way. Worth a skim if
  a page starts using a word you have not met.
- [Where Leviath sits](/docs/comparison) is for deciding whether you want Leviath at all, and what
  to run alongside it.
- [Where Leviath fits](/docs/integrations) covers driving Leviath from a tool you already use, like
  Gas City, Smithy, or a CI job.
