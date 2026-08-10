# leviath-net

Outbound-request policy for Leviath: which URLs an agent-driven fetch may
reach, and the shared HTTP client that enforces it. An agent fetches URLs the
model chose, and the model chose them from context an attacker can influence, so
the check runs before the request and again on every redirect hop.

Split out of [`leviath-core`](https://crates.io/crates/leviath-core), whose own
documentation describes it as plain serializable data with no async
dependencies. It was not: this module's HTTP client brought a little over a
hundred crates with it, so depending on Leviath's data types meant compiling all
of them.

Part of [Leviath](https://github.com/GEMISIS/leviath), a structured
agent runtime for LLMs. Most applications should depend on the
[`leviath`](https://crates.io/crates/leviath) facade crate rather than this
one, and if you want the `lev` command-line tool, install
[`leviath-cli`](https://crates.io/crates/leviath-cli).
