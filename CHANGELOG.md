# Changelog

Notable changes to Leviath. Versions follow [semver](https://semver.org); the
workspace publishes in lockstep, so one version covers every `leviath-*` crate
and the `lev` binary.

Release binaries ship through the alpha, beta, and stable channels described
in [the release docs](https://leviath.dev/docs/releases); each versioned
GitHub release also carries auto-generated notes listing the merged pull
requests since the previous version.

## Unreleased

- Pause and resume are now user-facing: `lev pause <run-id>` and
  `lev resume <run-id>`, `POST /api/agents/{id}/pause` and `/resume` on the
  HTTP API, and `p`/`r` in the dashboard. A paused run shows as `paused` in
  `lev ps`, the dashboard, and the API, and comes back still paused after a
  daemon restart.
- Pausing a run that is waiting on input (or already finished) is now refused
  instead of silently accepted; the old behavior could wedge a fan-out parent
  by overwriting the status its merge poll depends on.
- Note for downgraders: run metadata written while a run is paused uses the
  new `paused` status, which older `lev` binaries cannot read. Resume or
  cancel paused runs before downgrading.
- Tool calls are now validated against the JSON Schema each tool advertises
  before they run. A call with missing, mistyped, or out-of-range arguments is
  refused back to the model with the concrete violations instead of executing
  on garbage or surfacing as a permission prompt. A schema that cannot be
  compiled (a typo'd Rhai `@param` type, an uninterpretable MCP fragment)
  skips validation for that tool with a logged warning, and external `$ref`s
  never resolve over the network.
- Taint-gate `[blocked]` results no longer count as successful modifications,
  so a stage whose writes were all blocked cannot satisfy a
  `require_modifications` transition gate.
- `send_to_agent`'s documented `target_region` argument now works: it was
  silently dropped on the sub-agent path and every message landed in the
  conversation region.
- Removed the unused message priority field; inbox delivery was always
  first-in, first-out in practice and now is by contract.

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
