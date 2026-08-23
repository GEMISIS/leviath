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
- `max_tokens` on a region is a ceiling, not a reservation. Setting one alongside a percentage
  budget caps the region at the smaller of the two.

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

Rates for models with no published price come from `[model_capabilities]` in your config, which
is also the only place a negotiated rate or a self-hosted model's cost can live:

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

## A short checklist

1. Set `notify_spend_usd` so a run tells you what it is doing.
2. Set `max_agents_per_run` if an unbounded fan-out would be a problem on your account.
3. Check `lev validate` names the models you meant, especially on the expensive stages.
4. Put the expensive model where judgment happens, not where volume does.
5. Clear regions the next stage does not read.
6. Read the whole tree when you add up what a fan-out cost.
