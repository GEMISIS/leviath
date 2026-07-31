# leviath-runtime

Leviath's execution engine. Agents live as entities in an ECS world, and the
stage pipeline, persistence, fan-out to sub-agents, provider wiring, and
taint tracking are systems that run over them.

Part of [Leviath](https://github.com/Sun-Forge-AI/leviath), a structured
agent runtime for LLMs. Most applications should depend on the
[`leviath`](https://crates.io/crates/leviath) facade crate rather than this
one, and if you want the `lev` command-line tool, install
[`leviath-cli`](https://crates.io/crates/leviath-cli).

Docs live at [leviath.dev](https://leviath.dev). Licensed under the MIT
license.
