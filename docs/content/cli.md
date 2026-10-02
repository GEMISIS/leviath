---
title: CLI reference
description: Every lev command and flag, which ones speak --json for scripts and CI, and how to read lev ps status and wait reasons.
group: Reference
group_order: 3
order: 3
---

# CLI reference (`lev`)

Everything Leviath does is one binary, `lev`. This page lists every command and its flags.
`lev <command> --help` prints the same thing at the terminal.

If a command is not doing what you expect, [Troubleshooting](/docs/troubleshooting) is organised by
symptom, and `lev doctor` checks the usual causes for you.

`-v` / `--verbose` is global and works on every subcommand.

Scripting against the CLI? `--json` is on `run`, `ps`, `doctor`, `validate`, `list`, `models list`,
`context`, `result`, `respond`, `stages`, `timeline`, `tools`, `update`, `approvals safe`, and `mcp
list`. Everything else prints
for a person. Warnings go to stderr, so stdout parses on its own. A service that would rather
speak HTTP should use [`lev serve`](/docs/api) instead.

Most commands talk to the [shared-world daemon](/docs/daemon). `lev run`, `lev dash`, `lev serve`,
and `lev agent-client` start one automatically if none is running, and restart it if it is running
an older build.

## Running agents

### `lev run [PATH]`

