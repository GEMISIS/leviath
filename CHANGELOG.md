# Changelog

Notable changes to Leviath. Versions follow [semver](https://semver.org); the
workspace publishes in lockstep, so one version covers every `leviath-*` crate
and the `lev` binary.

Release binaries ship through the alpha, beta, and stable channels described
in [the release docs](https://leviath.dev/docs/releases); each versioned
GitHub release also carries auto-generated notes listing the merged pull
requests since the previous version.

## 0.1.0 - 2026-07-31

First public release.

- The `lev` binary: run multi-stage agents in a shared-world daemon, with a
  TUI dashboard, REST + WebSocket API, Agent Client Protocol support, and MCP
  tool servers.
- Ten bundled agent blueprints installed by `lev setup`.
- The `leviath` library crate: the whole runtime behind one dependency, with
  `leviath-core`, `leviath-runtime`, and the other layer crates published
  individually for slimmer builds.
- Structured context regions with token budgets, sandboxed tool execution,
  experimental taint tracking, Rhai scripting for providers, tools, regions,
  and policy rules, and OpenTelemetry export.
