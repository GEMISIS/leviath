# leviath-providers

LLM provider integrations for Leviath: Anthropic, OpenAI (with an API key,
or OpenAI Codex signed in with a ChatGPT subscription), xAI (with an API key,
or Grok signed in with a subscription), Google Gemini, OpenRouter, AWS
Bedrock, Meta, Ollama, Meshy, any OpenAI-compatible endpoint, and drop-in
providers written in Rhai. Also holds the shared retry, rate-limit, and
tokenizer plumbing the providers have in common.

Each provider is a cargo feature, all on by default: `anthropic`, `openai`,
`openai-subscription`, `xai`, `xai-subscription`, `google`, `openrouter`,
`bedrock`, `meta`, `ollama`, `meshy` and `rhai`. With `default-features =
false`, name the ones you want. The model tables a gateway reads to describe
another vendor's models stay compiled either way.
[Choosing providers](https://leviath.dev/docs/choosing-providers) has more.

Part of [Leviath](https://github.com/GEMISIS/leviath), a structured
agent runtime for LLMs. Most applications should depend on the
[`leviath`](https://crates.io/crates/leviath) facade crate rather than this
one, and if you want the `lev` command-line tool, install
[`leviath-cli`](https://crates.io/crates/leviath-cli).

Docs live at [leviath.dev](https://leviath.dev). Licensed under the MIT
license.
