# Changelog

Notable changes to Leviath. Versions follow [semver](https://semver.org); the
workspace publishes in lockstep, so one version covers every `leviath-*` crate
and the `lev` binary.

Release binaries ship through the alpha, beta, and stable channels described
in [the release docs](https://leviath.dev/docs/releases); each versioned
GitHub release also carries auto-generated notes listing the merged pull
requests since the previous version.

## 0.1.1 - 2026-07-31

Post-launch cleanup.

- The daemon's launchd service label is now `dev.leviath.daemon`;
  `lev daemon install`/`uninstall` also remove any registration under the old
  `ai.sunforge.leviath` label, so upgrading cannot leave a stale supervised
  daemon behind.
- The `lev run` error hint shows a working invocation.
- Removed the outdated per-agent READMEs bundled with the CLI (the
  [agent catalog](https://leviath.dev/docs/agent-catalog) is the maintained
  reference); improved the crates.io pages with inline install steps and a
  runnable library example.
- crates.io releases are now published automatically from each stable deploy,
  from the same commit the binaries are built at.

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
