---
title: Managing your costs
description: What a run actually costs and why, how to watch it while it happens, and the knobs that bound it - each one with the measurement behind it.
group: Guides
group_order: 4
order: 2
---

# Managing your costs

A research run on this machine cost $75. The next one, same blueprint and same question,
cost $236. Nothing was broken either time.

This page is about why that happens and what to do about it. Every number below is measured
from finished runs in `~/.leviath/runs`, not estimated.

## A run's price is its headcount

Across four finished research runs on two blueprints:

| run | agents | cost | per agent |
| --- | --- | --- | --- |
| deep, first | 14 | $75.17 | $5.37 |
| deep, second | 42 | $236.34 | $5.63 |
| wide, first | 10 | $90.48 | $9.05 |
| wide, second | 25 | $190.91 | $7.64 |

Cost per agent varies by 1.7x. The number of agents varies by 4.2x. So the bill follows the
headcount, and the question "what will this cost" is really "how many agents will this spawn".

An agent that fans out spawns workers, and those workers can fan out again. Most of the money
in a research run is in the second generation: in the $236 run, 34 grandchildren accounted for
$198 of it, while the top-level agent itself cost $15.

## Watch it while it happens

The failure mode worth avoiding is finding out afterwards. A run that is quietly spending far
more than you intended looks, from outside, exactly like one making ordinary progress: it is
running, it is making tool calls, nothing is wrong.

```toml
[limits]
notify_spend_usd = [10, 25, 50, 100]
```

Each figure is announced once per run, the first time its total passes it, over the event
stream and in [the dashboard](/docs/dashboard). The event names the running total and the stage
that was running when it crossed, which is the stage doing the spending.

This is reporting, not a ceiling: it does not stop anything. Stopping a run mid-stage throws
away work, which is a different decision from wanting to know.

## Bound the headcount

```toml
[limits]
max_agents_per_run = 20
```

The number of agents one run may create, sub-agents included. A run that reaches the ceiling
stops widening: the workers already running finish, the merge happens on what came back, and
the report is written from that. It is not a failure, and nothing is cancelled. Stopping early
is a cheaper answer.

Counted from the run's root, so a worker deep in the tree cannot spend the whole budget on its
own branch. `0`, the default, is no ceiling.

At the measured $5 to $9 an agent, a ceiling of 20 is roughly a $100 to $180 run.

Two blueprint-side bounds work with it:

- **`max_child_depth`** in the `[graph]` table caps how deep the sub-agent tree goes. Depth 2
  means workers may fan out once more and their workers may not. Whoever starts the run can set
  it for that run with `launch.max_depth` (`lev run --max-depth`).
- **`max_items`** on a fan-out stage caps how many work items one split may produce.
  See [sub-agents and fan-out](/docs/sub-agents).

Both are the blueprint author's statements about the shape of the work. `max_agents_per_run` is
yours about the budget, and it applies to any blueprint you run.

## Spend less per agent

The per-token rates differ by more than 10x across the models a blueprint might name, so which
model runs which stage is the largest single lever after headcount. Rates below are dollars per
million tokens, as published on 2026-10-08:

| model | input | cached input | output |
| --- | --- | --- | --- |
| `claude-opus-5` | $5.00 | $0.50 | $25.00 |
| `gpt-5.5` | $5.00 | $0.50 | $30.00 |
| `claude-opus-5-5` | $4.00 | $0.20 | $20.00 |
| `claude-sonnet-5` | $2.00 | $0.20 | $10.00 |
| `claude-sonnet-5-5` | $2.00 | $0.10 | $10.00 |
| `gemini-3.1-pro` | $2.00 | $0.20 | $12.00 |
| `gemini-3.5-flash` | $1.50 | $0.15 | $9.00 |
| `claude-haiku-5-5` | $0.10 | $0.01 | $0.50 |

