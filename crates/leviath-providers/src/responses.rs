//! The OpenAI Responses protocol, shared by every provider that speaks it.
//!
//! Codex (a ChatGPT subscription), xAI and Grok (an xAI API key or a Grok
//! subscription), Meta's Model API and OpenAI's own API all take the same
//! request root (`input` items, `instructions`, flat `function` tools) and
//! answer with the same event stream. What differs between them is a short
//! list of rules, and a [`Dialect`] is that list: which parameters a route
//! refuses, whether it takes an output cap or a temperature, and whether its
//! usage block carries a cost.
//!
//! Every dialect sends `store: false`. A stored response is kept on the
//! vendor's servers (thirty days or more) and is not needed: Leviath replays
//! the conversation itself, reasoning included, on every turn.

pub mod client;
pub mod reasoning;
pub mod request;
pub mod stream;

/// The rules one route follows. A `const` per provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dialect {
    /// The registry name reasoning blobs are sealed under, so a blob is only
    /// ever replayed to the provider that minted it.
    pub provider: &'static str,
    /// Top-level keys the route answers `400 Unsupported parameter` to,
    /// removed after a stage's `[model.parameters]` are merged in.
    pub rejected_parameters: &'static [&'static str],
    /// Whether `max_output_tokens` is sent from the request's output cap.
    pub output_cap: bool,
    /// Whether `temperature` is sent from the request.
    pub temperature: bool,
    /// Whether `text.verbosity` is sent.
    pub verbosity: bool,
    /// Whether a reasoning summary is asked for beside the effort.
    pub reasoning_summary: bool,
    /// Whether the usage block's `cost_in_usd_ticks` is recorded as the
    /// call's cost. Off for a subscription, whose marginal cost is zero
    /// whatever list price the route quotes.
    pub reported_cost: bool,
    /// Whether `prompt_cache_key` is sent.
    pub cache_key: bool,
}

/// One United States dollar in xAI's `cost_in_usd_ticks` unit. Measured
/// against a live image generation priced at $0.02, which reported
/// `200000000` ticks.
pub const TICKS_PER_USD: f64 = 10_000_000_000.0;
