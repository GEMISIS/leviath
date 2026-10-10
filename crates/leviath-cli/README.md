# leviath-cli

The `lev` command-line tool for [Leviath](https://github.com/GEMISIS/leviath),
a structured agent runtime for LLMs: context memory laid out in regions with
token budgets, multi-stage workflows described by blueprints, and an ECS-based
execution engine.

## Install

```bash
cargo install leviath-cli
```

That builds every model provider in. To build only the ones you use, turn the
defaults off and name them:

```bash
cargo install leviath-cli --no-default-features --features anthropic,openai
```

The features are `anthropic`, `openai`, `openai-subscription` (Codex),
`xai`, `xai-subscription` (Grok), `google`, `openrouter`, `bedrock`, `meta`,
`ollama`, `meshy` and `rhai` (script providers). With none, `lev` reaches only
the OpenAI-compatible endpoints in your config.
[Choosing providers](https://leviath.dev/docs/choosing-providers) has the rest.

Prebuilt binaries skip the compile. On macOS:

```bash
brew tap gemisis/leviath https://github.com/GEMISIS/leviath-dist.git
brew trust gemisis/leviath
brew install leviath
```

Install scripts for Linux and Windows, plus a Scoop bucket, are in the
[main README](https://github.com/GEMISIS/leviath#readme).

## Quick start

Set up a provider, then run one of the bundled agents:

```bash
lev setup                # interactive wizard, installs bundled agents too
lev run coder --task "Add pagination to the /users endpoint"
```

`lev run` hands the agent to a background daemon that keeps runs going after
your terminal closes. `lev create my-agent` scaffolds a blueprint of your own,
an `agent.toml` holding the inputs it takes, models per stage, context regions
and budgets, tools, and the workflow graph. `lev validate my-agent` checks it.

Full documentation is at [leviath.dev](https://leviath.dev).

## Wrapping lev

The whole of `lev` is `leviath_cli::run()`. A binary of your own that wraps
it, under another name or with setup of its own, can be this small:

```rust
fn main() -> std::process::ExitCode {
    leviath_cli::run()
}
```

`run` prints everything lev has to say, help and errors included, and returns
the exit code instead of exiting: 0, 2 for a command line lev refused, or 1
for a command that failed. Anything your `main` does after it still runs.
`leviath_cli::run_from(argv)` takes the arguments from you instead of the
process, so you can add or rewrite them before lev parses them.

Two things to keep:

- lev starts its daemon by running its own executable as `<exe> daemon`. Pass
  that through to `run` unchanged.
- The allocator belongs to your binary. `lev` uses mimalloc, and its
  [`main.rs`](src/main.rs) shows the two lines that set it up. A long-running
  daemon holds far less memory with them.

## Embedding

To use the runtime as a library instead of a binary, depend on the
[`leviath`](https://crates.io/crates/leviath) crate.

Licensed under the MIT license.
