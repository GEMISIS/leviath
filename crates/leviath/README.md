# leviath

Leviath is a structured agent runtime for LLMs: context memory laid out in
regions with token budgets, multi-stage workflows described by blueprints, and
an ECS-based execution engine.

This crate is the library entry point. It re-exports the whole runtime under
one namespace so an application only needs a single dependency:

```toml
[dependencies]
leviath = "0.1"
```

```rust
use leviath::prelude::*;
```

The modules map one-to-one onto the underlying crates: `leviath::core`,
`leviath::runtime`, `leviath::providers`, `leviath::tools`, `leviath::mcp`,
`leviath::scripting`, `leviath::telemetry`, `leviath::package`, and
`leviath::agent_client`. If you only need one layer, you can depend on that
crate directly instead.

If you want the `lev` command-line tool rather than a library, install
[`leviath-cli`](https://crates.io/crates/leviath-cli).

Source, documentation, and issue tracker live at
[github.com/GEMISIS/leviath](https://github.com/GEMISIS/leviath).
See also [leviath.dev](https://leviath.dev).

Licensed under the MIT license.
