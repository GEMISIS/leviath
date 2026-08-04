# Changelog

Notable changes to Leviath. Versions follow [semver](https://semver.org); the
workspace publishes in lockstep, so one version covers every `leviath-*` crate
and the `lev` binary.

Release binaries ship through the alpha, beta, and stable channels described
in [the release docs](https://leviath.dev/docs/releases); each versioned
GitHub release also carries auto-generated notes listing the merged pull
requests since the previous version. A channel publishes only when the version
below it has moved, so the headings here and the releases on GitHub are the
same list.

## Unreleased

## 0.2.0 - 2026-08-04

- Windows no longer flashes console windows across the desktop. Every child
  process Leviath starts is a console application, and one started by a process
  with no console of its own gets a fresh window on the interactive desktop.
  With a `shell` call or two per agent iteration that is a strobe, and a fleet
  of agents made it worse. Every spawn whose output is already piped or
  discarded now asks for no window: the `shell` tool, a script tool's `shell()`,
  seed commands, container lifecycle commands, MCP servers, the Claude Code
  provider, the browser launcher, the dashboard's clipboard helper, and the
  daemon itself. The editor `lev run` opens for you is deliberately left alone,
  since it is the one child meant to be seen. Nothing about output capture
  changes.
- Which shell a command runs in is now decided by a function that takes the
  platform as an argument rather than by a compile-time branch, so the Windows
  answer is checked on every CI machine instead of only on the Windows one.
  Behaviour is unchanged: `cmd.exe /C` on Windows, `$SHELL` then bash, zsh, sh
  for the `shell` tool elsewhere, and always `/bin/sh -c` for script tools.
- OpenRouter works end to end. Several separate faults added up to an install
  that was configured correctly and still did nothing useful:
  - `default_provider` is now honoured. It was only consulted after every
    registered entry a blueprint listed, and the bundled agents all list
    Anthropic, OpenAI and Ollama, so setting it to `openrouter` changed
    nothing. Registered candidates on your default provider now head the
    stage's list, with the blueprint's own entries kept behind them as
    fallbacks. A stage pins its own provider with `allow_user_default = false`,
    which suppresses this as it always did.
  - A provider that cannot be reached at all now counts as unavailable, so the
    run fails over instead of dying. Ollama registers with no key whether or
    not a server is running, so a refused connection to `localhost:11434` used
    to kill runs at iteration 0 with a working provider sitting unused behind
    it in the same list.
  - Reasoning models no longer answer with nothing. They return `content: null`
    and put their text under `reasoning`, which reached the runtime as an empty
    response: the agent was nudged to use its tools, looped, and the run
    finished having said nothing. The field is read when the message carries no
    content and no tool calls, so it never displaces real output.
  - An error a gateway delivers with a 200 status is reported. OpenRouter
    answers `{"error":{...}}` with a success status when an upstream provider
    rejects a request it had already accepted, and that read as
    "No choices in response", throwing away the only text that said why. The
    envelope's own status code is classified as a real one, so a 402 arriving
    this way fails over and trips the circuit breaker like any other.
  - Errors delivered mid-stream surface instead of silently truncating the
    stream.
  - Requests carry the `X-Title` header OpenRouter pairs with `HTTP-Referer`,
    so calls are attributed to Leviath on the account's activity page.
- A hand-written `config.toml` parses. Every field on the top-level config was
  required, so the three lines that point Leviath at OpenRouter failed with
  ``missing field `providers` `` - a table the user has no reason to know
  about, in a message that says nothing about what to add.
- `lev serve` gained three read-only routes, so a browser front end can show
  what a run produced without shell access to the host. All three work with the
  daemon down.
  - `GET /api/agents/{id}/files?path=` returns one file the run wrote. The path
    may be relative to the run's working directory or absolute, but either way
    the resolved path has to land inside that directory, under the same
    symlink-aware containment the file tools use, so the endpoint reads exactly
    what the run was allowed to write. Reads stop at 1 MiB and say so; a cap
    that lands mid-character drops the split character rather than calling a
    text file binary.
  - `GET /api/doctor` runs the checks `lev doctor` runs and returns them as
    data. A failing check is an `ok: false` entry in a 200, never an HTTP error.
  - `GET /api/fs/dirs?path=` lists one directory level of subdirectory names,
    so a folder picker can offer a working directory instead of asking someone
    to type one blind. Paths must be absolute, `--workdir-root` fences it the
    same way it fences spawning, and `parent` is null at the fence so the
    picker is never offered a step above it. Add `hidden=true` for
    dot-prefixed names.
- `lev doctor`'s `resolve` check says when your configured `default_provider`
  is being passed over, and why. `default_provider` with no `default_model` is
  a half-configuration that silently does nothing, and the check used to report
  `OK` next to a provider you never asked for.

## 0.1.2 - 2026-08-02

- `lev run <agent>` with no `--task` now opens your editor on a commented
  template instead of refusing to start, so a task longer than a sentence no
  longer has to survive shell quoting. Saving an empty file cancels the run.
  Stdin still has to be a terminal: a script or CI job without `--task` gets an
  error, now worded to say why the editor cannot be used. The editor is
  `$VISUAL`, then `$EDITOR`, then the first installed of `vim`, `nano`, `vi`
  (`edit`, `notepad`, `vim` on Windows).
- `lev run .` and `lev run ./some-agent` work. The blueprint path was sent to
  the daemon exactly as typed, and the daemon resolved it against its own
  working directory, so a relative path failed with "read manifest
  './agent.leviath': No such file or directory". It is now resolved before the
  request leaves. This is the command `lev create` prints as your next step.
- `lev run` with no PATH uses the current directory, which is what the CLI
  reference has always described. It used to be an error.
- `--task` reads a file when the value names one. A value that looks like a
  path but names nothing is now an error rather than being sent to the agent as
  the prompt, which is what a mistyped filename used to become. Prompt text is
  unaffected: the check only fires on a value with no whitespace that carries a
  `/`, a `\`, or a leading `~`.
- A run stays in `lev ps` for five minutes after it ends instead of vanishing
  when the daemon unloads it. A run that died on its first inference used to
  leave the listing a second or two later, which made it indistinguishable from
  a run that had never been spawned: both read as `no agents running`. Anything
  scheduling work by spawning agents then had to guess how long a healthy agent
  takes to get going, and a guess that came in under a cold start would abandon
  runs that were still starting. The row now carries the status the run ended
  on, so an `HTTP 402` at iteration 0 says so. Tunable with
  `[limits] finished_retention_secs`; `0` restores the old behaviour. The record
  is in memory, so a restart clears it, and `meta.json` and `GET /api/agents`
  remain the durable copy.
- `lev ps --json` gained a `finished` key alongside `runs` and `health`.
  Finished runs are kept apart rather than mixed in, so `lev daemon status` and
  the dashboard still count only the agents the daemon is hosting.
- `meta.json` now records `last_progress_at`, the moment a run last actually
  moved. `updated_at` cannot answer that and never could: it advances on a
  30-second heartbeat whether or not anything happened, so that a stale
  timestamp means the daemon stopped rather than the run. Anything outside the
  daemon that aged a run on `updated_at` was reading a signal that stays fresh
  on a run which has stopped dead.
- `RunMeta.pid` is documented as what it has always been: 0 for every run, live
  or finished. There is no process per run, so nothing can be concluded from
  it, and a sweeper that reverted work on `pid == 0` reverted all of it. Left
  in place for compatibility; it is a candidate for removal in the next major.
- New `lev ps --all`, listing the runs on disk that the daemon is not hosting,
  read from the runs dir rather than the daemon's memory. The retention window
  above covers the minutes after a run ends; this covers the rest of time, and
  survives a restart, which is what a scheduler reconciling its own queue needs.
  Rows that claim on disk to be running while nothing drives them are marked
  `(abandoned)`. With `--all`, a daemon that is down is reported rather than
  fatal, and marks nothing abandoned, because a restarting daemon looks exactly
  like every run dying at once.
- New `[limits] wedge_timeout_secs`: fail a run that has ended up in a state no
  part of the engine can reach, rather than leaving it reported as running for
  the life of the daemon. It never fires on a run that is merely slow; an agent
  waiting on the model, on a tool, on its sub-agents, or on a person is exempt
  however long it takes. Off by default, since it fails runs. Together with the
  above this is what stops an external scheduler leaking slots to runs that
  have quietly stopped, and there is now a page on doing that reconciliation
  properly in the daemon docs.
- New `lev doctor`, which checks that provider wiring works without you having
  to build a throwaway agent to find out. Four checks run in order and each is
  reported: the config file parses and a registry can be built, your defaults
  resolve to a provider that is actually registered, one real inference reaches
  the model, and a one-turn agent spawns over the control socket and finishes.
  The check that fails is the diagnosis. That last one matters most: config,
  resolve and inference passing while `daemon` fails is the difference between
  "my keys are wrong" and "the daemon is wedged", which used to look identical
  from the outside.
- `lev doctor` prints the provider and model it actually resolved to, not just
  "OK". A stage that names no model of its own falls back to `anthropic`, so a
  machine holding only an OpenRouter key can resolve to a provider it has no
  credential for, spawn, and sit at iteration 0 — which is how a batch of runs
  once went nowhere at once. Now it says so, before anything is spawned.
- A failing provider call is reported verbatim, status line and response body
  included, so a 402 naming the exhausted credit or a 404 naming the model
  reads as itself rather than as "inference failed". `--model provider/model`
  tries a model string before you wire it into a blueprint, and is the only way
  to reach a Rhai script provider, which is resolved by name and cannot be
  listed. `--no-daemon` stops after the inference; `--json` prints the checks
  for scripts; a failure exits non-zero, so it works as a CI gate.
- The probe cleans up after itself: the throwaway agent is staged in a temp
  directory and its run is deleted on every path out, including the failing
  ones, so nothing is left in `lev ps` or on disk.
- A provider that runs out of credits no longer takes every agent down with it.
  A `402` arrived as an opaque API error carrying the raw JSON body, so the
  runtime had nothing to branch on and each run died at iteration 0 with the
  blob as its status. Out-of-credits, rejected-key, and not-permitted responses
  are now told apart from an ordinary bad request, including the ones that
  arrive under an innocent status (Anthropic reports a drained balance as a
  `400` saying the credit balance is too low). The message says what to do about
  it and keeps the provider's response for the logs.
- A stage now fails over instead of failing. Its ordered `models` list was only
  ever consulted once, at spawn, to pick the first provider with a key; a
  provider that was configured but unusable was chosen and then never abandoned.
  The rest of the list is kept and used. An ordinary error still cannot spend a
  fallback, and a stage that exhausts its list ends as before, with a readable
  message.
- New `[providers] fallback_order`, a host-wide list of `provider/model` pairs
  tried after a stage's own entries and the default model. A blueprint that
  names a single model has nowhere to go without it. It is per-run policy, so it
  reloads with no daemon restart.
- Providers that keep failing are taken out of service. Failing over rescues one
  run; the next one would start on the same dead provider and rediscover it.
  After `[limits] provider_failures_before_open` consecutive failures (default
  3, since a single payment error can be one oversized request) no run is
  dispatched there. `provider_circuit_cooldown_secs` (default 300) later lets
  one request through as a probe, so topping up an account brings the factory
  back with no restart. Runs with no candidate left are failed with an
  explanation rather than left running forever.
- `lev ps` names any provider currently out of service, with the reason and the
  retry countdown; `lev ps --json` carries it under `health.providers_down`. New
  `leviath.provider.circuit.open` and `leviath.provider.circuit.opened.total`
  metrics report the same per provider. Ten runs dying in a row used to produce
  ten identical error rows and nothing that said the account was empty.
- Anthropic and Ollama now classify HTTP failures through the same shared path
  as every other provider. Both had hand-rolled copies; a side effect was that
  `list_models` reported a rejected API key as a request failure, which reads as
  a transient network fault worth retrying. Ollama also gains the `429` handling
  it never had.
- `lev validate` now checks the things a blueprint leaves unsaid, not just the
  ones it gets wrong. A stage with no `[stages.X.model]` block parsed fine and
  then ran on whatever the user's `default_provider` was; an agent-level
  `[model]` block was read by nothing at all; a typo in `available_tools`
  matched nothing, so the stage quietly advertised one tool fewer and the model
  was told the tool did not exist. Each of those was invisible on inspection and
  turned up hours later as a run behaving oddly. There are thirteen checks in
  all, each with a stable code and a suggested fix.
- Typos are errors and exit non-zero; everything else is a warning that does
  not, or a note that never can. `lev validate --deny-warnings` makes warnings
  fatal for CI.
- The same findings are logged when the daemon spawns a run, so a blueprint
  nobody validated still says what is wrong with it in `daemon.log`. No finding
  refuses a spawn.
- `lev validate` also warns when an autonomous stage grants `ask_user_text`,
  `present_for_review` or another tool that suspends until a person answers.
  Unattended, the run parks there until it is killed. New stage key
  `allow_blocking_tools = true` records that the stage means it; it grants
  nothing and changes no behaviour.
- The lint found two defects in the shipped blueprints. `parallel-fixer` set
  `bash = "ask"` while every stage granted `shell`: policy is matched on the
  name the model calls, so the entry was never consulted. And
  `software-engineer`'s review stage had no `max_iterations`.
- `POST /api/blueprints/validate` returns a `warnings` list alongside `errors`.
- An unattended run no longer gets the tools that wait on a person.
  `ask_user_text`, `ask_user_choice`, `ask_user_confirm`, `present_for_review`,
  and `edit_document` do one thing: open a prompt and block. Under `--yolo`
  nobody answers, so a call to one used to park the agent in `WaitingInput`
  until the daemon restarted; six production runs sat there for three to five
  hours each, holding their slots. They are now dropped from the tool set the
  model is offered, per stage, before the first inference. The model never sees
  them and decides for itself instead of spending a round trip to be told nobody
  is there.
- A stage that genuinely needs a person opts out with `required_tools`, listing
  the human tools it keeps even when the run is unattended. Entries must also
  appear in `available_tools`, and a manifest where one does not is rejected
  rather than quietly ignored.
- Interaction points gained the same escape hatch. `unattended = "ask"` on a
  point holds the run for a real answer under `--yolo` instead of approving
  itself. The bundled `software-engineer` uses it for plan approval: everything
  after that gate writes code, so waving it through unread is the one thing that
  agent should not do on its own.
- New `[limits] interaction_timeout_secs`, one hour by default, puts a deadline
  on any prompt that waits on a person: `ask_user_*`, tool approvals, taint
  gates, and interaction points alike. There had never been one. Expiry resolves
  the prompt exactly as cancelling it does, so an approval and a taint gate both
  deny, the model is told no answer came, and a checkpoint proceeds with no user
  text. A timeout is never read as consent. Set it to `0` to wait indefinitely.
- `lev validate`'s `blocking-tool-in-autonomous-stage` warning now takes
  `required_tools` as the answer it is asking for. Keeping a tool says the same
  thing `allow_blocking_tools` says, one tool at a time, and says it about the
  run as well as the manifest.
- A blueprint's `[read_paths]` declaration now says whether your config
  actually grants it. Declaring a path outside the workdir has never been the
  same as being allowed to read it, but nothing said so: `lev validate` printed
  "valid", `lev list` printed the agent, the run spawned, and the first read
  outside the workdir was refused with no earlier sign that a config grant was
  the missing piece. That was fine on the machine whose config happened to have
  the grants and a mystery on every other one. `lev validate` now checks each
  declared entry against your `config.toml`, names the ones nothing grants, and
  prints the `[agent_read_paths.<agent>]` block that would fix it. `lev run`
  repeats that warning where the person running the agent can see it, rather
  than only in the daemon's log. `lev list` shows the counts per agent, `lev ps`
  grows a `READS` column reading granted over declared (and only when some run
  declares any), and `lev add` reports the status of what it just installed.
  The check compares patterns rather than touching the filesystem, so a grant
  naming a directory that does not exist yet still counts; an individual read is
  still matched against the real, symlink-resolved path when it happens.
- A run is no longer reported as having produced nothing when it never had a
  way to produce anything. `empty_output` in `meta.json` has meant "modified no
  files" since it was added for coding agents, so a router that delegates to
  sub-agents, or an agent whose answer is its text, was flagged on every
  successful run. Blueprints that advertise no file-modifying tool at any stage
  are now exempt, matching the escape a transition gate already makes for a
  stage that could never satisfy it. Agents that can write are judged exactly as
  before, `shell` included: edits made with `sed -i` still leave no record, and
  a run that made only those is still reported.
- That verdict is now visible. `lev ps` reads `complete (no output)`, the
  completion webhook carries an `empty_output` key, and the flag rides in
  `lev ps --json`. It had been written to disk and read back only on restart, so
  a run that finished with nothing to show for it looked exactly like one that
  worked.
- New `leviath.runs.total` metric, counting finished runs by terminal status and
  by whether they produced output, so the empty-run rate can be charted and
  alerted on.
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
- Releases are cut by a version bump rather than by a schedule. Alpha now
  publishes as soon as a commit bumping `[workspace.package] version` lands on
  `main`, and beta and stable promote it on their usual weekly cadence; a
  channel with nothing new finishes in seconds having published nothing. That
  ends the nightly churn of rebuilding identical source and re-promoting an
  already-promoted build, and with it the `vX.Y.Z+date` tags that existed only
  to avoid colliding with a version already released.

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
