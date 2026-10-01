# leviath-blueprint

Blueprints for Leviath. A blueprint is a named run graph kept in an
`agent.toml` file: a `[blueprint]` table that says what it is called, and a
`[graph]` table that is the run graph itself, in the same shape a raw spawn
request carries. A caller changes a blueprint's run only through the inputs
its graph declares.

This crate reads and checks those files, turns a blueprint reference and its
inputs into a spawn request, and converts the older `agent.leviath` format
with `migrate`.

Part of [Leviath](https://github.com/GEMISIS/leviath), a structured
agent runtime for LLMs. Most applications should depend on the
[`leviath`](https://crates.io/crates/leviath) facade crate rather than this
one, and if you want the `lev` command-line tool, install
[`leviath-cli`](https://crates.io/crates/leviath-cli).

Docs live at [leviath.dev](https://leviath.dev). Licensed under the MIT
license.