A research stage that reads a great deal and writes little is dominated by its input rate; a
stage that rewrites a whole report is dominated by output. So the expensive model belongs where
judgment matters and the cheap one where volume does. A blueprint names models per stage exactly
so this can be chosen rather than inherited:

```toml
[[graph.stages]]
name = "gather"
model = { models = [{ model = "gemini-3.5-flash" }, { model = "claude-sonnet-5" }] }

[[graph.stages]]
name = "synthesize"
model = { models = [{ model = "claude-opus-5" }, { model = "gpt-5.5" }] }
```

`lev run <blueprint> --check` prints which model each stage would run on your install, so a stage
that cannot run the one it leads with shows the one it fell back to. See
[providers](/docs/providers).

> [!NOTE]
> Cached input is a fifth to a tenth of the price of fresh input, and a multi-stage run re-sends
> most of its prompt every turn. On real runs, prompt caching took the cached share from 0% to
> between 60% and 81%. It is on by default where a provider supports it; there is nothing to
> configure, but it is why a stage's second visit costs so much less than its first.

## Keep the context from growing into the bill

Every turn re-sends the prompt, so a region that grows without bound is paid for on every
inference after it grows. [Structured context](/docs/context) is where this is governed:

- Percentage budgets scale with the model's window, so a stage that moves to a bigger model does
  not silently start sending four times as much.
- An edge's `carry` decides what crosses a stage boundary. Clearing a region the next
  stage does not read is the cheapest change available: a report-rewriting stage does not need
  the transcript of the research that produced it.
- A `max` in a region's budget (`budget = { percent = "30%", max = 60000 }`) is a ceiling, not a
  reservation. It caps what that percentage resolves to. On every model where it binds, it and not the
  percentage is what sizes the region. Reach for it only when the region's useful size does not
  grow with the window.

## Know what you actually paid

Every run records its own accounting in its [run file](/docs/run-file), and
`GET /api/runs/{id}` serves it. Every model call a run sends is in it, whether the call answered,
failed after it was sent, or was cut off by a deadline, a cancel or a daemon that stopped. Each
call's cost is in one of three classes:

- **reported**: the provider stated the amount it billed. OpenRouter and xAI do, on the reply.
- **computed**: the reply carried the call's whole usage, and Leviath priced it from the rate
  table. A figure Leviath worked out is never labelled as the provider's.
- **unknown**: neither could be had. Nothing is estimated in its place.

On the run record:

- `cost_usd` is what every call that has landed came to, or `null` when the cost of any of them
  is unknown. Never `0` for unknown: a total that leaves out what it could not learn looks
  authoritative and understates. While the run is going it is the figure so far.
- `calls_in_flight` counts the calls that are out and have not landed yet. They are in no other
  count and not in `cost_usd` until they land.
- `cost_is_exact` is `true` when `cost_usd` is the run's whole cost: every call was reported or
  computed, none is unknown and none is in flight.
- `reported_calls`, `computed_calls` and `unknown_calls` count each class, and `unknown` says
  why, by reason:

| reason | what it means |
| --- | --- |
| `no_usage` | the reply said nothing of what the call used: no token counts, or no length, size or count for a media call |
| `no_price` | no price is known for the model |
| `unmodelled_terms` | the call was billed on terms the rate table does not cover: a service tier, audio or image tokens, a server tool |
| `no_lookup` | the call failed or was cut off after it was sent, and the provider gives no way to look up what it charged |
| `lookup_unanswered` | the same, on a provider that keeps a record, which did not answer in time |
| `not_recorded` | an earlier release recorded the call without a cost |
| `not_settled` | the run ended with the call still out, and what it cost is not settled yet: a lookup is running, or was when the daemon stopped |

- `cost_priced_usd` is what the calls with a figure come to. It is kept while `cost_usd` is
  `null`, and it is not the run's cost.

`lev stages <run-id>` (or `GET /api/runs/{id}/stages`) breaks the same totals down per stage,
which is how you find the one stage that spent most of the run, and ends with the run's own cost
and what it rests on. `lev stages <run-id> --calls` lists every call with how it ended, its
class and its cost.

