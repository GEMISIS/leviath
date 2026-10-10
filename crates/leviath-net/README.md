# leviath-net

Outbound-request policy for Leviath: which URLs an agent-driven fetch may
reach, and the shared HTTP client that enforces it. An agent fetches URLs the
model chose, and the model chose them from context an attacker can influence, so
the check runs before the request and again on every redirect hop. Also here:
the redirect policy for clients that carry credentials, which keeps a key on
the origin it was meant for, and the Server-Sent Events framing shared by every
client that reads an event stream.

It is a crate of its own so that [`leviath-core`](https://crates.io/crates/leviath-core)
can stay plain serializable data with no async dependencies: an HTTP client
brings a little over a hundred crates with it, and depending on Leviath's data
types should not mean compiling all of them.

Part of [Leviath](https://github.com/GEMISIS/leviath), a structured
agent runtime for LLMs. Most applications should depend on the
[`leviath`](https://crates.io/crates/leviath) facade crate rather than this
one, and if you want the `lev` command-line tool, install
[`leviath-cli`](https://crates.io/crates/leviath-cli).
