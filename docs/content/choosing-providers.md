---
title: Choosing providers
description: Build lev, or the leviath crate, with only the model providers you use, and see what a build left out does.
group: Guides
group_order: 4
order: 8
---

# Choosing providers

Every provider Leviath supports is compiled into `lev` by default, including the ones you never
configure. If you build from source, you can leave out the ones you do not use. Each provider is a
cargo feature, so you name the providers you want and the build carries only those.

```bash
cargo install leviath-cli                                                    # every provider
cargo install leviath-cli --no-default-features --features anthropic,openai  # just these two
```

The release binaries, the Homebrew tap and the Docker image all carry every provider. Choosing is
for a build you make yourself.

## How choosing works

The default features turn on every provider. `--no-default-features` turns them all off, and
`--features` then names the ones to keep. With no features at all you get no vendor providers, and
only OpenAI-compatible endpoints from your config remain.

| Build | Command |
|---|---|
| Every provider | `cargo install leviath-cli` |
| Only some | `cargo install leviath-cli --no-default-features --features anthropic,google` |
| Scripted providers only | `cargo install leviath-cli --no-default-features --features rhai` |
| No providers | `cargo install leviath-cli --no-default-features` |

Turning the defaults off does not change anything else about `lev`. It keeps its allocator, and the
providers are the only thing the defaults choose.

## The features

| Feature | Provider | Config name |
|---|---|---|
| `anthropic` | Anthropic, with an API key | `anthropic` |
| `openai` | OpenAI with an API key, and `kind = "openai"` hosts such as Azure | `openai` |
| `openai-subscription` | OpenAI Codex, signed in with a ChatGPT subscription | `codex` |
| `xai` | xAI, with an API key | `xai` |
| `xai-subscription` | Grok, signed in with a SuperGrok or X Premium+ subscription | `grok` |
| `google` | Google Gemini | `google` |
| `openrouter` | OpenRouter | `openrouter` |
| `bedrock` | AWS Bedrock | `bedrock` |
| `meta` | Meta | `meta` |
| `ollama` | Ollama on your machine | `ollama` |
| `meshy` | Meshy 3D models | `meshy` |
| `rhai` | Providers you write as [Rhai scripts](/docs/rhai-providers) | your script's name |
| `providers` | All of the above | |

Grok runs on xAI's client, so `xai-subscription` brings `xai` with it. An
[OpenAI-compatible endpoint](/docs/providers#custom-openai-compatible-providers) needs no feature;
every build can reach one.

## In Docker

The image takes the same list as a build argument. Leave it empty for every provider.

```bash
docker build --build-arg LEV_FEATURES=anthropic,openai -t leviath .
```

## Embedding the library

The `leviath` crate takes the same feature names. Turn its defaults off and name the providers your
application uses.

```toml
[dependencies]
leviath = { version = "0.6", default-features = false, features = ["anthropic"] }
```

If you depend on `leviath-runtime` directly, choose providers on `leviath-providers` beside it. The
runtime turns none on by itself.

```toml
[dependencies]
leviath-runtime = "0.6"
leviath-providers = { version = "0.6", default-features = false, features = ["openai"] }
```

## When a build leaves a provider out

A config can name a provider your build does not carry. That happens when a fuller build wrote the
config, or when you share one config between machines. Nothing fails at startup. The daemon logs a
warning and skips that provider.

Everywhere else names the feature to rebuild with:

```console
$ lev doctor
  config     OK    ...  (note: provider "bedrock" is not built into this lev (rebuild with --features bedrock))

$ lev auth login codex
Error: provider "codex" is not built into this lev (rebuild with --features openai-subscription)
```

A run whose stage routes to that provider stops with the same advice. `lev setup` and the provider
lists offer only what the build carries, and `lev --version` names the providers when some are left
out.

```console
$ lev --version
lev 0.6.5
providers: anthropic, openai
```
