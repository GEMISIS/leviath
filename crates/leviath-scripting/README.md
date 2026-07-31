# leviath-scripting

Rhai scripting for Leviath. Blueprints can ship custom validators,
transforms, script tools, and even whole context region implementations as
scripts, and this crate is the engine that runs them.

Part of [Leviath](https://github.com/Sun-Forge-AI/leviath), a structured
agent runtime for LLMs. Most applications should depend on the
[`leviath`](https://crates.io/crates/leviath) facade crate rather than this
one, and if you want the `lev` command-line tool, install
[`leviath-cli`](https://crates.io/crates/leviath-cli).

Docs live at [leviath.dev](https://leviath.dev). Licensed under the MIT
license.
