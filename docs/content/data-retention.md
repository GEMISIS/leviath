---
title: Data retention
description: What each provider keeps of your prompts and replies, how to ask for zero retention, and how Leviath refuses a model that cannot give it.
group: Concepts
group_order: 2
order: 13
---

# Data retention

Every prompt a run sends carries your task, your files, and whatever the agent read along the
way. Once the reply is back, the provider may keep a copy for days, for abuse review, or for
nothing at all, and nothing in the request tells you which. Leviath keeps a table of what each
provider keeps, reads the settings a provider exposes, and lets you ask for zero retention
everywhere. With the switch on, a stage whose model would keep something is refused before a
request goes out, in the provider's own words.

```bash
lev providers retention              # what each configured provider keeps, and who controls it
lev providers retention set zero     # ask everywhere; refuse a model that cannot give it
lev validate ./my-agent              # says which stages the switch would refuse
lev run my-agent --task "..."        # refused at spawn if a stage's model keeps anything
```

## The terms

**Retention** is how long a provider keeps a request's prompt and reply after answering. A
provider that keeps nothing once the reply is returned offers **zero data retention**, often
written ZDR. A retention of 30 days usually means an abuse-monitoring log that the provider
reviews and deletes. It is separate from training: none of the shipped providers train on API
traffic, whatever they keep.

**Who controls it** differs by provider, and that decides what Leviath can do about it:

| Control | Meaning | Providers |
|---|---|---|
| per request | A field on each request asks for it | OpenAI (`store`), OpenRouter (`provider.zdr`) |
| account setting | An API reads and writes it | Bedrock (`data-retention` mode) |
| agreement | A contract with the provider, which no API can read | Anthropic, OpenAI, Google |
| fixed | Nothing to set; the policy is what it is | Meshy, local models, subscription transports |

## How to think about it

Zero retention is a property of a model at a provider, not of Leviath. Leviath can send the
request field, set the account mode, and refuse a model that cannot give it. It cannot make a
provider keep less than its floor. Three things follow.

**Some models keep data whatever you ask.** Claude Fable 5 and 5.1, and Claude Mythos 5 and 5.1,
keep prompts and replies 30 days for safety review on every platform that serves them. On
Bedrock, every OpenAI model is served only under modes that keep something. With the switch on,
a stage that names one of these is refused. Name another model or turn the switch off; there is
no third option.

**An agreement is taken at your word.** Anthropic, OpenAI and Google grant zero retention by
contract, and no API reports whether you hold one. You declare it in
`[providers] zero_retention_agreements`, and the provider then counts as keeping nothing. Declare
only what your organisation has signed.

**A gateway keeps what its upstream keeps.** OpenRouter keeps nothing itself unless you turn on
prompt logging, and routes to endpoints run by other vendors. With the switch on, Leviath asks it
to use only endpoints with a zero-retention policy, and refuses a model that has none rather than
let OpenRouter route it elsewhere.

## Bedrock, model by model

Bedrock is the one provider where retention is both an account setting and a per-model fact.
The account has a **data retention mode**: `none` keeps nothing, `default` leaves each model to
its own policy, and `aws_review` lets AWS keep flagged content up to 30 days for human review. An
account set to `inherit` serves each model under that model's own default, which is `default`
for most.

Each model also says which modes it may be served under. A model that never allows `none`
cannot run with zero retention on Bedrock at all, and under an account set to `none` Bedrock
reports it unavailable. Access is per model too: a model your account has no grant for is
unavailable whatever the mode, and Bedrock says why.

Leviath reads the account mode and the per-model list when it starts, and reads the mode again
before every spawn while zero retention is on. `lev providers retention` prints both, naming the
models never served under
`none` and any unavailable to your account with Bedrock's reason:

```text
  bedrock      zero (account setting, read from the account)
               account data retention mode: none (read just now)
               never served under mode none, so never with zero retention: openai.gpt-5.4, openai.gpt-5.5
               unavailable to this account as things stand: anthropic.claude-fable-5 (This model is not available under data retention mode 'none'.)
```

`lev providers retention set zero` sets the account mode to `none` as well as writing the switch.
`lev providers retention bedrock <mode>` sets the mode alone. While the switch is on, a running
daemon reads the mode again before every spawn, so either change is in force at once.

## What the switch does

`[providers] zero_retention = true` in `config.toml`, written by `lev providers retention set
zero`, the setup wizard's **Zero data retention** row, or `lev setup --zero-retention true`:

| Provider | With the switch on |
|---|---|
| Bedrock | Account mode set to `none`; a model never served under `none` is refused |
| OpenAI | `store = false` on every request; the abuse log stays unless you declare an agreement |
| OpenRouter | `provider.zdr = true` and `data_collection = "deny"`; a model with no ZDR endpoint is refused |
| Anthropic, Google | Refused unless you declare an agreement |
| local models | Nothing to do; nothing leaves the machine |
| Meshy, Codex, Claude Code | Refused; the policy is fixed |

A stage is judged by the model it would start on. Its fallbacks are judged the same way, and one
that keeps something is dropped from failover, with a line in the stage's log saying so. Nothing
is rerouted: an author who pinned a model would not see it swapped for one at another vendor.

`lev validate` says the same thing before a run does. A stage whose model would be refused is a
`retention-not-zero` error, and a fallback that would be dropped is a `retention-fallback-dropped`
warning, each carrying the provider's reason. The
[lint reference](/docs/cli#lev-validate-path) lists both.

## Reading the answer

Every answer has three parts: what is kept, who controls it, and where the answer came from.

```text
lev models show claude-sonnet-5
  Retention   30 days (by agreement, documented)
              Anthropic's commercial API keeps prompts and outputs up to 30 days ...
```

`documented` is the table this build ships. `read from the account` is a setting Leviath read
live, which only Bedrock offers. `requested per request` is the field sent with the switch on.
`declared agreement` is one you wrote in the config. `config override` is a `retention` key you
set on a `[model_capabilities.<model>]` or `[model_providers.<name>]` entry, which wins over
everything else and is how you tell Leviath about a custom host it cannot know.

## What this cannot do

It cannot read a contract, so a declared agreement is trusted. It cannot see a provider's internal
logs, so the table is what each provider documents, dated on the
[providers page](/docs/providers#data-retention). And it cannot lower a floor: a model that
retains regardless is refused under the switch, never sent. If you need a guarantee stronger than
a provider's published policy, the answer is a local model, which keeps nothing because nothing
leaves the machine.

The exact keys, flags and command forms are on the [providers](/docs/providers#data-retention),
[configuration](/docs/configuration#providers) and [CLI](/docs/cli#lev-providers) pages.
