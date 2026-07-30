# Contributing to Leviath

```bash
git clone https://github.com/Sun-Forge-AI/leviath.git
cd leviath
cargo build
cargo test --workspace
cargo clippy --workspace
```

## The pre-commit hook

The hook installs itself the first time you build or test — no setup step. `cargo test` / `cargo build` pulls in `xtask`'s dev-dependencies, which include [`cargo-husky`](https://github.com/rhysd/cargo-husky). On the first build it installs `.cargo-husky/hooks/pre-commit` into `.git/hooks/pre-commit` automatically.

The hook enforces, before every commit:

- **formatting** (`cargo fmt --check`)
- **clippy** with warnings-as-errors
- **doc lints** (`cargo doc` with `-D warnings` — no broken/private intra-doc links or stray HTML)
- the **full test suite**
- the **coverage-suppression-marker lint** (`ast-grep scan`, if `ast-grep` is installed; CI always enforces it)

It does **not** run the full `cargo xtask coverage` check — that's several minutes, too slow for a local commit gate. CI runs it on every push instead, enforcing 100% for real.

If the hook script itself changes (e.g. a commit edits `.cargo-husky/hooks/pre-commit`), `cargo-husky` only reinstalls it on a *fresh* compile of its crate, not on incremental builds. Force it with:

```bash
cargo clean -p cargo-husky && cargo test -p xtask
```

## `ast-grep` (suppression lint)

The suppression-marker scan uses [`ast-grep`](https://ast-grep.github.io), which matches Rust/YAML structurally (via tree-sitter) — rules live in `.sgrules/`. CI installs it automatically and always enforces the scan; the pre-commit hook runs it only if it's installed locally, otherwise it prints a warning and skips (CI is the backstop). Install it once with any of:

```bash
brew install ast-grep            # macOS / Linuxbrew
cargo install ast-grep --locked  # from source
npm install -g @ast-grep/cli     # via npm
```

## Testing policy

The workspace is gated at a hard **100%** on lines, regions, and functions — with no way to opt out. Coverage-suppression markers (`#[cfg(not(test))]`, `coverage(off)`, tarpaulin/lcov/grcov annotations) are banned by the ast-grep lint above, so code can't be hidden from measurement — it has to be refactored until it's testable. The *only* un-unit-tested code is the thin `lev` binary entrypoint (`crates/leviath-cli/src/main.rs`): the composition root that wires real terminal, stdin, network, and subprocess I/O into the library's tested cores. It's excluded from coverage measurement and guarded by a CI check that requires maintainer sign-off to change.

## Running coverage locally

`cargo xtask coverage` gates each workspace package with `cargo llvm-cov --package <pkg> --fail-under-{lines,functions,regions} 100` — llvm-cov does the counting *and* the gating; there's no custom parsing or aggregation. CI enforces a hard **100%** on all three metrics on Linux, macOS, and Windows — any file below 100% fails the build, and a browsable HTML report lands in the gitignored `coverage/` folder.

Measurement is deliberately **per-package**, not `--workspace`: `-C instrument-coverage` records every function in every binary that links it (including ones that never call it), and the whole-workspace merge can let a never-run record shadow the covered one — so one package at a time is what keeps llvm-cov's counts accurate. See the doc comment atop `xtask/src/coverage.rs`.

```bash
cargo xtask coverage                                   # full workspace
cargo llvm-cov --package <crate> --lib                 # a single crate
cargo llvm-cov --package <crate> --lib --html --open   # browsable per-crate report
```

> Branch coverage isn't collected: `cargo llvm-cov --branch` reliably SIGSEGVs inside LLVM's own coverage-mapping code ([open upstream bug](https://github.com/llvm/llvm-project/issues/119558)). See the doc comment atop `xtask/src/coverage.rs` for the full investigation.

## Regenerating the workflow diagrams

The agent workflow SVGs in `docs/assets/agents/` are rendered from the Mermaid sources in `docs/assets/agents/src/` — see `docs/assets/agents/src/README.md` for the render command and theme configs. If you change an agent's stage graph in `agents/<name>/agent.leviath`, update its `.mmd` source and re-render both the light and dark variants.