### A call that did not answer

A call that never left this machine cost nothing and is not counted. One that was sent is
settled where it ended, before the run moves on:

- The provider finished the call and said what it used before the failure (a reply that was
  then rejected): that usage is its cost.
- The provider knows the call by an id and keeps a record of it: the record is read. OpenRouter
  gives every call a generation id with its first header, and `GET /generation?id=` answers with
  the amount charged once OpenRouter has written the record, for a stream that was cut and for
  one that was cancelled. That is usually about six seconds after the call ends, and on a slow
  day it has been measured at twelve to more than twenty-four. The whole lookup, every request
  in it, ends within 15 seconds; a record that has not answered by then leaves the call
  `lookup_unanswered`, and a call cut before its first header arrived has no id to ask by and
  is `no_lookup`.
- The provider refused the request outright with 401, 402, 404 or 429, and nothing had begun:
  no reply had opened and no usage had arrived. Nothing was run, so nothing was billed. Google
  and OpenRouter say they do not charge any request they answer with an error, which holds at
  their own endpoints and not for an error from a proxy at a `base_url` of your own.
- Otherwise the cost is unknown. A status named part way through a reply that had started is
  not a refusal. A call that sent its request twice (a script provider that retries) has no
  whole figure. Anthropic, OpenAI, Bedrock and Meta give no way to read back what a cut stream
  was charged with an ordinary API key, and no figure is put in its place.

A media job is one cost however many attempts wait on it. An attempt that stops waiting while
the provider is still running the job records nothing, and stays on the run's list of calls out
as the job's account. Then one of two things happens, and the job is recorded once either way:

- An attempt picks the job up and reports. That report is the job's cost, and the call that
  stood for the job comes off the list with nothing recorded.
- The run gives the job up: another model answered in its place, the run ended or was
  cancelled, or the run was continued. The provider took the job, so the call that stood for it
  is recorded as cut off: `$0` on rates that charge nothing, and `no_lookup` otherwise, since a
  job is not a call a provider keeps a bill for. A finished run found still holding a job when
  the daemon starts is settled the same way. A fork takes none of its source's jobs.

### A call that is still out

While a run is going, a call of its that is out is in flight. It is counted in `calls_in_flight`
and in nothing else, `cost_usd` stays what the calls that landed came to, and `cost_is_exact`
is `false` until it lands. `lev stages` says "so far" and how many calls are in flight, and the
dashboard shows the figure with the count beside it. A sub-agent tree works the same way:
`subtree_cost_usd` is what has landed anywhere in the tree, with `subtree_calls_in_flight`
beside it.

A run that has ended waits on a call only to learn what it cost. Until that is settled the call
is `not_settled`, which is an unknown like any other: the run has no total and is not exact.
That lasts a few seconds while a lookup runs, and until the next start when the daemon stopped
in the middle of one:

- A finished or cancelled run stays loaded while a call of its own is in flight, and for up to
  30 seconds from the moment that call began to settle, so the cost lands on it. A paused run
  is not paged out while a call of its own is out either.
- A call that reports after its run was unloaded is written to the run's file, once.
- A daemon that stops, gracefully or not, does not wait for a lookup in flight: it drops it.
  The call stays on its run's list in the run file, with the provider's id. The next start
  settles it: when the run is brought back if it had not finished, and at start itself if it
  had. The turn a run sends again after a restart is a second call.

> [!WARNING]
> A run's cost is its own calls. A sub-agent or a fan-out worker is a run of its own, with its
> cost on its own record, and a parent's figure does not include it. Summing only the top-level
> run understates a fan-out badly: in the $236 run above, the top-level agent's own record said $15. Read the
> whole tree with `GET /api/runs/{id}/tree-status`, whose `subtree_cost_usd` adds up every run
> below this one, or with GraphQL `treeStatus { cost { costUsd } }`, which adds it up the same way.
> While workers are mid-call both are the figure so far, with `subtree_calls_in_flight`
> (`callsInFlight`) saying how many calls have yet to land.

