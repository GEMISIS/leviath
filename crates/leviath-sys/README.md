# leviath-sys

The platform-specific pieces of Leviath behind one cross-platform API:
process control and signals, socket peer credentials, terminal handling,
and the OS keychain. Despite the -sys suffix, this is not an FFI binding
crate; it is where Leviath's OS-specific code is quarantined.

Part of [Leviath](https://github.com/GEMISIS/leviath), a structured
agent runtime for LLMs. Most applications should depend on the
[`leviath`](https://crates.io/crates/leviath) facade crate rather than this
one, and if you want the `lev` command-line tool, install
[`leviath-cli`](https://crates.io/crates/leviath-cli).

Docs live at [leviath.dev](https://leviath.dev). Licensed under the MIT
license.
