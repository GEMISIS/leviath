# leviath-alloc

Allocator tuning for the `lev` binary. It does one thing: it tells mimalloc to
hand freed memory back to the operating system when it is freed, rather than
the next time the thread that freed it allocates again. Without it, a daemon
worker that goes idle after a burst of work holds tens to hundreds of
megabytes it no longer uses until something wakes it. A user who sets
`MIMALLOC_PURGE_DELAY` keeps their own value.

It is a crate of its own because the call is a C function through FFI, which
needs `unsafe`, and every other crate in the workspace forbids `unsafe`. The one
call is audited in the crate's documentation, and the option index it passes
is checked when the crate builds and again by its tests.

Without the `mimalloc` feature it compiles no FFI at all, and the entry point
does nothing.

Part of [Leviath](https://github.com/GEMISIS/leviath), a structured
agent runtime for LLMs. Most applications should depend on the
[`leviath`](https://crates.io/crates/leviath) facade crate rather than this
one, and if you want the `lev` command-line tool, install
[`leviath-cli`](https://crates.io/crates/leviath-cli).