## Where the prices come from

A computed cost is the reply's own usage at the published list price. The provider's bill is
still the figure that counts, and a negotiated rate belongs in the override below. Where a
figure comes from:

| provider | source | live or table | coverage |
|---|---|---|---|
| OpenRouter | the amount each call reports it was charged | live | every model it serves; reported. On a call made with your own upstream key the amount is OpenRouter's fee plus the upstream's charge, which the reply states beside it |
| OpenAI, Anthropic, Google, Meta | the vendor list prices as carried by OpenRouter's catalogue, cross-checked against LiteLLM's table | table, refreshed weekly by an automated PR | computed. A row prices the model it names and its dated snapshots (`gpt-5.5-2026-04-23`, `-latest`), and no other: a model that only shares a family name with a row has no price. The table is the vendor's own endpoint's: behind a `base_url` of your own, only the override below prices a call |
| xAI | the amount each call reports it was charged, then its own listings | live, with the table as the fallback | every model it lists; reported for a call that stated its cost, computed otherwise |
| image, video, speech and music models (OpenAI, Google, Bedrock, xAI, Meta) | LiteLLM's table, or the vendor's price page written in by hand where LiteLLM has none (a hand-written row wins) | table, refreshed with the rest; a hand-written row carries its check date | computed, per image, second of video, hour of audio, million characters or music clip that came back. A reply that names no quantity is unknown, and so is an image made at a quality or size the flat price is not for. See below |
| Ollama | free by design | neither | every model; a self-hosted cost belongs in the override below |
| Rhai script providers, OpenAI-compatible endpoints | the script's `list_models`, or a `[model_capabilities]` override | whichever the script gives | unknown unless one of those says otherwise |
| Meshy | none: Meshy bills in credits, whose dollar price depends on your plan | neither | unknown |

The table prices the ordinary service tier. A reply that says it was served on another tier
(`flex`, `priority`, `batch`), or that counts audio or image tokens, a server tool such as web
search, or a cache write kept for longer than the rate covers, is unknown rather than priced at
the list rate. Anthropic's hour-long cache write is priced: it is twice the input rate on every
model.

Some media models are billed by the token rather than by the unit, and those are priced by their
tokens. OpenAI's image models and Gemini's image and speech models work that way.

`lev models list` prints each model's input and output rate, a unit price such as `0.05/s` for a
media model, or `n/a` where nothing prices it. `lev models show` names the source of a table row
and the day the table was read.

The table is compiled into the build, so a build cannot notice a repricing between refreshes.
A refresh is the deterministic `cargo xtask prices`. It writes a row only where the two sources
agree within 5%. It keeps a disagreement as it was and prints it, and it refuses a move it does
not believe.

A negotiated rate never appears on a public page, so it belongs in the per-model override in your
config. That override wins over every table row. It is also the only place a self-hosted model's
cost can live.

```toml
[model_capabilities."my-model"]
input_per_mtok = 3.0
output_per_mtok = 15.0
```

## A long prompt can cost more per token

Some vendors bill a whole request at a higher rate once its prompt reaches a size. That size is
100 000 tokens on Claude Haiku 5.5, 200 000 on Gemini 2.5 Pro, Claude Sonnet 4 and the current
Grok models, and 272 000 on several GPT-5 models.
Leviath bills such a request at the higher rate. It does not let the higher rate decide which
model a stage uses, but it tells you:

- `lev models list` prints a `+` after the price of a model with a higher tier, with a footnote.
- `lev models show <model>` prints both rates and the threshold.
- `lev validate` adds a `long-context-price` note to a stage whose context can grow past the
  threshold. It is a note, not a warning: nothing needs to change unless the cost matters.

## A subscription has no per-call price

