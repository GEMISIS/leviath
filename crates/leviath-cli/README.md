# leviath-cli

The `lev` command-line tool for [Leviath](https://github.com/GEMISIS/leviath),
a structured agent runtime for LLMs: context memory laid out in regions with
token budgets, multi-stage workflows described by blueprints, and an ECS-based
execution engine.

## Install

```bash
cargo install leviath-cli
```

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
your terminal closes. `lev create my-agent` scaffolds a blueprint of your own:
models per stage, context regions and budgets, tools, and the workflow graph.

Full documentation is at [leviath.dev](https://leviath.dev).

## Embedding

To use the runtime as a library instead of a binary, depend on the
[`leviath`](https://crates.io/crates/leviath) crate.

Licensed under the MIT license.
