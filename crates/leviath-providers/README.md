# leviath-providers

LLM provider integrations for Leviath: Anthropic, OpenAI, Gemini, Ollama,
OpenRouter, and drop-in providers written in Rhai. Also holds the shared
retry, rate-limit, and tokenizer plumbing the providers have in common.

Part of [Leviath](https://github.com/GEMISIS/leviath), a structured
agent runtime for LLMs. Most applications should depend on the
[`leviath`](https://crates.io/crates/leviath) facade crate rather than this
one, and if you want the `lev` command-line tool, install
[`leviath-cli`](https://crates.io/crates/leviath-cli).

Docs live at [leviath.dev](https://leviath.dev). Licensed under the MIT
license.