The Codex and Grok transports bill a subscription: a ChatGPT plan, or SuperGrok or X Premium+.
A call's marginal cost really is zero, so Leviath reports it as a known zero rather than as
unknown. A run on either lands in the report at no cost, which is accurate and not very useful.

The number that matters there is quota. Codex reports a rolling five-hour window and a weekly one
as percentages; Grok reports this week's on-demand credits and this month's use against the plan.
`lev auth status` and `lev providers quota` show them (`--json` for scripts), `lev doctor` warns
when a limit is reached, and `GET /api/providers` carries them for the Lair. Leviath also reads
them when the route rate-limits without saying how long to wait, so a run sleeps until the window
actually resets instead of backing off against a wall clock it cannot see.

Two consequences worth knowing. Caching does not engage on short prefixes at
all, so a small agent pays full price for every turn where a large one pays
about seven percent. And the first turn of each stage is always a miss, because
the cache key changes when the stage does, which is correct: the stage swap
changes the prefix.

## Speed is a separate knob

Wall-clock time is not cost, but a run that takes twice as long is twice as long to notice a
problem in. The inference pool is per model, and a fan-out whose workers all resolve to the same
model shares one pool between all of them:

```toml
[limits.max_concurrent_inferences_by_model]
"claude-sonnet-5" = 24
```

Measured on a 67-agent run where 65 agents resolved to one model: 9.9 inference turns a minute
against the default pool of 8. Widening the pool for the model a fan-out piles onto is the
throughput knob. See [the engine](/docs/engine) for how the pools work. A provider rate limit is a
different mechanism, configured under `[model_providers.<name>.rate_limit]`.

The bare name covers every route to that model, so the line above also sets the pool when the
same model is reached through a gateway that prefixes the vendor, as
`anthropic/claude-sonnet-5`. Write the full gateway id instead to keep the entry to that one
route, and an exact id beside a bare one wins for the route it names. Ollama size tags are left
alone: `qwen3.5:9b` and `qwen3.5:70b` are separate models with separate pools, which is what you
want, since the pool a 9b can afford is not the one a 70b can.

## Don't pay for the same tokens twice

A region that accumulates, the one tool results land in, is re-sent on every inference for the
rest of the stage. Whether you pay full price for it each time comes down to one field:

```toml
raw_findings = { kind = "temporary", budget = "30%", volatility = "grows" }
```

`volatility` defaults to `rewritten`, which tells the assembler the whole region changes every
turn, so none of it is cached. That is right for a scratchpad and wrong for an append-only pile
of fetched pages. On a measured run, a 280,000-token findings region left at the default cached
4% of the prompt: the same content re-sent, re-billed, and re-processed on every call, which is
latency as much as cost. Declared `grows`, the settled head caches and only the tail is new.

Size the region with the percentage and leave it there. A percentage is the mechanism for
scaling to the model in front of you. 30% is 60,000 tokens on a 200K-token model and 300,000 on
a 1M-token one, and both are 30% of what that model can hold.

It is tempting to add an absolute `max` alongside it as insurance. Resist it unless you
mean the cap to be the real limit, because that is what it becomes. A ceiling low enough to
matter binds on every model above the window it was chosen for. From there up, the percentage
decides nothing. A region that resolves to the same number on a 200K model and a 1M one is not
percentage-sized at all. If your region is too big, the percentage is the number to change.

The exception is a region whose useful size genuinely does not grow with the window, such as a
fixed list or a seeded constant. Those are the ones a fixed `max` is for.

See [structured context](/docs/context) for the full set of region fields.

## A short checklist

1. Set `notify_spend_usd` so a run tells you what it is doing.
2. Set `max_agents_per_run` if an unbounded fan-out would be a problem on your account.
3. Check `lev run <blueprint> --check` names the models you meant, especially on the expensive
   stages.
4. Put the expensive model where judgment happens, not where volume does.
5. Clear regions the next stage does not read.
6. Read the whole tree when you add up what a fan-out cost.
