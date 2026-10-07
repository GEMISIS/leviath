# leviath-package

Packaging and installation for Leviath blueprints: bundling a blueprint
directory (its `agent.toml` and the files beside it) for sharing, and
installing one into the local data root.

Part of [Leviath](https://github.com/GEMISIS/leviath), a structured
agent runtime for LLMs. Most applications should depend on the
[`leviath`](https://crates.io/crates/leviath) facade crate rather than this
one, and if you want the `lev` command-line tool, install
[`leviath-cli`](https://crates.io/crates/leviath-cli).

Docs live at [leviath.dev](https://leviath.dev). Licensed under the MIT
license.
