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

- **`max_child_depth`** on the `[agent]` table caps how deep the sub-agent tree goes. Depth 2
  means workers may fan out once more and their workers may not.
- **`max_items`** on a `mode = "fan_out"` stage caps how many work items one split may produce.
  See [sub-agents and fan-out](/docs/sub-agents).

Both are the blueprint author's statements about the shape of the work. `max_agents_per_run` is
yours about the budget, and it applies to any blueprint you run.

## Spend less per agent

The per-token rates differ by more than 10x across the models a blueprint might name, so which
model runs which stage is the largest single lever after headcount. Rates below are dollars per
million tokens, as published on 2026-08-23:

| model | input | cached input | output |
| --- | --- | --- | --- |
| `claude-opus-5` | $5.00 | $0.50 | $25.00 |
| `gpt-5.5` | $5.00 | $0.50 | $30.00 |
| `claude-sonnet-5` | $2.00 | $0.20 | $10.00 |
| `gemini-3.1-pro` | $2.00 | $0.20 | $12.00 |
| `gemini-3.5-flash` | $1.50 | $0.15 | $9.00 |

A research stage that reads a great deal and writes little is dominated by its input rate; a
stage that rewrites a whole report is dominated by output. So the expensive model belongs where
judgment matters and the cheap one where volume does. A blueprint names models per stage exactly
so this can be chosen rather than inherited:

```toml
[stages.gather.model]
models = ["gemini-3.5-flash", "claude-sonnet-5"]

[stages.synthesize.model]
models = ["claude-opus-5", "gpt-5.5"]
```

`lev validate <blueprint>` prints which model each stage would run on your install, and says so
when a stage cannot run the one it leads with. See [providers](/docs/providers).

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
- An edge `transform` decides what crosses a stage boundary. A `clear` on a region the next
  stage does not read is the cheapest change available: a report-rewriting stage does not need
  the transcript of the research that produced it.
- `max_tokens` on a region is a ceiling, not a reservation. Alongside a percentage budget it
  caps what that percentage resolves to - so on every model where it binds, it and not the
  percentage is what sizes the region. Reach for it only when the region's useful size does not
  grow with the window.

## Know what you actually paid

Every run records its own accounting in `~/.leviath/runs/<run-id>/meta.json`:

- `cost_usd` is the total, or `null` when some call could not be priced. Never `0` for unknown:
  a total that silently omits what it could not price looks authoritative and understates.
- `unpriced_calls` counts those. Non-zero means the real figure is higher by an unknown amount.
- `cost_is_exact` says whether the priced calls carried the provider's own figure rather than
  one reconstructed from published rates.

`stages.json` breaks the same totals down per stage, which is how you find the one stage that
spent most of the run.

> [!WARNING]
> A sub-agent's cost is on the sub-agent's own record. Summing only the top-level run understates
> a fan-out badly: in the $236 run above, the top-level agent's own record said $15. Add up the
> tree, following `children` in each `meta.json`.

OpenRouter quotes every model's rate in its listing, so a run through it is priced from what the
gateway said that day, and `lev models list` prints those rates beside each model. Anthropic, OpenAI
and Google publish no rate through their APIs, so theirs are transcribed into this build and dated;
`lev models show` says which kind it is printing. Rates for models with neither come from
`[model_capabilities]` in your config, which is also the only place a negotiated rate or a
self-hosted model's cost can live:

```toml
[model_capabilities."my-model"]
input_per_mtok = 3.0
output_per_mtok = 15.0
```

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
throughput knob; see [the engine](/docs/engine) for how the pools work, and note that a provider
rate limit is a different mechanism configured under `[model_providers.<name>.rate_limit]`.

The bare name covers every route to that model, so the line above also sets the pool when the
same model is reached through a gateway that prefixes the vendor, as
`anthropic/claude-sonnet-5`. Write the full gateway id instead to keep the entry to that one
route, and an exact id beside a bare one wins for the route it names. Ollama size tags are left
alone: `qwen3.5:9b` and `qwen3.5:70b` are separate models with separate pools, which is what you
want, since the pool a 9b can afford is not the one a 70b can.

## Don't pay for the same tokens twice

A region that accumulates - the one tool results land in - is re-sent on every inference for the
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
scaling to the model in front of you: 30% is 60,000 tokens on a 200K-token model and 300,000 on
a 1M-token one, and both are 30% of what that model can hold.

It is tempting to add an absolute `max_tokens` alongside it as insurance. Resist it unless you
mean the cap to be the real limit, because that is what it becomes: a ceiling low enough to
matter binds on every model above the window it was chosen for, and from there up the percentage
decides nothing. A region that resolves to the same number on a 200K model and a 1M one is not
percentage-sized at all. If your region is too big, the percentage is the number to change.

The exception is a region whose useful size genuinely does not grow with the window - a fixed
list, a seeded constant. Those are the ones `max_tokens` is for.

See [structured context](/docs/context) for the full set of region fields.

## A short checklist

1. Set `notify_spend_usd` so a run tells you what it is doing.
2. Set `max_agents_per_run` if an unbounded fan-out would be a problem on your account.
3. Check `lev validate` names the models you meant, especially on the expensive stages.
4. Put the expensive model where judgment happens, not where volume does.
5. Clear regions the next stage does not read.
6. Read the whole tree when you add up what a fan-out cost.
