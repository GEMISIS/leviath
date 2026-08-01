# Changelog

Notable changes to Leviath. Versions follow [semver](https://semver.org); the
workspace publishes in lockstep, so one version covers every `leviath-*` crate
and the `lev` binary.

Release binaries ship through the alpha, beta, and stable channels described
in [the release docs](https://leviath.dev/docs/releases); each versioned
GitHub release also carries auto-generated notes listing the merged pull
requests since the previous version.

## Unreleased

- `lev ps` says why a run is waiting. `waiting` was one word for six unrelated
  situations, so an operator could not tell a run stopped on an approval prompt
  from a parent parked while its workers churn. It now reads
  `waiting: tool approval` or `waiting: children(3)`, alongside stage,
  iteration, tool-call, and age columns. `lev ps --help` defines every status
  and reason, and `lev ps --json` prints the raw listing for scripts.
- The `AGE` column measures time since the run last actually moved, which
  `meta.json`'s `updated_at` does not: that also advances on a 30-second
  heartbeat, so it stays fresh on a wedged run.
- `--yolo` now applies to the whole run tree. Sub-agents and fan-out workers
  inherit it instead of being spawned attended, so a child can no longer stop on
  a prompt nobody is watching for and strand the parent waiting on it.
- `--yolo` also survives a daemon restart, persisted as `yolo` in `meta.json`.
  It used to be dropped on reload on the grounds that forgetting an override can
  only prompt more; in practice that turned a running unattended job into one
  parked forever. Runs written by older versions default to attended. A
  configured `deny` still beats `--yolo`, and `ask_user_choice` still refuses to
  answer blind.
- A stage holding for its sub-agents could be walked back to `active` while
  those children were still running, if an unrelated prompt of its own resolved.
- Fixed a slot leak that could park the daemon with capacity it could not see.
  Releasing an inference-pool permit now wakes the tick loop, so the agents
  queued on a full model pool are re-driven and can take the freed slot. A
  cancelled inference used to hand its slot back in silence, and the loop is
  event-driven, so the freed capacity stayed invisible until something
  unrelated happened to wake it.
- The daemon now re-drives itself on a timer (every 30s) instead of relying
  solely on wakeups. Any missed wake anywhere is bounded to one interval rather
  than parking the daemon indefinitely - previously an agent whose provider was
  not registered, for example, sat at iteration 0 with the daemon completely
  idle and silent.
- Added a lane heartbeat so pool pressure is visible: per-model inference
  occupancy, tool-lane busy/queued counts, and agents by status. It logs at
  `info` only when a lane is at capacity with work queued behind it, and at
  `debug` otherwise, so an idle daemon stays quiet.
- Fixed runs that were spawned but never executed: they sat at iteration 0 with
  no tokens, reported as `running` for ever. A `lev run` whose stages have no
  configured provider is now refused outright, naming the stage and every
  provider it tried, instead of starting a run that could never take a turn.
- A spawn that fails now records the failure in the run directory it staked
  out, rather than leaving a `starting` placeholder that claimed the run was
  alive for ever.
- A run that ends up unable to dispatch anyway - a provider removed from the
  config after it started, say - is now failed once its stall outlives
  `[limits] stall_timeout_secs` (default 60 seconds; `0` waits indefinitely, as
  before). Waiting for a busy model's inference pool is never failed: that is
  ordinary backpressure, however long it lasts.
- An async lane task that dies without reporting (a provider adapter that
  panics) no longer strands its agent waiting for a completion that can never
  arrive; it surfaces as an ordinary inference, routing, or compaction error.
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
- Agents can be granted read access outside their working directory with a
  `[read_paths]` block. The declaration is inert on its own: your config must
  grant it via `[security] read_paths` or `[agent_read_paths.<agent>]`, access
  is read-only, and every path is checked after resolving symlinks.
- The daemon now watches `config.toml` and reloads it when it changes, so a
  permission, grant, sandbox, limit, or taint edit applies to the next
  `lev run` with no restart. A half-written file leaves the last good config in
  place. Boot-time wiring (providers, MCP, telemetry) still needs a restart.
- Inference errors and iteration caps are written into the next stage's
  context instead of only the logs, preferring a pinned `error_report` region
  when the blueprint declares one, so a recovery stage no longer has to
  rediscover what went wrong.
- The empty-response nudge is now configurable per stage, per agent, and
  machine-wide through `[nudge]` (`enabled`, `max`, `text`, with `{stage}` and
  `{regions}` placeholders). A stage whose deliverable is prose can turn it off
  rather than being told to use tools it does not have.
- Tool batches are journaled at dispatch and each call as it completes, so a
  daemon that dies mid-batch replays the results it already has instead of
  re-running the calls. Anything that never finished comes back as an
  interrupted result the model is told to verify first.
- Completion webhooks now carry a stable delivery id, so a receiver can
  deduplicate retries of the same delivery.

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