Start a run in the daemon. `PATH` is an installed blueprint name, a blueprint directory, or its
`agent.toml`. Omitted, the current directory is used. Every flag below lands on one
[spawn request](/docs/starting-a-run#one-request-every-front-door), the same request every other
front door sends.

```bash
lev run coder --task "Add retry with backoff to the HTTP client"
lev run release-notes --input since=v0.6.0 --audience developers --max_items 10
lev run release-notes --since v0.6.0 --check
lev run --request nightly.toml
```

| Flag | Purpose |
|---|---|
| `-t`, `--task <TEXT\|FILE>` | The task prompt, or the path of a file holding it: the blueprint's `task` input. Left off, your editor opens |
| `-i`, `--input <NAME=VALUE>` | Give the run an input, read by the type the blueprint declares for it. Repeatable. See below |
| `--<name> <VALUE>` | The short form of `--input <name>=<value>`, for any input |
| `--request <FILE>` | Send a whole spawn request from a TOML or JSON file instead of naming a blueprint. See below |
| `--check` | Resolve the run the whole way without starting it. See below |
| `-m`, `--model <MODEL>` | Model override for the whole run, as `provider/model` or a bare model name. Fan-out workers and sub-agents inherit it |
| `--workdir <DIR>` | Working directory for the run, defaulting to where you ran the command. See below |
| `--yolo[=PROFILE]` | Run unattended, or under a named profile from `yolo.toml`. The equals sign is required. See below |
| `--allow <TOOL>` | Allow one tool outright. Repeatable |
| `--max-depth <N>` | Override the blueprint's maximum sub-agent tree depth |
| `--no-seed-commands` | Refuse the blueprint's `seed = { command = "..." }` regions for this run |
| `--count <N>` | Start this many runs of the same agent and task, each under its own run id, from one invocation |
| `--json` | Print the spawned run as JSON rather than a sentence. See below |
| `--output-format <LABEL>` | Ask for the final output in this shape. See [Final outputs](/docs/outputs) |
| `--output-instructions <TEXT>` | Extra guidance about that shape |
| `--output-schema <JSON\|@FILE>` | A JSON Schema the final output must satisfy |
| `--attach <PATH[:REGION][:TYPE][:text|native|stand_in]>` | Put a file in a region as a typed part. Repeatable. See below |

**`--workdir`** decides more than where commands run. File tools are confined to it, and relative
`read_paths` entries resolve against it.

**`--json`** is for a caller that parses the run id back out. With `--count` above 1 it prints an
array, one object per run. With `--check`, the JSON is the summary or the list of problems.

**`--output-format`** with a label that differs from what the blueprint declares retires its Rhai
validator and JSON schema, and says so with a warning on stderr.

**`--yolo`** waives approvals, not checkpoints. It approves every tool call, and it takes away the
tools that wait on a person (`ask_user_*`, `present_for_review`, `edit_document`) so the run does
not stop for somebody who is not there.

Two things still hold it. A stage keeps whatever it lists in
[`required_tools`](/docs/tools#these-tools-need-someone-there), and an
[interaction point](/docs/interaction#interaction-points) declaring `unattended = "ask"` opens its
prompt however the run was launched. A blueprint can ask for that on its plan
approval, because everything after that gate writes code. `lev run --yolo` prints what will hold
before the run starts, and `lev validate` reports it as `holds-under-yolo`.

`--yolo` can turn an `ask` into an `allow`, but it can never lift a `deny`.

**`--yolo=<profile>`** is `--yolo` taken apart. A profile in [`yolo.toml`](/docs/yolo)
says which tool calls and shell commands run unprompted, which still go through the ordinary
approval prompt, and which are refused. It also says whether the model's questions, the stage
checkpoints and the taint gate still come to you. The equals sign is required: `lev run --yolo coder` keeps
meaning "run coder, plain yolo". A name the file does not have stops the run before the daemon is
asked, and lists the names it does have. Before a profiled run starts, `lev run` prints what the
profile keeps for you, above the blueprint's own held checkpoints. `lev yolo list` shows the
profiles you have.

#### Inputs

A blueprint declares the values a caller gives it as [inputs](/docs/starting-a-run#inputs), each
with a type. `--input name=value` gives one, and `--<name> value` is the short form for any input
the blueprint declares. Run `lev validate` on the blueprint's directory to see which inputs it takes.

```bash
lev run reviewer --diff @changes.patch --criteria @./team-standards.md
lev run release-notes --input since=v0.6.0 --input max_items=10 --input audience=developers
```

`lev run` reads each value by the type the blueprint declares for it:

| Declared type | On the command line |
|---|---|
| `text` | The text, or `@file` for a file's text |
| `bool` | `true` or `false` |
| `int`, `float` | A number |
| `list` | `a,b,c`, or a JSON array |
| `record` | A JSON object |
| `file` | `@path`, which is attached and named for the input |
| anything else | The text, which the daemon checks |

Every problem is reported at once, each on its own line with the input's path. A name the
blueprint does not declare and a value that does not read as its type fail before an editor
opens:

```
$ lev run release-notes --input since_tag=v1 --max_items twenty
Error: 2 problems with this run:
  inputs.since_tag: unknown: --input names no input this run takes. Known: since, audience, max_items
  inputs.max_items: wrong type: this is not a whole number (expected an integer 1 to 50; got "twenty")
```

The daemon then checks what reads but does not fit, such as a missing required input or a choice
that is not one of the options. See [reading the problems](/docs/starting-a-run#reading-the-problems).

`--task` gives the input named `task`, which by convention fills the `task` region. A blueprint
that declares no `task` input refuses `--task`, and the message lists the inputs it does take.

A `@file` given to a text input that is not text (an image, a recording, a PDF) is attached to the
input's region as a typed [part](/docs/mime) instead of being read as text. `--attach` puts a file
in any region the blueprint declares, and says more about the file when the name alone does not:

```bash
lev run storyteller --task "a 30 second trailer" \
  --attach voice.wav:voice_samples --attach frame1.png:storyboard
lev run modeller --attach scene.bin:props:model/gltf-binary
lev run reviewer --task "does @mockup.png match the brief?"
```

The segments after the path are told apart by shape. `type/subtype` declares the mime type when
the registry cannot tell from the bytes or the extension. `text`, `native` or `stand_in` chooses how
the part reaches the model, and anything else names the region. Left off, the region is the one
the task lands in. A `@path` inside the task text or another text input attaches that file to the
same entry, and the text keeps the `@path` as written so the model reads the same name the part
carries. Paths resolve from where you ran the command. Write `\@` for a literal `@`; a token that
names no file is left as text, and `lev run` warns when it looked like a path. A part bound for a
region the blueprint does not declare, or whose declared type the region's `accepts` excludes, is
refused before the run starts.

#### Writing the task in your editor

Run `lev run <blueprint>` with no `-t` and Leviath opens your editor on a short commented template.
Type the task, save, and the run starts. Lines beginning with `#` are stripped, so none of the
template reaches the run. Save an empty file and the run is cancelled. No editor opens when the
command line already carries another input (`--diff @x.patch`) or an attachment, and the
blueprint's `task` input is not `required`: that run starts with no task. No editor opens for a
blueprint that declares no `task` input, or with `--check`.

The editor is `$VISUAL`, then `$EDITOR`, then the first of `vim`, `nano`, `vi` that is installed.
On Windows it is `edit`, then `notepad`, then `vim`. `$VISUAL` and `$EDITOR` are split on
whitespace, so `code --wait` works, but a program path containing spaces needs a wrapper script on
your PATH.

Stdin has to be a terminal for any of this. In a script, a pipeline, or CI, pass `-t` and Leviath
says so rather than blocking.

`-t` reads a file when the value names one that exists. A value that looks like a path with no
file behind it is an error, so a mistyped filename fails instead of quietly becoming the prompt.
"Looks like a path" means no spaces, plus a `/`, a `\`, or a leading `~`. Other text inputs work
the other way round and want an explicit `@` before a path, because their value is often a file
while a task is usually a sentence.

#### A whole request from a file

`--request <FILE>` sends a whole spawn request, TOML for a `.toml` file and JSON otherwise, instead
of naming a blueprint. Its `source` names an installed blueprint, a blueprint directory, or a whole
raw graph. Any input or launch flag given beside it lands on the request too, over what the file
says. `lev schema spawn-request` prints what a request may hold, and unknown keys are refused.

```toml
# nightly.toml
source = { blueprint = { name = "release-notes" } }
model = { provider = "anthropic", model = "claude-sonnet-5" }
workdir = "/srv/repo"

[inputs]
since = "v0.6.0"
audience = "developers"

[launch]
unattended = "all"
```

```bash
lev run --request nightly.toml
lev run --request nightly.toml --max_items 5 --check
```

Naming a blueprint and passing `--request` together is refused, because a request names what it
runs. See [Starting a run](/docs/starting-a-run) for every field.

#### Checking a run without starting it

`--check` asks the daemon to resolve the run the whole way and stop there. It prints what would
run: each stage's model and tools, the inputs, the working directory. Or it prints every problem
it found, one per line with where in the request it is. Nothing is spawned and nothing is billed.

```
$ lev run release-notes --since v0.6.0 --max_items 10 --check
release-notes would run (blueprint release-notes@a4adce5110b52cc26e95bb6ef879985f2253229b85fcaa3b02842ab287dae61a, version 0.1.0)
  starts in: gather
  workdir:   /srv/repo
  unattended: no
  max depth: 3
  inputs:
    audience = users
    max_items = 10
    since = "v0.6.0"
  stages:
    gather  anthropic/claude-sonnet-5  tools: read_file, shell, context_append
    write  openai/gpt-5.5  no tools
```

A missing task is reported with the other problems rather than opening your editor. With
`--json`, the summary or the list of problems is JSON. See
[check before you start](/docs/starting-a-run#check-before-you-start).

### `lev run show <RUN>`

Show what a run's [file](/docs/run-file) holds. `RUN` is the run's id, as `lev ps` lists it. With
no flag it prints the run's spec: the graph it runs, its inputs, and the machine it was bound to.

| Flag | Purpose |
|---|---|
| `--at <SEQ>` | Show the run's state after step `SEQ` instead of its spec. Step 0 is the state it started in |
| `--deltas <FROM..TO>` | Show the steps from `FROM` to `TO`, both included, instead of the spec |
| `--toml` | Print TOML (the default) |
| `--json` | Print JSON |

`--deltas` takes `3..7`, `3..` for every step from 3 on, `..7`, or `..` for them all. Run
`lev run show <run> --deltas ..` to list the steps there are. What may keep the run from ever
finishing is printed on stderr first. The JSON spec also carries it as a `warnings` list, one line
each, as `lev run --json` does.

```bash
lev run show release-notes-1790848768-495da0e423db
lev run show release-notes-1790848768-495da0e423db --at 12 --json
lev run show release-notes-1790848768-495da0e423db --deltas 3..7
```

It reads the file on disk, so it needs no daemon. See [Inspecting a run](/docs/inspecting-a-run)
for the other ways to read the same things.

### `lev schema spawn-request`

Print the JSON Schema of a spawn request: every field a `--request` file, a `POST /api/runs` body or
a GraphQL `spawnRun` may carry.

```bash
lev schema spawn-request > spawn-request.schema.json
```

The daemon serves the same schema at `GET /api/schema/spawn-request`.

### `lev stages <RUN-ID>`

The per-stage ledger, which is where a staged agent's cost lives. A single loop has one
number you can eyeball; a staged agent has a different window per stage, regions that persist
across stages, and per-stage models with different prices.

| Flag | Purpose |
|---|---|
| `--regions` | Also show each stage's per-region token high-water mark, largest first |
| `--visits` | Break each stage into its stays, so a stage entered twice is two rows |
| `--json` | Print the ledger as JSON |

```
STAGE                STATUS         PROMPT     OUTPUT   CACHE RD   CACHE WR        COST
ingest               complete        16832       2249          0          0    ~$0.0891
report               complete        37644        493          0          0    ~$0.1932
summary              complete       252848        648          0          0    ~$1.2812
TOTAL                               307324       3390          0          0    ~$1.5635
```

`CACHE WR` is the write half of a cache decision. Without it a stage showing no reads might be
paying to write a prefix nothing reuses, or might not be caching at all, and the ledger could not
tell those apart.

`COST` is what that stage spent, and it is deliberately not a number you can always get. A leading
`~` means the figure was reconstructed from published rates rather than read off the provider's own
answer. A bare `?` means at least one call in that stage could not be priced at all, by either
route. A `?` is not a zero: a stage whose calls went unpriced has not been shown to be free, and one
unpriced stage takes the `TOTAL` with it for the same reason.

Every call a run bills counts against the stage it was made in, including the compaction calls that
summarize a full window and the routing call a stage makes at its own boundary. The exception is the
run's title call, which happens once at spawn beside the run rather than inside any stage, so this
column can sum to slightly less than the run's own figure.

`--visits` splits a stage by each stay in it. The row above is the sum across every visit, which is
the right total for the stage. It is the wrong number to put on a graph of the path the run took,
where a stage entered twice is two nodes. Leviath records up to 128 stays per stage; past that the split
stops and the table says so, while the stage's own row keeps counting.

`--regions` answers the question a structured layout is really asking: what am I paying to carry,
and where. The number shown is the largest each region reached while the stage was active, since a
region is re-sent whole on every call.

Leviath also warns once when a stage's per-call prompt grows past four times its first call. That
is the shape of a region accumulating without a cap, which is the failure that costs money and
the hardest one to spot by eye.

### `lev timeline <RUN-ID>`

Where a run's wall-clock time went. `lev stages` says what each stage cost; this says what the
run was doing for an hour. Every number comes from the journal (`run.lvr`), which timestamps each
model call, tool batch, tool result and status change, so the split is exact.

| Flag | Purpose |
|---|---|
| `--calls` | List every model call: when it started, how long it took, prompt, cached and output tokens |
| `--tree` | Include the run's children (and theirs), one line per run. See below |
| `--json` | Print the same data as JSON |

```
deep-researcher-1787779292 (deep-researcher, complete) wall 1:17:02 = model calls 46:55 + tools 0:35 + waiting on children 29:59 + other 0:00

STAGE                MODEL TIME  CALLS     OUTPUT    LARGEST
(title)                    0:03      1         30         30
gather                     6:14     12      19330       7879
polish                    19:59      6     116670      23996
```

A model call's time is measured from the moment the run had nothing else in flight, so it
includes any wait for an inference slot. That is deliberate: the run experienced the queue as
latency, and `--tree` shows which model was oversubscribed. It also reports the peak number of
calls per model in flight at once.

The command warns about one shape it recognises: several back-to-back replies in the same stage,
each thousands of tokens and all the same size. That is what a reply cut off by the stage's output
cap and retried looks like, and it is easy to miss in a column of token counts.

### `lev create <NAME>`

Scaffold a new [blueprint](/docs/agents) directory.

| Flag | Default | Purpose |
|---|---|---|
| `-t`, `--template <NAME>` | `default` | Starting template. `coder` scaffolds the multi-stage shape; anything else gives a single-stage starting point |

### `lev blueprint migrate <PATH>`

Convert an `agent.leviath` manifest into an `agent.toml` blueprint. `PATH` is the manifest, or the
directory holding one. With no flag the blueprint is printed, so you can read it first.

| Flag | Purpose |
|---|---|
| `-o`, `--output <FILE>` | Write the blueprint here instead of printing it. An existing file is left alone unless `--force` is given |
| `--force` | Overwrite the `--output` file when it already exists |

```bash
lev blueprint migrate ./my-agent                          # print the agent.toml
lev blueprint migrate ./my-agent -o ./my-agent/agent.toml # write it beside the manifest
lev validate ./my-agent/agent.toml
```

A key Leviath 0.6.4 and earlier accepted but never read is left out, with a `warning:` line on
stderr naming the blueprint, the key and its value.

A region the manifest seeds with a key such as `seed = "diff"` becomes an input named `diff`, bound
to that region. See
[migrating from agent.leviath](/docs/blueprint-format#migrating-from-agentleviath) for how each key
is written in `agent.toml`.

### `lev validate [PATH]`

Check a blueprint before running it. `PATH` is a blueprint directory or its `agent.toml`, and
defaults to `.`. It reads the blueprint the way a spawn would, then prints what it is, the inputs
it takes and the shape of its graph:

```
$ lev validate ./release-notes
✓ Blueprint 'release-notes' is valid.
  2 stages, version 0.1.0
  Inputs: --since (text, required, seeds region 'brief'), --audience (one of "users", "developers"), --max_items (an integer 1 to 50)
  Note: this agent takes no --task; give it input via --since, --audience, --max_items
  Entry stage: 'gather'
  - gather → write
  - write (terminal)
  WARN stage 'write': no max_iterations, so the stage is unbounded unless your config sets [limits] default_max_iterations [stage-missing-max-iterations]
       give the stage a max_iterations it should never reach
  NOTE 1 region(s) run a shell command at spawn, before the first inference and before any tool-approval prompt: commits: git log --oneline -200 (pre-approved) [command-seed]
       a refused seed needs its programs in the graph's safe_commands shell list; disable seeds entirely with `--no-seed-commands`, or machine-wide via `[security] allow_seed_commands = false`
```

A blueprint that does not hold together fails with every problem at once, one per line with its
path in the file:

```
Error: ✗ Validation failed: ./release-notes/agent.toml has 2 problem(s):
  graph.edges[0].to: dangling reference: no stage is named "wrtie". Known: gather, write
  graph.inputs.since.binds[0].region: dangling reference: no region is named "brif". Known: brief, commits, conversation, notes
```

Every table refuses keys it does not know, so a misspelled key is a parse error that names it
rather than a setting quietly ignored. See [the blueprint format](/docs/blueprint-format) for every
key it checks.

Every piece of code the blueprint names is compiled too, exactly as a spawn would compile it. A
custom region script, an output validator, or a stage hook script that is missing or will not load
fails the command, since it would fail the run. The agent's own `tools/*.rhai`, your global script
tools and your script providers are compiled as well, and one that will not load is named as a
warning.

And because a clean verdict against a config the daemon would grumble about is worth less than it
looks, the command reads your `config.toml` on the way past. Keys nothing reads are named as
warnings, usually a typo'd setting. A `[model_providers.*]` script entry whose `.rhai` file is not
on disk is named along with the path that was looked for. With a config that loads, the report
also says which model each stage would run on here, and whether this machine meets the blueprint's
declared dependencies.

Beyond parsing and structural validation, `lev validate` lints the blueprint: it reports what the
blueprint leaves unsaid. Findings come in three levels: an **error** exits non-zero, a **warning**
does not, and a **note** never does. Each finding prints with its code in brackets.

| Level | Code | What it means |
|---|---|---|
| error | `unknown-tool` | A name in a stage's `tools` matches nothing. See below |
| error | `unparseable-safe-command` | A `graph.safe_commands` shell entry no call can ever match. See below |
| error | `output-missing-submit-tool` | A stage must produce an output and has no way to submit one. See below |
| error | `output-stage-cannot-answer` | An output stage whose models cannot call tools, with no file to hand back. See below |
| error | `output-unreachable` | An output stage that no edge routes to, so the run can never produce one |
| error | `orphan-stage-permission` | A stage's `tool_permissions` key names a tool the stage never granted. See below |
| error | `required-tool-not-granted` | A `required_tools` entry no name or group in `tools` reaches. See below |
| error | `unserved-model` | A stage names a model the provider that would run it does not carry. See below |
| error | `fanout-worker-task-unheld` | A `fan_out` stage runs workers on this graph, which binds no input to a region. See below |
| error | `retention-not-zero` | `[providers] zero_retention` is on and the model this stage would start on keeps something. See below |
| warning | `retention-fallback-dropped` | `[providers] zero_retention` is on and a listed fallback keeps something, so failover skips it |
| warning | `stage-missing-model` | No `model`, so the stage runs on whatever your `default_provider` is |
| warning | `stage-missing-max-iterations` | Unbounded unless `[limits] default_max_iterations` is set. Fan-out stages are exempt |
| warning | `blocking-tool-in-autonomous-stage` | An autonomous stage grants a tool that waits for a person. See below |
| warning | `implicit-shell-policy` | A shell grant with no policy behind it. See below |
| warning | `blueprint-permission-clamped` | A `tool_permissions` entry looser than the tool's built-in default. See below |
| warning | `routing-without-region-read` | Tool output routed to a region, with `read_file` granted but not `context_read`. See below |
| warning | `output-shape-not-required` | A stage declares an `output` shape but is not required to produce one |
| warning | `output-stage-can-modify` | An output stage grants `write_file`, `edit_file`, or a group that holds them |
| warning | `allow-complete-skips-output` | A stage with `allow_complete` may end the run before the output stage. See below |
| warning | `unknown-model` | A model this build has not heard of. See below |
| warning | `catalog-unchecked` | A script provider that will not say which models it serves. See below |
| warning | `no-reachable-provider` | Nothing in the stage's models list can run here. See below |
| warning | `compact-summarizes-deliverable` | A `compact` edge would hand a `required` region to the summarizer. See below |
| warning | `required-region-unenforceable` | A `required` region no stage using it can write to. See below |
| warning | `unbounded-percentage-budget` | An evicting region whose percentage budget is huge on the widest model named. See below |
| warning | `unreachable-stage`, `cycle-without-max-revisits`, `broad-read-path` | Graph and `graph.read_paths` shape |
| warning | `dead-end-possible` | Every route out of a stage can run out of budget. See below |
| warning | `fanout-no-escape` | A `fail_all` fan-out stage with no `error` or `dead_end` edge. See [sub-agents](/docs/sub-agents) |
| warning | `read-paths-not-granted` | The blueprint declares `read_paths` your `config.toml` does not grant. See below |
| warning | `read-paths-grant-invalid` | A `read_paths` grant in your own config will not compile. It is a hard spawn error |
| warning | `tool-accepts-ungranted` | A `tool_accepts` limit on a tool the stage does not grant, so it never applies |
| warning | `mime-type-overrides-builtin` | A `graph.mime_types` row changes the family or text flag of a built-in type |
| warning, note | `mime-unseen` | A stage takes mime types its models cannot read natively. See below |
| note | `holds-under-yolo` | A checkpoint that still stops an unattended run for a person. See below |
| note | `long-context-price` | A stage's context can grow past the size at which its model bills at a higher rate. See below |
| note | `safe-commands-declared` | The blueprint declares `graph.safe_commands`. Declaring is not granting. See below |
| note | `command-seed`, `tool-seed`, `read-paths-declared` | Things worth knowing before you run the blueprint. See below |

Many of those findings need more than a phrase.

**`unknown-tool`** means the name matches no built-in, no sub-agent tool, and no `tools/*.rhai`
file. The stage then advertises one tool fewer, so the model is told a tool it was meant to have
does not exist. MCP names (`server__tool`) are skipped, since they resolve only once that server is
installed.

**`unparseable-safe-command`** fires on an entry that is not a bare command prefix, so no call can
ever match it. Write a program, optionally with the subcommand that narrows it: `rg`, `cargo test`.

**`output-missing-submit-tool`** means a stage sets `require_output` but never grants
`submit_output`. Use `mode = "output"`, which grants the tool.

**`output-stage-cannot-answer`** is about a stage whose every model cannot call tools, such as an
image or 3D model, so `submit_output` is out of reach. Such a stage answers only through a file.
Declare it under the stage's `output.artifacts` and route the model's part into a region with
`output_routing`, or list a model that calls tools.

**`orphan-stage-permission`** names a tool the stage never granted, by name or through a group. The
key reads as a grant and is not one. A stage that grants a whole MCP server through `connectors` is
not checked, since its tools are unknown until the server answers.

**`required-tool-not-granted`** is only checked when a group is in play. Without one, a spawn
refuses a required tool the stage does not grant.

**`unserved-model`** is the one model finding that fails the command, because it is the one that can
be proved. The provider is configured here, it published the full list of what it carries, and the
model the stage names is not on it. That is a typo or a renamed model rather than anything about
your machine, so a stage naming one is refused at spawn too. The message carries a few of the ids
the provider does list; `lev models list --provider <name>` prints the rest.

A provider publishes its list either by answering `list_models` (a Rhai provider, or a gateway whose
catalogue Leviath has read) or by having one written down under `[model_providers.<name>] serves`.
The `serves` route needs no network and no key, which makes it the way to get a script provider
checked in CI.

**`fanout-worker-task-unheld`** fires when the workers run a stage of this same graph and the graph
binds no input to a region. A work item's inputs then have nowhere to land, so every worker starts
without its work and the merge stage works alone. Declare an input the split fills, such as
`task`, and bind it with `binds = [{ region = "task" }]`.

**`retention-not-zero`** means the spawn would be refused. The message carries the provider's
reason: a Bedrock model the listing never offers under mode `none`, an OpenRouter model with no
zero-retention endpoint, a provider whose agreement is not declared. See
[data retention](/docs/providers#data-retention).

**`blocking-tool-in-autonomous-stage`** fires when an autonomous stage grants `ask_user_*`,
`present_for_review` or `edit_document`. With nobody attached, the run parks there until it is
killed. List the tool in `required_tools`, or set `allow_blocking_tools = true` on the stage, to say
you meant it. A stage granting `@builtin` or `@all` reaches all of them at once and gets one
warning naming the group.

**`implicit-shell-policy`** matters because the default is `ask`. An unattended run waits on that
prompt rather than being denied. The shell arrives with `@builtin` as surely as by name, so a group
grant with no `shell` policy is reported too. A policy in the graph's or the stage's
`tool_permissions` silences it.

**`blueprint-permission-clamped`** is the other side of that. Setting `shell = "allow"` (or
`write_file`, `edit_file`, `install_global_tool`) silences `implicit-shell-policy`. A downloaded
blueprint is not allowed to grant itself write or shell access. The runtime clamps the policy back
to the stricter of it and the built-in default, so the tool still asks. The line looks like a
decision and is not one. Run the agent with `--yolo`, set `[security] allow_blueprint_permissions
= true` in your own `config.toml`, or set the tool there yourself; otherwise drop the line. Tools a
blueprint may pre-approve (`web_search`, `web_fetch`) are exempt, and a policy no looser than the
default (`ask`, `deny`) is fine.

**`routing-without-region-read`** fires when a stage routes tool output into a region other than
the conversation and grants a file-reading tool but not `context_read`. The model is told the
output lives in that region, and its only read verb points at the filesystem. Grant `context_read`.

**`allow-complete-skips-output`** means the model is offered a way to end the run from that stage,
even when its own prompt never mentions it. A run that takes it ends with no answer and looks like
success. Drop `allow_complete` and route to the output stage instead.

**`catalog-unchecked`** is the `unserved-model` question with no answer. The script provider
loaded, but it has neither a `list_models(state)` function nor a `serves` list. It has never said
what it takes, so nothing here can tell a good model id from a bad one. It is a warning rather than
an error because saying nothing is not a refusal. It exists so that "checked and fine" and "never
checked" stop looking identical. Only script providers are named this way; a built-in that keeps
quiet is either covered by `unknown-model` below or has a genuinely open catalog.

**`unknown-model`** is the older, weaker check: the table of models compiled into this build, which
covers Anthropic, OpenAI and Google. It is skipped for any provider that answered for itself, since
a live catalog knows about models released after this build was cut. A provider that neither
publishes a catalog nor appears in that table is not checked at all, which is what keeps an open
catalog (Ollama serves whatever you have pulled) from raising false alarms.

**`no-reachable-provider`** means every entry in the stage's list names something this install
cannot run: a pinned entry whose provider is not configured, or a bare model name nothing here
serves. One entry that works is enough to keep the stage quiet, since the list is an ordered set of
fallbacks and a machine declining some of the options is the normal case. This is also the check
that catches a misspelled model in a stage that names only one: nothing serves `claude-sonet-5`,
so the stage would have fallen back without saying so. It falls back to your `fallback_model`, if
one is set.

**`compact-summarizes-deliverable`** means a later stage reads a paraphrase of a region you marked
`required`. Set `summarizable = false` on the region.

**`required-region-unenforceable`** means `required = true` has no effect. A stage may not finish
while a required region is empty, but only a stage that can write to it is held. When no stage
using the region grants `context_write` or `context_append`, nothing is. Grant one of them to the
stage that owes the region, or drop `required`. A region an input fills is exempt.

**`unbounded-percentage-budget`** names a region that evicts at its bound, budgeted as a share of
the window with no `max`. On the widest model the blueprint names, that share can be so large the
region never evicts and only grows. Give the budget a cap: `budget = { percent = "38%", max = 24000 }`.

**`dead-end-possible`** fires when every normal edge's target has a `max_revisits` budget, so the
run errors once they are spent. Add an edge with `when = "dead_end"` to a stage without one. A
`max_iterations` edge does not count, because it fires on the iteration cap rather than on this
path.

**`read-paths-not-granted`** is the declaring-is-not-granting case. Those reads are refused at
runtime, and the fix line carries the stanza that would grant them.

**`mime-unseen`** is a warning when a stage's models can read none of the mime types it takes, and
a note when they read some of them. Either way, such parts reach the model as one-line stand-ins.
List a model that takes the type, or name the type under the stage's `input_as_text`.

**`holds-under-yolo`** names an interaction point declaring `unattended = "ask"`, or a blocking tool
a stage keeps in `required_tools`. Both are deliberate wherever they appear. It is a note because
`--yolo` reads as "run without me".

**`long-context-price`** is about the whole request: past that size the model bills all of it at
the higher rate. The message gives both rates and the threshold, and nothing needs to change unless
the cost matters. See [costs](/docs/costs#a-long-prompt-can-cost-more-per-token).

**`safe-commands-declared`** applies only where you opt in. That is per agent via
`[agent_safe_commands.<name>] allow_blueprint`, or globally via
`[security] allow_blueprint_safe_commands`.

**`command-seed`**, **`tool-seed`** and **`read-paths-declared`** say what the blueprint will do
before you run it. The seeds run at spawn, before any approval prompt. `command-seed` says whether
the default safe list covers each command, and `read-paths-declared` carries the granted and
declared counts, plus each entry's status.

| Flag | Purpose |
|---|---|
| `--deny-warnings` | Exit non-zero on warnings too. Notes still never fail. |
| `--json` | Print the report as one JSON object. See below |
| `--graph` | Draw the stage graph after the report, as plain text. Ignored with `--json`. See below |
| `--width <COLS>` | How many columns `--graph` may use (default 120). Only with `--graph`. See below |

`--json` prints one object for every outcome: `valid`, `blueprint`, `error`, `findings`, and the
`errors`, `warnings` and `notes` counts. `blueprint` holds the name, version, description, entry
stage, stages, whether a `task` input is accepted, and every input. A blueprint that does not read
or hold together fills `error` and leaves `blueprint` null.

`--graph` draws the same picture the dashboard's stage explorer shows, escape edges included.
`--width` sets how wide it may be, and a wider graph is shrunk to fit. `--width` on its own,
without `--graph`, is refused.

The dashboard's blueprint editor and `POST /api/blueprints/validate` report the same findings, so
a blueprint edited there is checked the same way.

`graph.read_paths` entries are checked against your own `config.toml`, entry by entry, because
declaring one is not the same as being allowed to read it. Anything your config does not grant is
named as such, with the stanza that would grant it. Without a config to consult, it stays the plain
"these need granting" note. See [reading outside the workdir](/docs/security#reading-outside-the-workdir).

### `lev deps <list|check|install> <agent>`

Inspect and set up what an agent [declares it needs](/docs/agents#dependencies). The agent is an
installed name or a path to a blueprint directory or its `agent.toml`.

`lev deps list <agent>` prints the declared dependencies: an MCP server, an environment variable, a
program on PATH, or a condition a Rhai script decides.

`lev deps check <agent>` says whether this machine satisfies them, marking each one and exiting
non-zero when a required one is missing, so it fits a setup script. This is the same check a run
makes: an agent whose required dependency is unmet fails to spawn before any model is billed.

`lev deps install <agent>` puts them in place, and always asks before it does anything, because it
changes your machine. For an MCP server it writes the blueprint's non-secret server settings into
your config. For each secret the server needs, it tells you to set the variable in your own
environment rather than writing it to a file. For a program it runs the install command the
blueprint declares (a per-OS one when given) or a Rhai install script. Pass `--yes` to skip the
confirmation and `--all` to act on every dependency, not only the missing ones.

### `lev test [PATH]`

Run a blueprint's tests: everything in its `tests/` directory, against the real provider.

| Flag | Purpose |
|---|---|
| `-f`, `--filter <PATTERN>` | Only run cases whose name contains this substring |
| `--dry-run` | Parse and report the cases without calling a provider, so nothing is spent |

Each `tests/*.toml` file holds one or more cases:

```toml
[[test]]
name = "greeting"
input = "Say hello"
expect_contains = "hello"

[[test]]
name = "reads the config"
input = "What is in config.toml?"
expect_tool_call = "read_file"
max_tokens = 500
```

| Key | Meaning |
|---|---|
| `name` | Case name. `--filter` matches on it |
| `input` | Given as the `task` input, exactly as `lev run -t "..."` would |
| `expect_contains` | Case-insensitive substring the response must contain |
| `expect_tool_call` | A tool the model must call. It has to be one the stage lists in `tools` |
| `max_tokens` | Caps this case's output. Narrows the ceiling the window and model already impose; it cannot raise it |

**What a case actually runs.** One inference, not a run. `lev test` builds a fresh context
window from the blueprint's layout, seeds `input` as the task, and assembles the request exactly
as a live run's *first* turn would. That means iteration 0, region hooks active, and the first
stage's model and tools. It then calls the provider once and checks the assertions. Nothing
executes: a tool call is
asserted on, never performed, so a case can expect `write_file` without a file appearing.

A `tests/*.rhai` file is run instead as a script through the scripting engine, and fails the run if
it returns `false`.

Before any case runs, the blueprint's own scripts are compiled the way a spawn would compile them:
custom region scripts, output validators, and stage hook scripts all have to load. `--dry-run`
includes those checks, so a broken script is caught without spending anything.

### `lev models`

| Command | Flags |
|---|---|
| `lev models list` | `-p/--provider <NAME>`, `--offline`, `-a/--all`, `--accepts <MIME_TYPE>`, `--produces <MIME_TYPE>`, `--json`, `-r/--remote`. See below |
| `lev models show <MODEL>` | `-p/--provider <NAME>` (ask only this provider), `--offline`, `-r/--remote`. See below |

`-a/--all` includes providers with no credential here. `--accepts <MIME_TYPE>` keeps only models
that take `image/png`, `audio/*` and so on, and `--produces <MIME_TYPE>` keeps only models that hand
back `video/mp4`, `image/*` and so on.

In `lev models list`, the `MIME` column says what a model takes beyond text (`img,pdf`) and, after
an arrow, what it hands back beyond text (`->img`). A `+` after a price marks a model that bills
long prompts at a higher rate. A `!` before a model id marks one whose retention conflicts with your
settings, such as a model that keeps data while `zero_retention` is on. The reasons are listed under
the table.

`lev models show` prints both rates of a model with a long-context tier, a media model's unit price,
and a retention conflict when there is one.

Both ask every configured provider for its own listing by default, waiting up to five seconds each,
and print what the provider said. The columns include the release date and the input and output
price per million tokens, where the listing or the build's price table carries them (`n/a` where
neither does). A trailing line says how many rows came from a provider and how many from the
table compiled into this build. `lev models show` names where a table row's rate came from and the
day the table was read; see [where the prices come from](/docs/costs#where-the-prices-come-from). A provider that could
not be reached keeps its table rows, with a warning naming it. `--offline` skips the network and
prints the table alone. `-r/--remote` is still accepted for older scripts and changes nothing.

`--provider` naming a [Rhai script provider](/docs/rhai-providers) loads that script and calls its
`list_models`: a script names its own catalog at run time, so there is no built-in table to read it
from. What it answers counts as a real provider listing, toward the trailing line and as
`"learned": true` in `--json`. A `serves = [...]` or `[model_capabilities]` claim in your config
never becomes a listing row at all: those feed validation, not this table. A `--provider` may name
nothing at all: no configured provider, no row in the built-in table, no script of that name that
loads. That **exits non-zero** rather than printing an empty table, since there is nothing an empty
table could be reporting.
A provider the built-in table knows but this install has no credential for is still an empty table
and still exits 0.

### `lev mime`

The mime registry as this install sees it, and what a file resolves to under it. See
[Mime](/docs/mime) for what a row means.

| Command | Flags |
|---|---|
| `lev mime list` | `--json`. What the registry holds, and the limits around it. See below |
| `lev mime show <TYPE>` | `--json`. One type as the registry resolves it. See below |
| `lev mime check <FILE>` | `--type <MIME_TYPE>` (take the file as this type, as a sender declaring it would), `--json`. See below |
| `lev mime init` | `--force`. Write a commented example [`mime_types.toml`](/docs/configuration#mime_typestoml) beside your config |
| `lev mime add <TYPE>` | `--family <NAME>`, `--text` or `--binary`, `--tokens <RULE>`, `--extensions a,b`, `--magic <HEX>`, `--stand-in <TEMPLATE>`, `--check <PATH>` or `--no-check`. See below |
| `lev mime remove <TYPE>` | Take a row out of `mime_types.toml` |

`lev mime list` prints the part ceiling in force and where it came from, whether uploads to provider
file storage are on, and each configured provider's inline and file limits. It then prints every
type the registry knows: its family, whether its bytes are text, its extensions, which layer the row
came from (`builtin`, `config`, `mime_types.toml`) and the [check](/docs/rhai-mime-checks) its bytes
must pass.

`lev mime show` resolves one type and gives every field, the token rule spelled out, the check and
whether it loaded, and the source of the most specific row.

`lev mime check` reports the type the file resolves to and where that row came from, its family,
size, dimensions or duration when the header says, and the token estimate. It shows the stand-in a
model that cannot take it would see. It also says how the file reaches a model: as text to any
model, or natively to one that lists the type and as its stand-in to the rest.

`lev models list --accepts <type>` names the models that list a type. `lev mime check` ends with the
verdict of the type's check over the file's bytes, when a row names one.

`lev mime add` adds a row to `mime_types.toml`, or sets the fields given on a row that is there.
`<TYPE>` may be `type/*` for a whole family. `--tokens` takes one of `per_byte=0.25`,
`per_pixel=750,max=1600`, `per_second=32`, `per_page=2000` or `fixed=1000`. The file is checked
before it is written, a `--check` script compiled included, so a flag that would leave it unloadable
is refused with the reason.

`init` is optional. The registry works with no file at all, `add` creates the file when it has
to, and the example `init` writes is a starting point for editing by hand, every field
commented. The three readers use the registry as the daemon builds it, and refuse to run on a row
that does not load, the same fault `lev doctor` reports. `add` and `remove` edit the file in
place, and leave every other row, comment and blank line as you wrote them. An edit reaches the
next run at once and every run already under way within the daemon's housekeeping interval of
thirty seconds.

### `lev agent-client`

Serve an agent over the [Agent Client Protocol](/docs/agent-client-protocol) as JSON-RPC on stdio.

| Flag | Purpose |
|---|---|
| `--agent <NAME\|PATH>` | Blueprint to serve. Omitted, each session's working directory is searched for an `agent.toml` |
| `--yolo` | Approve every tool call without prompting. Recommended for hosts that do not implement `session/request_permission` |
| `--allow <TOOL>` | Allow one tool outright. Repeatable |
| `--max-depth <N>` | Override the maximum sub-agent tree depth |
| `--no-seed-commands` | Refuse the blueprint's command seeds |
| `--output-format <LABEL>` | Ask the agent for its [final output](/docs/outputs) in this format. A differing label retires the declared validator and schema |
| `--output-instructions <TEXT>` | Extra instructions for that final output |

## Blueprints and packaging

| Command | Flags | Purpose |
|---|---|---|
| `lev list` | `--json`, `-f`, `--filter <all\|agents\|blueprints>` | List installed and bundled blueprints. See below |
| `lev add <PACKAGE>` | | Install a blueprint directory or `.leviath-bundle`. Prints what the package grants itself before installing |
| `lev remove <NAME>` | | Uninstall a blueprint |
| `lev pack [PATH]` | `-o`, `--output <FILE>` (default `{name}-{version}.leviath-bundle`) | Bundle a blueprint for [sharing](/docs/packaging) |

`lev list --filter` narrows the listing to installed agents or to bundled blueprints. An
unrecognized value is an error rather than a silent ignore. A blueprint declaring
[`read_paths`](/docs/security#reading-outside-the-workdir) also shows how many of its entries your
config grants.

## Watching and steering

| Command | Flags | Purpose |
|---|---|---|
| `lev ps` | `--json`, `--all` | List runs in the daemon with their status. `--all` also reads the runs dir. See [below](#reading-lev-ps) |
| `lev dash` | | Full-screen TUI [dashboard](/docs/dashboard) |
| `lev msg <AGENT_ID> <CONTENT>` | `--attach` | Deliver a message into a running agent's context. `--attach` and a `@path` in the text send files with it |
| `lev pause <RUN_ID>` | | Pause a run. It finishes its in-flight step, then holds. See [below](#pausing-a-run) |
| `lev resume <RUN_ID>` | | Un-pause a run, or bring back a cancelled one where it stopped |
| `lev cancel <RUN_ID>` | `--force` | Cancel a run. Also aliased as `lev kill` |
| `lev context <RUN_ID>` | `--json`, `--full` | Show a run's context-window history from its `run.lvr` archive |
| `lev result <RUN_ID>` | `--json`, `--raw`, `--artifact`, `--out`, `--open` | Print what the agent handed back, or hand out the files it produced. See [below](#lev-result) |
| `lev blobs <RUN_ID> [PART]` | `--json`, `--out`, `--open` | List the files a run holds as stored parts, or fetch one. See [below](#lev-blobs-run-id-part) |

`lev msg --attach` and a `@path` in the message text take the same forms as on `lev run`.

### Pausing a run

A run waiting on a question, an approval or a checkpoint can be paused too. The question stays
open, and you can still answer it. The run takes the answer and holds until `lev resume`. A run
waiting only on its own sub-agents pauses those sub-agents instead.

`lev cancel --force` writes the run's on-disk state terminal without asking the daemon, for when
the daemon is gone or unresponsive. Without it, the daemon is asked first, since it can stop the
work rather than only record the outcome, and the on-disk write is the fallback.

`lev context --full` includes each region's entry contents instead of per-region summaries. An
entry that carries files shows each as its own row: the stand-in the model would see, the hash,
the token estimate, and a delivery override when the entry has one. The summary counts a region's
stored parts beside its entries, and `--json` carries every part as it was recorded.

### `lev result`

Print the answer a finished run submitted. It reads the run's [file](/docs/run-file), so it needs
no daemon and works for a run that finished last week.

```bash
lev result agent-abc123          # the answer, with its run and stage
lev result agent-abc123 --raw    # the answer alone, for a pipeline
lev result agent-abc123 --json   # the answer plus its shape and stage
```

A run that produced no answer exits non-zero rather than printing nothing. So
`lev result <id> > answer.txt` in a script cannot quietly write an empty file.

Files the run produced are listed under the answer, with their type, size and hash. Three flags
hand them out without a trip to the working directory:

```bash
lev result agent-abc123 --artifact final > trailer.mp4   # one file's bytes, by the name the stage gave it
lev result agent-abc123 --out ./delivered                # every file into a directory, each path printed
lev result agent-abc123 --artifact final --out ./here    # just that one, into a directory
lev result agent-abc123 --open final                     # hand one to whatever the OS opens it with
```

The bytes come from the run's own store when the answer recorded a hash, so they are what the
stage submitted even if the working directory has moved on. A file the store does not hold is read
from the working directory instead. `--open` writes the file under the system temp directory
first, so it has a name and an extension the opener can type it by. Nothing in `lev` plays or
draws a file.

Only an agent that calls `submit_output` has an answer to show. See
[Final outputs](/docs/outputs) for how a blueprint asks for one.

### `lev blobs <RUN-ID> [PART]`

Every file a run holds as a stored part, whatever put it there: an attachment on `lev run`, a
`read_file` on an image, an MCP server's audio block, a `context_attach`, a submitted artifact.
Read from the run's [file](/docs/run-file) and its `blobs/` directory, so it needs no daemon.

```bash
lev blobs agent-abc123                       # name, type, size, shape, tokens, hash, and the regions holding each
lev blobs agent-abc123 --json
lev blobs agent-abc123 hero.png > hero.png   # one part's bytes, by name
lev blobs agent-abc123 ab12cdef --out ./     # by a hash prefix (six characters or more), into a directory
lev blobs agent-abc123 hero.png --out x.png  # to a path
lev blobs agent-abc123 hero.png --open       # hand it to the OS
```

A part the context names but the store no longer holds is listed with a note and cannot be
fetched. A part with no name exports as its short hash plus the extension its type implies.

### `lev interactions [REQUEST_ID]`

List the questions runs are waiting on, each with its options numbered from 1 and the
`lev respond` line that answers it. Name one to see it in full, with the tool call's arguments and
the document under review as well. Reading never answers anything.

```bash
lev interactions                    # every open interaction
lev interactions probe-1789971553   # one, in full
lev interactions --json             # the whole request for each, for a script
```

```
probe-1789971553-793b8652da33-approve-1  [tool-approval]  agent=probe-1789971553-793b8652da33  stage=work
  Allow tool call: `shell` - `cargo test`?
    [1] Allow once  (allow)
    [2] Allow cargo test for this stage  (allow-stage)
    [3] Allow cargo test for this run  (allow-run)
    [4] Deny  (deny)
    [5] Deny with feedback  (deny --feedback "TEXT")
    tool: shell
  answer with: lev respond probe-1789971553-793b8652da33-approve-1 allow|allow-stage|allow-run|deny|deny-feedback  (or 1-5; deny takes --feedback TEXT)
```

`--json` gives each request an `answer_options` list. Every option there has an `id` (the word
that answers with it), its `label`, the `number` the listing shows, and `answer`, the whole
`lev respond` command. An agent keeps the `id`: it does not move when the options are reordered.

`REQUEST_ID` follows the same rules as on `lev respond` below.

### `lev respond <REQUEST_ID> [ANSWER]`

Answer an interaction the daemon is holding, with what the question shows. An answer can't be
taken back, so the command needs exactly one of `ANSWER`, `--choice`, `--approve` or `--deny`.
With none it refuses and points at `lev interactions`.

| Question | `ANSWER` |
|---|---|
| Tool approval | `allow`, `allow-stage`, `allow-run` or `deny`. `deny` takes `--feedback` |
| Confirm | `yes` or `no` |
| Multiple choice | The option, or any start of it that names just one |
| Free text or edit | The text itself, exactly as written |

Words match in any case. The number `lev interactions` lists an option under answers with it too,
so `lev respond <id> 2` picks the second option on the screen. Only a question with options reads
a number that way: for a free-text question, `1` is the text "1".

```bash
lev respond probe-1789971553 allow-run      # allow, and every later call this covers, this run
lev respond probe-1789971553 3              # the same, by its number
lev respond probe-1789971553 deny --feedback "use git log, not git show"
lev respond pick-1789971553 postgres        # a choice, by its start
lev respond ask-1789971553 "ship it Friday" # a free-text answer
```

`REQUEST_ID` can be the start of an id rather than the whole thing, so a prompt is answered
without copying forty-odd characters. It has to leave exactly one open interaction: a start
that fits two is refused with both of them listed, and nothing is answered. An id given in
full always answers that interaction, even where longer ids begin with it.

The answer has to fit the question. `""` is a text answer too: it acknowledges a review or keeps
a document unchanged. A start of a choice that fits two options is refused with both named. A word
the question does not offer is refused with every answer that would work, ready to copy, and the
question stays open:

```
"maybe" is not an answer to this approval; answer allow, allow-stage, allow-run, deny or
deny-feedback (or 1-5)
```

The flags below are the same answers in the older spelling, kept so existing scripts still work.

| Flag | Purpose |
|---|---|
| `--choice <INDEX>` | The scripting form: the option at this zero-based place in the listing, for any question with options. `--choice 0` is the option listed as 1 |
| `--approve` | The same as `allow`, or `yes` on a confirm. Conflicts with `--deny` |
| `--deny` | The same as `deny`, or `no` on a confirm |
| `--feedback <TEXT>` | With a deny, however it is written, what the model should do instead. An error beside `--approve` |
| `--stage` | With `--approve`, the same as `allow-stage` |
| `--session` | With `--approve`, the same as `allow-run` (alias `--run`) |
| `--attach <PATH[:REGION][:TYPE][:text|native|stand_in]>` | Attach a file to a text answer, as on `lev run --attach`. Repeatable. See below |

The model reads `--feedback` text inside the refused call's tool result.

A `@path` inside the answer attaches that file too. `--attach` is refused on a choice or an
approval.

See [Human-in-the-loop](/docs/interaction) for what raises these.

### Reading `lev ps`

```
RUN                             TITLE                  STATUS                  STAGE         ITER   TOOLS  AGE  WORK  MOVED
solo-1785568852-9fa61fd279dd    Retry backoff audit    waiting: tool approval  work          1      1      12m  41s   41s
busy-1785568852-384bad04c9ac    Index the changelog    active                  work          13824  13824  12m   12m  0s
waiter-1785568852-7895a2209850  Split the log sweep    waiting: children(1)    delegate 1/2  2      1      12m  12m   41s

1 run needs an answer: lev interactions
```

`TITLE` is the [generated one-line title](/docs/configuration#title). The column appears only when
at least one listed run has one. A run whose titling was turned off or did not finish leaves the
cell empty, rather than widening every row for nothing.

### Age, work, and moved

Three columns, because a run can look very different under each and the difference is
usually the thing you are trying to see. The first row above is the case: alive for twelve
minutes, at work for forty-one seconds of them, and holding a prompt open for the rest.

`AGE` is how long since the run was launched. It says nothing about whether the run has
done anything.

`WORK` is how long the run actually spent working. The clock runs while it is inferring,
calling tools, or held for its own fan-out workers and sub-agents. It stops for
everything that is not the run's doing: paused, blocked on a person, parked until the
machine is fixed, finished. This is the figure to call a run's duration. `AGE` counts the
overnight pause, and this does not. The run's [file](/docs/run-file) keeps the clock, and each
stage keeps one of its own.

`MOVED` is how long since the run last actually moved: a new iteration, a new stage, or a
change of status. It is deliberately not the run's `updated_at`, which also advances
on a 30-second heartbeat so that observers can tell a live daemon from a dead one. A fresh
`updated_at` is therefore not evidence of progress; a fresh `MOVED` is. The same figure is
`last_progress_at` in `lev ps --json`.

> [!NOTE]
> `MOVED` was headed `AGE` before, and showed what `MOVED` shows now. If you have a script
> reading the table, read `lev ps --json` instead: every row there carries `started_at`,
> `last_progress_at` and `active` raw, plus `age_secs` and `working_secs` already computed,
> the same two keys the [HTTP API](/docs/api#how-long-a-run-has-taken) serves.

`lev ps` lists what the daemon is holding, plus the runs that finished within the
retention window above. `lev ps --all` adds a second block read from the runs dir instead,
so runs older than that window, and runs from before the last daemon restart, are still
accounted for:

```
NOT RUNNING
RUN                             STATUS               AGE  WORK  MOVED
coder-1785568100-a1b2c3d4e5f6   complete             1h   22m   4m
coder-1785567000-c3d4e5f6a1b2   error                3h   1m    1h
router-1785560000-e5f6a1b2c3d4  running (abandoned)  2h
```

`(abandoned)` means the run claims on disk to be running, the daemon is not holding it,
and it has not moved in five minutes. Clear it with `lev cancel <run-id>`. With `--all` a
daemon that is down is reported rather than fatal, and nothing is marked abandoned in that
case, because an unreachable daemon looks exactly like every run dying at once. See
[reconciling an external work queue](/docs/work-queues) if
you are driving Leviath from a scheduler.

| Status | Meaning |
|---|---|
| `active` | Running a turn, or waiting on the model or a tool |
| `idle` | Spawned, not yet started |
| `paused` | Paused with `lev pause` |
| `waiting` | Blocked. The reason follows the colon |
| `complete` | Finished |
| `cancelled` | Cancelled with `lev cancel`. `lev resume` brings it back |
| `error` | Ended with the error shown |

A `waiting` run always says what it is blocked on, because the answer decides whether you
need to do anything. These are stopped until a person acts:

| Reason | What to do |
|---|---|
| `tool approval` | A tool call needs approving with `lev respond` |
| `user prompt` | The agent asked a question (`ask_user_*`). Answer it |
| `taint gate` | A call needs clearance for the data it touches |
| `checkpoint` | A blueprint stage-boundary review |

These resolve on their own, and are a normal part of a healthy multi-agent run:

| Reason | Meaning |
|---|---|
| `workers(n)` | A [fan-out](/docs/stages) parent, `n` workers still to finish |
| `children(n)` | A stage holding for `n` spawned [sub-agents](/docs/sub-agents) |

That distinction is the useful one. `waiting: children(3)` next to three busy children is a healthy
run doing exactly what it should. `waiting: tool approval` at ten minutes is a run nobody answered.

Launch with `--yolo` to approve automatically. Sub-agents and fan-out workers inherit it, and it
survives a daemon restart.

#### `(no output)`

A finished run can read `complete (no output)`, and likewise for `cancelled` and `error`. It means
the run changed no files, even though its agent had a tool for changing them.

Almost always the edits went through the shell, which Leviath cannot see. `sed -i`, `tee`, and
redirects leave no trace, so nothing downstream knows the work happened. Either re-apply those edits
with `write_file` or `edit_file`, or name the tool you do write with in a transition
[gate](/docs/stages#what-counts-as-output) so that it counts.

Agents that never had a file-writing tool are never marked this way. A router that delegates, or a
researcher whose answer is its report, has no file changes to be missing.

#### The `READS` column

This column only appears when one of the listed runs declares
[`read_paths`](/docs/security#reading-outside-the-workdir). It reads granted over declared, as
resolved when the run spawned.

`0/2` is the one to watch for. That run is up and looks healthy, and every read its author designed
it around will be refused. Run `lev validate` on the blueprint's directory to see which entries, and the config block
that grants them.

### Runs that have finished

A run keeps its place in the listing for five minutes after it ends, then drops out.

That window exists so a run that failed is still there to say so. Without it, a run that died on its
first model call would leave the listing within seconds, and read exactly like a run that was never
spawned at all.

So you get this instead of an empty listing:

```
RUN                             STATUS                              STAGE  ITER  TOOLS  AGE
worker-1785616492-6f0d21ab4c11  error: HTTP 402 Payment Required    work   0     0      41s
```

`ITER 0` and `TOOLS 0` next to an error mean the run never got as far as its first turn. Set
`[limits] finished_retention_secs` to widen or narrow the window, or `0` to drop a run as soon
as it finishes. The record is held in memory, so restarting the daemon clears it early; the
durable copy is the run's file, `run.lvr`, which `lev ps --all` and `GET /api/runs` read.

Two things this does not cover. A spawn that fails outright never becomes a run, so it is
reported by `lev run` itself rather than here. And a run that finished longer ago than the
window is gone from the listing for good.

`lev ps --json` prints the same data unformatted, for scripts:

```json
{ "runs": [ ... ], "finished": [ ... ], "health": { ... } }
```

Finished runs are their own key rather than mixed into `runs`, so counting what is running
stays a matter of reading one list. Both carry the `empty_output` field, and a `read_paths`
object with the granted and declared counts when the blueprint declares any. The completion
webhook carries the `empty_output` key.

## The daemon and API

### `lev daemon [ACTION]`

With no action, runs the [daemon](/docs/daemon) in the foreground.

| Action | Purpose |
|---|---|
| `start` | Start it in the background. A no-op if one is already running |
| `stop` | Shut it down |
| `status` | Report whether it is running and how many agents it hosts |
| `restart` | Stop, then start, reloading persisted agents |
| `install` | Register with the OS supervisor (launchd, or `systemd --user`) so it starts at login and restarts if it dies |
| `uninstall` | Deregister it |

`--socket <ID>` overrides the control socket path and works on every action.

### `lev serve`

Start the [REST and WebSocket API](/docs/api).

| Flag | Default | Purpose |
|---|---|---|
| `-p`, `--port <PORT>` | `3000` | |
| `-H`, `--host <HOST>` | `127.0.0.1` | |
| `--name <NAME>` | the port | Names this server's log file, `~/.leviath/serve-<NAME>.log`, so two servers side by side keep separate logs |
| `--token <TOKEN>` | unset | Bearer token clients must present. Overrides `LEVIATH_API_TOKEN`. The server refuses to start if neither is set |
| `--cors <ORIGIN>` | none | Allow browser requests from an origin. `*` is accepted and means any origin |
| `--allow-admin` | off | Mount the MCP administration and config-write routes |
| `--workdir-root <PATH>` | unset | Restrict agent working directories to this root |
| `--no-remote-yolo` | off | Refuse a spawn request whose `launch.unattended` is not `off`, or that sets `launch.allow` |
| `--no-remote-seed-commands` | off | Treat every spawn as `launch.seed_commands = false`, so command seeds never run for a run started over the API |
| `--max-concurrent-requests <N>` | `[serve]` key, else `64` | Requests in flight before the next is answered 503. `0` disables the cap. Websocket routes are not counted |
| `--request-timeout-secs <SECS>` | `[serve]` key, else `30` | Seconds a request may take before it is answered 408. `0` disables the deadline. Websocket routes are not timed |
| `--tls-cert <PATH>` | unset | PEM certificate chain. Serves HTTPS; needs `--tls-key` too |
| `--tls-key <PATH>` | unset | PEM private key for `--tls-cert` |

> [!TIP]
> A browser cannot call an `http://` Leviath that is not on loopback, whatever `--cors` says. That
> holds on a LAN too. That is what the TLS flags are for. See
> [reaching a Leviath on another machine](/docs/api#reaching-a-leviath-on-another-machine).

> [!WARNING]
> Prefer `LEVIATH_API_TOKEN` over `--token`. A command-line argument is visible in `ps` to every
> local user for the life of the process.
>
> `--allow-admin` is off by default because the MCP write routes are remote code execution by
> construction: adding a server writes a `command` into your config, which Leviath then spawns.
> `--workdir-root` matters for the same reason: without it a token holder can point a
> tool-executing agent at any directory, including `/`.

## Configuration and tools

### `lev doctor`

Check that provider wiring works, end to end. Four checks run in order, the first failure stops the
rest, and the one that fails is the diagnosis. A fifth, `journal`, asks the running daemon whether
it is still recording what its runs do. It runs early, costs nothing, and never cuts the run short,
so a report stopped by a billing failure still carries it. `--no-daemon` and `--offline` skip it,
because there is no daemon to ask.

| Check | What it proves | A failure means |
|---|---|---|
| `config` | `config.toml` parses and a provider registry can be built. See below | The config file is malformed |
| `resolve` | Your defaults pick a provider that is actually registered. See below | Nothing in `default_provider` or `provider_order` is configured: a key is missing or misspelled, or `lev setup` never ran |
| `inference` | One real call reaches the model | A bad key, an unknown model id, or a billing problem |
| `daemon` | A one-stage agent spawns over the control socket, runs, and finishes | The handoff is broken even though the credentials are fine |
| `journal` | The running daemon has written every run record it tried to | Something on the filesystem is stopping the daemon recording what its runs do |

The `config` OK line also carries notes for a file that loads with problems in it: keys nothing
reads, and `[model_providers.*]` script entries whose `.rhai` file is not on disk. With no
`override_model` or `fallback_model` set, `resolve` passes on the first configured provider in your
preference. The next check then picks a model from that provider's catalogue.

```bash
$ lev doctor

  config     OK  default_provider=openrouter; registered: ollama, openrouter (script providers resolve by name)
  journal    OK  1284 record(s) written, none lost
  resolve    OK  openrouter / anthropic/claude-sonnet-4.5
  inference  OK  12 in / 4 out / 16 total, replied PONG  (1.2s)
  daemon     OK  run doctor-1785649252-bf7b3d07a265 Complete after 1 iteration(s)  (0.3s)

doctor passed
```

The fourth check spawns a throwaway one-stage agent with no tools, waits for it, and then deletes
the run. Nothing is left in `lev ps` or on disk.

| Flag | Purpose |
|---|---|
| `-m`, `--model <MODEL>` | Test a specific model. Takes the same forms as `lev run --model` |
| `--no-daemon` | Stop after the third check. Contacts no daemon, starts none, and creates no run |
| `--offline` | Stop after the second check. Proves the config parses and the model resolves, and bills nothing |
| `--json` | Print the checks as `{"checks": [...], "passed": bool}` |

`--model` takes `provider/model` to pick both, and a bare model id pairs with your
`default_provider`. `--model provider/model` is the way to reach a
[Rhai script provider](/docs/rhai-providers), which is resolved by name. Use it to try a model
string before wiring it into a blueprint: it goes further than `lev models list --provider`, which
compiles the provider and reads its catalog but never sends an inference.

`lev doctor` exits non-zero when a check fails, so it works as a CI gate. It bills two inferences
per run, each capped at 64 output tokens; `--no-daemon` bills one, and `--offline` none.

### `lev rage`

Pack the logs and settings a bug report needs into one zip, with every key removed. A small
screen asks what the problem was about and, for a run, which one. Nothing is uploaded: attaching
the zip to an issue is your decision. [Reporting issues](/docs/reporting-issues) says what the zip
holds, what it never holds, and how to attach it.

```bash
$ lev rage --run abc123 --note "the review stage never finished" -o report.zip
Wrote report.zip (1.4 MiB, 6 secrets removed)
  README.md                 1 file(s)        3 KiB
  config/                   3 file(s)        2 KiB
  runs/                    14 file(s)      1.3 MiB
  ...
```

| Flag | Purpose |
|---|---|
| `--about <setup\|run\|agent\|other>` | What the problem was about. Answers the first question on the screen |
| `--run <RUN_ID>` | The run it happened in: an exact id, or a prefix only one run starts with. Implies `--about run` |
| `--agent <PATH>` | The blueprint you were building: its directory or its `agent.toml`. Implies `--about agent` |
| `--note <TEXT>` | What happened, in your words. Lands at the top of the zip's README |
| `-o`, `--output <PATH>` | Where to write the zip. Default: `./leviath-rage-<timestamp>.zip` |
| `--no-blobs` | Leave a run's stored media parts out |
| `--non-interactive` | No screen: build the zip from the flags and print its path. A non-terminal stdout does the same |

The zip keeps your task text, the model's replies, tool output and file contents, which is what a
helper needs. Read it before you share it.

### `lev setup`

The interactive [provider](/docs/providers) wizard. Every credential and agent choice it asks for
has a flag, so headless setup is scriptable. The wizard's Limits screen edits the
[`[limits]`](/docs/configuration#limits) keys, which have no flags: script those by writing
`config.toml` directly. That screen is opt-in, because every limit already has a working default.
It only appears once you turn on **Show advanced tuning** on the Defaults screen. Skipping it
changes nothing about what gets written.

The Providers screen lists the providers this install has, not the whole catalog. **Add a
provider** (or `a`) asks three short questions in a chooser, one level at a time. The first is how
the provider is reached (an API key, a subscription sign-in, a server you run). The second is what
it makes (text and images, video, speech and audio, 3D models), and the third is which one. A
provider that makes several of these is
listed under each. Esc steps back a level. Or type at the first question: a provider's name or
what it does (`anthropic`, `sora`, `video`) finds it, and Enter goes straight to it. The provider
then opens in a modal: its
credential, its sign-in or its endpoint entries, and three ways out at the foot. **Verify and use**
checks the credential against the provider and keeps it once the check passes, staying open with
the answer if it fails. **Skip verification and use** keeps it unchecked, and **Cancel** puts the
provider back the way it was. Enter on a listed provider reopens that modal, and `d` removes the
provider, clearing its key when you finish. A provider supplied by an environment variable cannot
be removed here; unset the variable. The wizard will not continue past this screen, or finish,
with no provider configured, because a config without one cannot run an agent.

Each listed provider shows what it said the last time anything asked it, and when: "12 models ·
checked 2 hours ago", or the error it gave. That answer is shared. The daemon records one each time
it starts and reads every provider's model list. `lev models` records one for each provider it
lists live, and the wizard records its own checks. All of them land in
`~/.leviath/model_capabilities.json`, beside the model lists themselves. A check counts only for
the key it was made with: change a key
and that provider reads "not checked yet" until it is checked again. The file holds a fingerprint
of each key, never the key, made with a random key of this install's own
(`~/.leviath/provider-check.key`). `lev rage` includes the cache in a bug report and never that
key, so a shared report gives nothing to guess a key back from.

The Defaults screen leads with **Provider priority**: the order a bare model name prefers, whose
head is your default provider. Enter opens a modal to arrange it. Drag a row by its `⠿` grip, or
move the one under the cursor with `Shift+↑`/`Shift+↓`. Use `K`/`J` on a terminal that keeps
Shift+arrows for itself (Apple Terminal does). Space takes a configured provider out of the order
or brings it back in. A provider left out is still configured, and still runs any stage that names
it as `provider/model`. A bare model name is never routed to it. Configuring a new provider
does not add it to the order on its own, and at least one provider always stays in. It writes the
same [`provider_order`](/docs/configuration#provider-preference-order) that `lev providers order`
and `PUT /api/config` set, so putting a subscription like Codex first there is how you route bare
model names onto your plan.

Quitting with unsaved choices asks first, and the dialog lists the choices that would be
discarded.

The same screen carries **Zero data retention (ZDR)**: a switch that asks every provider to keep
nothing of your prompts and replies once a reply is returned. Under it sits a row for each chosen
provider that settles retention by contract (Anthropic, OpenAI, Google), so an agreement your
organisation holds can be declared where the key is. Each row's help spells out what the provider
keeps without it. What the switch does, provider by provider, is
[data retention](/docs/providers#data-retention). Under those rows, **Upload media to provider
file storage** turns off uploading large parts to a provider's Files API; zero data retention
turns uploads off whatever it says. See [Files and size limits](/docs/mime#files-and-size-limits).

| Flag | Purpose |
|---|---|
| `--non-interactive` | Use only flag values, ask nothing |
| `--no-verify` | Skip checking credentials against the provider APIs |
| `--anthropic-key`, `--openai-key`, `--google-key`, `--xai-key`, `--meta-key`, `--openrouter-key`, `--bedrock-key <KEY>` | Provider API keys |
| `--file-uploads <true\|false>` | Upload large parts to a provider's file storage once and name them by id. On unless set |
| `--bedrock-region <REGION>` | AWS region for Bedrock (default `us-east-1`; also read from `AWS_REGION`) |
| `--zero-retention <true\|false>` | Ask every provider for zero data retention (ZDR). Writes `[providers] zero_retention`; see [data retention](/docs/providers#data-retention) |
| `--zero-retention-agreements <NAMES>` | Providers your organisation holds a zero data retention agreement with, comma separated (`anthropic,openai,google`). Replaces `zero_retention_agreements` |
| `--ollama-url <URL>` | Ollama base URL |
| `--override-model <MODEL>` | One model every stage starts on, ahead of what its blueprint names; unset lets each blueprint decide |
| `--fallback-model <MODEL>` | The model a stage falls back to when none of the models it names is configured here |
| `--codex <true\|false>` | Enable the Codex transport, which bills a ChatGPT subscription. Flips the switch only. See below |
| `--grok <true\|false>` | Enable the Grok transport, which bills a SuperGrok or X Premium+ subscription. Flips the switch only. See below |
| `--install-agents` | Install the bundled blueprints without asking |

```bash
lev setup --non-interactive --anthropic-key sk-ant-... --install-agents
```

Zero data retention turns uploads off whatever `--file-uploads` says.

`--codex` and `--grok` flip a switch and nothing more. Interactive `lev setup` signs in from its own
screen, and a non-interactive run has nobody watching a browser. Sign in with `lev auth login codex`
or `lev auth login grok` on that path.

The provider list ends with three entries for servers that speak OpenAI's chat API, and picking
any of them writes a [`kind = "openai-compatible"`](/docs/configuration#openai-compatible-endpoints)
entry rather than a key. **llama.cpp** and **LM Studio** are presets: each starts at its server's
default address (`http://localhost:8080/v1` and `http://localhost:1234/v1`) with no key, and is
written as `llama-cpp` or `lm-studio`. **Custom OpenAI-compatible endpoint** asks for a name, a
base URL, an optional key and optional headers (`Name: value`, several separated by semicolons).
All three repeat: a preset's modal is a small form per endpoint with **Add another** at the end
and **Remove this endpoint** on each, so two llama.cpp servers on two ports are two entries.
**Check this endpoint** asks the server for its models. On success they are listed, and the
**Default model** row cycles through them. On failure the entry is kept, and the **Models** row
takes the ids by hand, which is what the entry's `models` list is. Every endpoint
appears in the default-provider choice by its own name. These entries have no flags; script them
by writing `config.toml`.

> [!NOTE]
> The bundled agents are **not** installed unless `--install-agents` is passed in non-interactive
> mode. That is deliberate, so a scripted setup does not write blueprints you did not ask for.

Each blueprint is listed with what setup would do to it: install it, update it from the version on
disk, or nothing. A copy at the bundled version whose files differ from the bundled ones reads as
`edited locally` and is offered **unchecked**, because installing removes the destination directory
first and would take your edits with it.

`lev run` says the same thing at the moment it matters: a run starting on an installed bundled
blueprint that this build ships a different version of prints a one-line note before it spawns.

Setup remembers what you turned down. An MCP server you left unchecked, or a blueprint you chose
not to install, is still listed the next time you run `lev setup`, so you can change your mind. It
is no longer pre-selected, though, and finishing the wizard again will not quietly bring it in.
Only refusals are remembered, and only from a run you finished: accepting needs no memory, because
the server lands in your config and the blueprint lands on disk. A blueprint's refusal is recorded
against the version that was offered, so a newer bundled version is a fresh offer and gets asked
about again rather than being hidden by an old "no thanks". This lives in `ui-state.json` under the
data directory, alongside what the dashboard remembers, and never in `config.toml`.

Inside the wizard, the keys work the same way on every screen:

| Key | Meaning |
|---|---|
| `↑` `↓` (or `k` `j`) | Move between rows |
| `←` `→` (or `h` `l`) | Cycle a choice or the reasoning effort |
| Space or Enter | Select the focused row; Enter also opens editors for typed values |
| Enter on a default | Opens a searchable list of providers or models, with what the choice decides |
| PgUp / PgDn, Home / End | Scroll a long screen; the selection moves with the view |
| Enter on `[ Continue ]` | Move to the next screen (the button is the last row) |
| Tab / Shift-Tab | Next / previous screen |
| Esc | Previous screen, or cancel an edit or dialog |
| `v` | Re-check a credential against the provider's API |
| `o` | Open the provider's signup page |
| Ctrl-R | Show or hide credentials |
| Ctrl-S | Write the config and finish, from anywhere |
| `?` or F1 | Help overlay. It scrolls, so a long list is not cut off |
| `q` / Ctrl-C | Quit without writing. If you changed anything, it asks first |

Nothing is written until you confirm on the Review screen. Leaving the provider screen with
nothing selected asks before letting you continue, since an agent cannot run without one.

### `lev providers`

Show the configured providers and set their **priority order**. That is the
[`[providers] provider_order`](/docs/configuration#provider-preference-order) that decides which
provider serves a bare model name (one a blueprint lists with no provider) when more than one
serves it.

| Command | Options | Purpose |
|---|---|---|
| `lev providers` (or `lev providers list`) | `--json` | List configured providers and the current priority order |
| `lev providers order <NAME>...` | | Set the order, best first (e.g. `lev providers order codex openrouter openai`) |
| `lev providers order --clear` | | Remove the order, so `default_provider` alone decides |
| `lev providers retention` | `--json` | What each provider keeps of a request, how that is controlled, and Bedrock's live account mode. See [data retention](/docs/providers#data-retention) |
| `lev providers retention set <zero\|off>` | | Write `[providers] zero_retention`; `zero` also sets Bedrock's account mode to `none` |
| `lev providers retention bedrock <MODE>` | | Set Bedrock's account data retention mode directly: `none`, `default`, `aws_review` or `inherit` |
| `lev providers quota` | `--json` | How much of each signed-in subscription (Codex, Grok) is used, against what limit, and when each window resets |

The order is the whole list of providers a bare model name may run on. A configured provider that
is not in it is reachable only by an explicit `provider/model`, so configuring it never silently
moves a stage or its billing. That holds for a subscription transport (Codex, Grok) as much
as for an API key. A name that is not a configured provider is refused rather than written, since it
could never win a route.

### `lev mcp`

Manage [MCP tool servers](/docs/mcp).

| Command | Flags | Purpose |
|---|---|---|
| `lev mcp add <NAME>` | `--url`, `--command`, `--arg` (repeatable), `--env KEY=VALUE` (repeatable), `--header KEY=VALUE` (repeatable), `--no-login` | Add a server. Detects OAuth and starts a login unless `--no-login` |
| `lev mcp list` | `--json` | List servers and their auth status |
| `lev mcp remove <NAME>` | | Remove a server |
| `lev mcp login <NAME>` | | Authenticate or re-authenticate |
| `lev mcp logout <NAME>` | | Forget stored credentials |
| `lev mcp test <NAME>` | | Connect and list the server's tools |

Transport is inferred from whether you pass `--url` or `--command`.

### `lev auth`

| Command | Flags | Purpose |
|---|---|---|
| `lev auth status` | | The credential backend in use and what it holds, plus each signed-in subscription's plan and usage |
| `lev auth login <provider>` | | Sign in with a browser (`codex` or `grok`); stores the grant outside `config.toml`. See below |
| `lev auth logout <provider>` | | Forget a browser sign-in, leaving the provider enabled. Grok's session is also revoked at xAI |
| `lev auth migrate` | `--to-file`, `--dry-run` | Move secrets between `config.toml` and the OS keychain |

`lev setup` signs in on its own screen, so `lev auth login` is for headless machines and revoked
sessions.

`lev auth migrate` moves keys into the OS store by default; `--to-file` moves them back out. Set
`[security] credential_store` in the [config](/docs/configuration#security) first.

### `lev update`

Update Leviath, then offer to bring everything else up to date with it: the binary, the bundled
blueprints, and the config file, in that order.

The binary is updated with the installer that put it there, and which one that was is read off the
filesystem rather than guessed from the version string. The version cannot answer: every
[channel](/docs/releases) ships the same number, because the `-alpha` and `-beta` suffixes live in
the tap manifests and not in the binary. Where the file sits does answer.

| Found at | What it runs |
|---|---|
| A Homebrew Cellar path, or a Homebrew-only prefix | `brew update && brew upgrade <formula>` |
| `scoop/apps/<package>` or a scoop shim | `scoop update && scoop update <package>` |
| `~/.cargo/bin` | Nothing. It says to run `cargo install leviath-cli` |
| `/usr/local/bin`, `/usr/bin`, `~/.local/bin`, `%LOCALAPPDATA%\Leviath\bin` | `curl -fsSL https://leviath.dev/install.sh \| sh -s -- --channel <CHANNEL>` |
| Anywhere else | Nothing. It names the path and leaves the choice to you |

A Cellar or `apps` path carries the package name, and the package name carries the channel, so a
beta install updates to beta without being told. The install script records nothing at all, so its
channel is genuinely unknowable: that arm defaults to `stable` and `--channel` is how you say
otherwise.

A `cargo install` is described rather than run, because updating it is a full compile and that is
not something to start because somebody typed `lev update`.

| Flag | Purpose |
|---|---|
| `--check` | Print the plan and change nothing |
| `--json` | Print the plan as JSON and change nothing |
| `--channel <stable\|beta\|alpha>` | The channel to re-install. Only the install-script method reads it |
| `--dry-run` | Walk the whole flow, prompts and all, printing each action instead of doing it |
| `--yes` | Answer yes to the binary upgrade and the config write. It does **not** install blueprints |
| `--install-agents` | Install the bundled blueprints without asking |

```bash
$ lev update --check

lev 0.3.5, installed with Homebrew (formula leviath-beta, beta channel)

  binary   brew update && brew upgrade leviath-beta
  agents   1 of 11 would change
             data-analyst - update 0.0.1 → 0.0.2
  config   nothing to migrate
```

The package managers are told to refresh their index first, because neither sees a release
published minutes ago until it has. When the update check says this copy is already the newest on
its channel, the binary step is skipped and says so.

All three steps run every time, whatever the binary step did. That is the point of the command.
`brew upgrade` and `scoop update` hand you a new binary and say nothing about the blueprints in
`~/.leviath/agents` or the config beside them. Anyone who has ever updated that way is running
blueprints from whenever they last ran `lev setup`. A binary that needs no update is not evidence
that anything else is current.

The blueprint step is the same offer `lev setup` makes, and nothing is written to your agents
directory without a yes. The whole list is printed first, then one confirmation covers it;
`--install-agents` is how a script says yes. `--yes` alone is deliberately not enough, because
updating a binary and replacing the blueprints in your agents directory are different requests.

A copy at the bundled version whose files differ from the bundled ones reads as edited locally. It
is named as edited, asked about on its own, and no flag covers it: installing removes the
destination directory first and would take your edits, and any file you added, with it.

Blueprints you wrote yourself are left alone. A blueprint is an `agent.toml`, which has no old
spellings to rewrite: a key it does not know is an error that [`lev validate`](#lev-validate-path)
names. A blueprint still written as an `agent.leviath` is converted with
[`lev blueprint migrate`](#lev-blueprint-migrate-path).

The config step applies any migration this build knows how to make to your `config.toml`, printing
every change before it asks to write anything. A renamed key is one such migration, such as
`default_model` becoming `fallback_model` or `[sandbox] persist` becoming `keep_warm`. Others fix a
value whose default or meaning moved. Nothing to migrate means your file already says what this
version reads.

### `lev tools`

| Flag | Purpose |
|---|---|
| `--json` | Emit the inventory as JSON |

Lists and validates the global [Rhai tool scripts](/docs/rhai-tools) in `~/.leviath/tools/`.

### `lev approvals safe`

Print what runs without an approval prompt, and which file put each entry there. This is the answer
to "why did it not ask me".

| Flag | Purpose |
|---|---|
| `--agent <NAME>` | Include that agent's `[agent_safe_commands.<name>]` entries |
| `--json` | Emit the inventory as JSON |

There is no `list` or `clear`: nothing is persisted. A grant made at a prompt dies with the run that
made it, so the only durable state is the config this reports. See
[Human-in-the-loop](/docs/interaction) for what the entries mean.

### `lev policy`

Manage [taint tracking](/docs/security#taint-tracking-experimental) policy rules.

| Command | Flags | Purpose |
|---|---|---|
| `lev policy list` | | List current rules, static and scripted |
| `lev policy add <TOOL>` | `--target <PATTERN>`, `--max-sensitivity <public\|internal\|private>` (default `internal`) | Add an allowlist rule |
| `lev policy test <TOOL>` | `--target <PATTERN>`, `--taint <public\|internal\|private>` (default `private`) | Check whether a call would be gated |

### `lev yolo`

The profiles behind [`--yolo=<name>`](/docs/yolo): what you have, what one
says, and what it would decide. Every subcommand reads `yolo.toml` as it stands, the same way a
spawn does, so what it prints is what the next run gets.

| Command | Flags | Purpose |
|---|---|---|
| `lev yolo list` | `--json` | One line per profile: its default, the three human knobs, and how many rules of each kind it has |
| `lev yolo show <NAME>` | `--json` | The profile in full, as TOML, with what it keeps for a person |
| `lev yolo test <NAME> --tool <TOOL>` | `--command <LINE>`, `--args <JSON>`, `--workdir <DIR>`, `--configured <allow\|ask\|deny>`, `--kind <builtin\|subagent\|script\|mcp>`, `--allowed`, `--json` | What the profile would decide for one call, and which rule decided it |
| `lev yolo init` | `--force` | Write a commented example `yolo.toml` beside your config |

`test` is the same code path a run takes, so its answer is the run's answer. `--configured`
stands in for what the config layers resolve the tool to; left off, that is read from your
`config.toml`. `--allowed` decides as if `--allow <tool>` had been passed. A shell line is judged
with `--command`; any other tool takes its arguments as `--args '{"url": "..."}'`.

```bash
lev yolo init
lev yolo test careful --tool shell --command "rm -r target/debug"
lev yolo test careful --tool shell --command "cargo test && curl https://x" --json
lev run coder --yolo=careful -t "tidy the build"
```

## Environment

`LEVIATH_HOME` redirects the whole data root, and `LEVIATH_CONFIG_PATH` points at an exact config
file. Those two plus the rest are in the
[configuration reference](/docs/configuration#environment-variables).

Examples on this page use Unix shell syntax. On Windows, set variables the way your shell does:

```powershell
$env:LEVIATH_HOME = "D:\leviath"          # PowerShell
```

```bat
set LEVIATH_HOME=D:\leviath
```

The per-command Unix prefix form (`LEVIATH_HOME=/tmp/lev lev ps`) has no direct equivalent; set the
variable first, then run the command.
