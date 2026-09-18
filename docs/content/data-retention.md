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
everywhere. With the switch on, a model that would keep something is never sent a request. That
covers a stage's request, the run's title call, a compaction summary, `lev test` and the
`lev doctor` probe. Each is refused in the same words.

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
before every spawn while zero retention is on. `lev providers retention` prints both. It names the
models never served under
`none`, and any unavailable to your account with Bedrock's reason:

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

The switch is `[providers] zero_retention = true` in `config.toml`. `lev providers retention set
zero` writes it. The setup wizard's **Zero data retention** row writes it too, and so does
`lev setup --zero-retention true`:

| Provider | With the switch on |
|---|---|
| Bedrock | Account mode set to `none`; a model never served under `none` is refused |
| OpenAI | `store = false` on every request; the abuse log stays unless you declare an agreement |
| OpenRouter | `provider.zdr = true` and `data_collection = "deny"`; a model with no ZDR endpoint is refused |
| Anthropic, Google, xAI | Refused unless you declare an agreement |
| Meta | Refused, the standard models and a `-contributor` model alike |
| local models | Nothing to do; nothing leaves the machine |
| Meshy, Codex, Grok, Claude Code | Refused; the policy is fixed |
| Every provider with a Files API | Nothing is uploaded; parts go inline, within each provider's inline limits |

Meta publishes no retention window for its standard models, so the switch refuses them. A
`-contributor` model is refused outright, because Meta trains on it.

A stage is judged by the model it would start on. Its fallbacks are judged the same way, and one
that keeps something is dropped from failover, with a line in the stage's log saying so. Nothing
is rerouted: an author who pinned a model would not see it swapped for one at another vendor.

The switch holds past the spawn too. A run's title is written by the first model in its title
chain that keeps nothing, and a blueprint whose `compaction_config` names a model that keeps
something is refused at spawn. Turning the switch on under a running daemon refuses the next call
of a run already going, which then ends with the reason rather than sending it.

`lev validate` says the same thing before a run does. A stage whose model would be refused is a
`retention-not-zero` error, and a fallback that would be dropped is a `retention-fallback-dropped`
warning, each carrying the provider's reason. The
[lint reference](/docs/cli#lev-validate-path) lists both.

### Files uploaded to a provider

With the switch off, a large image, PDF, video or recording is uploaded to the provider's file
storage and named by id on later requests (see [Files and size limits](/docs/mime#files-and-size-limits)).
That upload is kept at the provider until the run ends or the upload's lifetime passes, a day by
default. It is data the provider keeps, so the switch turns uploads off, and
`[providers] file_uploads = false` turns them off without the switch. Anthropic's Files API is not
eligible for zero data retention in any case.

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

## Proxies, gateways and Azure

Many organisations reach a provider through a gateway of their own, which holds the real key,
strips retention by contract, and wants a token or a tag of its own on every request. Point the
provider at it with `<provider>_base_url`, and give it what it wants with `<provider>_headers`.
Declare the zero retention the gateway provides as an agreement, since no API can read it:

```toml
[providers]
anthropic_base_url = "https://llm-gateway.corp.example/anthropic"
anthropic_api_key  = "gateway-placeholder"
anthropic_headers  = { X-Gateway-Token = "...", X-Cost-Centre = "research" }
zero_retention = true
zero_retention_agreements = ["anthropic"]
```

Azure OpenAI is an OpenAI-compatible endpoint with its own header and its own retention terms.
Azure keeps prompts up to 30 days for abuse monitoring unless the exemption is approved for your
subscription, so `retention = "zero"` is yours to declare only then. `zero_retention_request`
tells Leviath to send it OpenAI's `store = false` with the switch on. It would otherwise withhold
that field from an endpoint, because a llama.cpp server would reject it:

```toml
[model_providers.azure]
kind      = "openai-compatible"
base_url  = "https://my-resource.openai.azure.com/openai/v1"
headers   = { api-key = "..." }
serves    = ["gpt-5.5"]
retention = "zero"
zero_retention_request = "openai"
```

The exact keys are on the [configuration page](/docs/configuration#reaching-a-provider-through-a-gateway).

## What this cannot do

It cannot read a contract, so a declared agreement is trusted. It cannot see a provider's internal
logs, so the table is what each provider documents, dated on the
[providers page](/docs/providers#data-retention). And it cannot lower a floor: a model that
retains regardless is refused under the switch, never sent. If you need a guarantee stronger than
a provider's published policy, the answer is a local model, which keeps nothing because nothing
leaves the machine.

The exact keys, flags and command forms are on the [providers](/docs/providers#data-retention),
[configuration](/docs/configuration#providers) and [CLI](/docs/cli#lev-providers) pages.
