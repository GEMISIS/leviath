//! `[limits]` and `[webhook]`: the ceilings a run is bounded by.
//!
//! Concurrency caps, timeouts, retention, the stall and wedge watchdogs, and the
//! webhook retry schedule. The `default_*` functions are serde defaults for the
//! fields below them and are kept beside those fields deliberately: a default
//! that drifts from the field it fills is invisible until someone reads a
//! config that omits it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

fn default_max_concurrent_inferences() -> Option<usize> {
    Some(8)
}

fn default_default_max_iterations() -> Option<usize> {
    Some(50)
}

fn default_max_concurrent_tools() -> usize {
    8
}

/// Seconds a script tool's HTTP request may take before it is abandoned.
fn default_script_http_timeout_secs() -> u64 {
    30
}

/// Concurrent script-tool HTTP requests allowed against any one host.
///
/// Four, not unbounded. A single research stage routinely batches six fetches,
/// and a fan-out multiplies that by the worker count, so one agent could open
/// nearly two hundred simultaneous connections to the same origin. That reads
/// as an attack: `investors.cerebras.ai` answered such a burst by failing every
/// HTTP/2 stream, and a run lost both of its primary sources to it. Batching
/// still happens - this only staggers the requests that share a host, which is
/// the only dimension where more concurrency stopped buying speed.
fn default_script_http_max_per_host() -> usize {
    4
}

fn default_script_shell_timeout_secs() -> u64 {
    60
}

fn default_stall_timeout_secs() -> u64 {
    leviath_runtime::pipeline::DEFAULT_STALL_TIMEOUT_SECS
}

fn default_dead_cycles_before_relief() -> u32 {
    leviath_runtime::host::DEFAULT_DEAD_CYCLES_BEFORE_RELIEF
}

pub(crate) fn default_mcp_idle_disconnect_secs() -> u64 {
    crate::daemon::mcp_pool::DEFAULT_MCP_IDLE_DISCONNECT_SECS
}

fn default_finished_retention_secs() -> u64 {
    leviath_runtime::host::DEFAULT_FINISHED_RETENTION_SECS
}

fn default_wedge_timeout_secs() -> u64 {
    leviath_runtime::pipeline::DEFAULT_WEDGE_TIMEOUT_SECS
}

fn default_provider_failures_before_open() -> u32 {
    leviath_runtime::pipeline::DEFAULT_FAILURES_BEFORE_OPEN
}

fn default_provider_circuit_cooldown_secs() -> u64 {
    leviath_runtime::pipeline::DEFAULT_CIRCUIT_COOLDOWN_SECS
}

fn default_interaction_timeout_secs() -> u64 {
    leviath_runtime::interaction_hub::DEFAULT_INTERACTION_TIMEOUT_SECS
}

fn default_inference_retry_attempts() -> u32 {
    leviath_runtime::DEFAULT_RETRY_ATTEMPTS
}

fn default_inference_retry_base_ms() -> u64 {
    leviath_runtime::DEFAULT_RETRY_BASE_DELAY_MS
}

/// Runtime resource limits with safe defaults baked in.
///
/// Both fields default to a bounded value so a fresh install can't accidentally
/// run unbounded inference concurrency or an unbounded agent loop. Set a field
/// explicitly in `[limits]` to raise or lower it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    /// Global fallback cap on concurrent inference requests for any model
    /// without its own entry in `max_concurrent_inferences_by_model`. Defaults
    /// to `Some(8)`; omit or set a large number to effectively unbound it.
    ///
    /// One physical bound sits behind this for *script* providers: each of
    /// their in-flight calls occupies a blocking-pool thread, and the daemon's
    /// runtime provisions 2048 of those. Pools above 2048 only run that wide
    /// for HTTP providers, whose calls are fully async.
    #[serde(default = "default_max_concurrent_inferences")]
    pub max_concurrent_inferences: Option<usize>,

    /// Size of the shared tool-execution worker pool - the number of agents whose
    /// tool batches may run concurrently across the whole daemon (the tool-lane
    /// counterpart of `max_concurrent_inferences`). Defaults to `8`. Clamped to at
    /// least 1.
    #[serde(default = "default_max_concurrent_tools")]
    pub max_concurrent_tools: usize,

    /// Fallback `max_iterations` applied to a stage that does not set its own,
    /// so an agent can't loop forever with no completion signal. Defaults to
    /// `Some(50)`. A stage's explicit `max_iterations` always wins.
    #[serde(default = "default_default_max_iterations")]
    pub default_max_iterations: Option<usize>,

    /// Opt-in exact pre-inference token budgeting. When `true`, each agent
    /// inference is preceded by an exact token count of the assembled request
    /// (via the provider's `count_tokens`, which uses a remote endpoint for
    /// Anthropic/Gemini and a local heuristic otherwise) and is rejected before
    /// sending if it would exceed the model's context window. Off by default:
    /// normal budgeting uses cheap local estimates, and this adds a network
    /// round-trip per inference for providers with a remote count endpoint.
    #[serde(default)]
    pub exact_token_counting: bool,

    /// Wall-clock timeout (seconds) for a Rhai script tool's `shell()` host call,
    /// mirroring the built-in shell tool's own 60-second cap so a script can't
    /// hang an agent on a runaway command. Defaults to `60`.
    #[serde(default = "default_script_shell_timeout_secs")]
    pub script_shell_timeout_secs: u64,

    /// Seconds a script tool's HTTP request may take. Defaults to `30`.
    #[serde(default = "default_script_http_timeout_secs")]
    pub script_http_timeout_secs: u64,

    /// Concurrent script-tool HTTP requests allowed per host. Defaults to `4`;
    /// `0` means unbounded.
    ///
    /// Four, not unbounded: a research stage routinely batches six fetches and a
    /// fan-out multiplies that by the worker count, so one run could open nearly
    /// two hundred simultaneous connections to a single origin. Batching is
    /// untouched by this; only the requests that share a host stagger.
    #[serde(default = "default_script_http_max_per_host")]
    pub script_http_max_per_host: usize,

    /// How long (seconds) a run may sit ready to work but unable to dispatch
    /// before it is failed instead of left running.
    ///
    /// This only ever fires for something the runtime cannot resolve on its own -
    /// today, a stage whose provider is not configured. Waiting for a busy
    /// model's inference pool is ordinary backpressure and is never failed, no
    /// matter how long it takes. Defaults to `60`; `0` disables the watchdog and
    /// restores the old behaviour of waiting indefinitely.
    ///
    /// Read once at daemon start, so a change needs a daemon restart.
    #[serde(default = "default_stall_timeout_secs")]
    pub stall_timeout_secs: u64,

    /// How many consecutive safety re-drives may find the tool lane full and no
    /// run moving before the daemon widens the lane to break the jam.
    ///
    /// The daemon re-drives itself every 30 seconds, so the default of `10` is
    /// five minutes of a full lane going nowhere. Relief only ever *adds*
    /// capacity, never cancels anything, and is capped at one extra lane's worth
    /// over the daemon's life, so it cannot run away.
    ///
    /// `0` turns relief off. Detection and reporting stay on either way, so
    /// `lev ps` and the metrics still show the streak.
    ///
    /// Read once at daemon start, so a change needs a daemon restart.
    #[serde(default = "default_dead_cycles_before_relief")]
    pub dead_cycles_before_relief: u32,

    /// How long (seconds) a run keeps its place in `lev ps` after the daemon
    /// unloads it from memory.
    ///
    /// A terminal run used to leave the listing the moment it was unloaded,
    /// which made a run that died on its first inference look exactly like a run
    /// that had never been spawned. A scheduler polling the listing could only
    /// tell the two apart with a stopwatch, and issue #205 is what that cost:
    /// forty minutes of spawning work, timing out, and spawning it again.
    ///
    /// Defaults to `300`. `0` drops a run as soon as it finishes, which is the
    /// old behaviour. The record lives in memory, so a restart clears it
    /// whatever this is set to.
    ///
    /// Read once at daemon start, so a change needs a daemon restart.
    #[serde(default = "default_finished_retention_secs")]
    pub finished_retention_secs: u64,
    /// How long (seconds) a per-agent MCP server may sit with zero live runs
    /// leasing it before the daemon disconnects it (ending a stdio server's
    /// child process). Long enough that back-to-back runs of a blueprint reuse
    /// the warm connection; the next run that declares the server reconnects
    /// lazily. `0` keeps every server connected for the daemon's life, which
    /// was the old behaviour. Global `[[mcp_servers]]` from config.toml are
    /// never disconnected regardless.
    ///
    /// Read once at daemon start, so a change needs a daemon restart.
    #[serde(default = "default_mcp_idle_disconnect_secs")]
    pub mcp_idle_disconnect_secs: u64,
    /// How long (seconds) a run may sit in a state no part of the engine can
    /// reach before it is failed instead of left reported as running.
    ///
    /// Not a general "this run looks slow" timeout, and never fires on one. An
    /// agent waiting on the model, on a tool, on its sub-agents, or on a person
    /// is holding the marker that says so, and is exempt however long it takes.
    /// This only catches an agent holding *no* marker at all, which the engine's
    /// own invariants say cannot happen and which nothing will ever look at
    /// again. Such a run stays `running` in `meta.json` for the life of the
    /// daemon and keeps whatever capacity an external scheduler assigned it,
    /// which is issue #202.
    ///
    /// Defaults to `0`, which is off: this fails runs, and an upgrade that
    /// starts killing work nobody asked it to kill is worse than the leak. `300`
    /// is a reasonable value to set. Turning it on is also a way to find out
    /// whether it is happening to you, since it says so in the log and in the
    /// run's error.
    ///
    /// Read once at daemon start, so a change needs a daemon restart.
    #[serde(default = "default_wedge_timeout_secs")]
    pub wedge_timeout_secs: u64,
    /// How many consecutive provider-fatal failures (out of credits, rejected
    /// key) take a provider out of service for every run.
    ///
    /// Defaults to `3`. One 402 can just be a request asking for more output
    /// tokens than the balance covers; three in a row is the account. While a
    /// provider is out, runs move to their next candidate, and runs with none
    /// left are failed by the stall watchdog rather than left "running".
    ///
    /// `0` disables the breaker, leaving per-run failover on its own.
    ///
    /// Read once at daemon start, so a change needs a daemon restart.
    #[serde(default = "default_provider_failures_before_open")]
    pub provider_failures_before_open: u32,

    /// How long a provider stays out of service before one request is let
    /// through to see whether it recovered.
    ///
    /// Defaults to `300` (five minutes). That probe either succeeds, which puts
    /// the provider straight back into service, or fails and restarts the wait,
    /// so topping up an account brings the factory back with no restart.
    ///
    /// Read once at daemon start, so a change needs a daemon restart.
    #[serde(default = "default_provider_circuit_cooldown_secs")]
    pub provider_circuit_cooldown_secs: u64,
    /// How long (seconds) a prompt may go unanswered before the daemon resolves
    /// it itself and lets the run carry on.
    ///
    /// Covers every prompt that waits on a person: an agent's `ask_user_*` /
    /// `present_for_review` call, a tool-approval prompt, a taint gate, and a
    /// blueprint interaction point. Before this existed, a run whose operator
    /// had walked away sat in `WaitingInput` holding its slot until the daemon
    /// restarted - hours, in the report that prompted it (issue #204).
    ///
    /// Expiry resolves the prompt exactly as cancelling it would: a tool
    /// approval and a taint gate **deny**, an `ask_user_*` call is told nobody
    /// answered, and an interaction point proceeds with no user text. Nothing is
    /// approved on the strength of a timeout.
    ///
    /// Defaults to `3600` (one hour); `0` waits indefinitely.
    ///
    /// Read once at daemon start, so a change needs a daemon restart.
    #[serde(default = "default_interaction_timeout_secs")]
    pub interaction_timeout_secs: u64,

    /// How many times an inference is attempted, the first try included,
    /// before the agent is failed with whatever the provider last said.
    ///
    /// Only a transient failure is retried at all - a reset connection, a
    /// timeout, a 429, a 5xx. An authentication error or an over-long request
    /// fails on the first answer, since the second would be identical.
    ///
    /// Defaults to `4`, which is one try and three retries. `1` turns retrying
    /// off. The wait between retries is `inference_retry_base_ms`, doubling each
    /// time, so the default schedule is 1s, 2s, 4s.
    ///
    /// Raising it lengthens how long a run rides out a provider **overload**
    /// (an Anthropic 529, or a 429), which is retried on its own much slower
    /// schedule of 15s, 30s, then 60s per further attempt - that case is why the
    /// key exists (issue #417). Whatever this is set to, the retries of one
    /// request may sleep at most five minutes in total.
    ///
    /// Read once at daemon start, so a change needs a daemon restart.
    #[serde(default = "default_inference_retry_attempts")]
    pub inference_retry_attempts: u32,

    /// The wait before the first inference retry, in milliseconds, doubling for
    /// each retry after it. Defaults to `1000`, so the schedule is 1s, 2s, 4s.
    ///
    /// This is the *blip* schedule and is meant to stay short: a reset
    /// connection or a 500 is usually gone by the next attempt. A provider
    /// overload does not use it - see `inference_retry_attempts` - so raising
    /// this to wait out an outage is the wrong lever and only delays ordinary
    /// failures.
    ///
    /// Read once at daemon start, so a change needs a daemon restart.
    #[serde(default = "default_inference_retry_base_ms")]
    pub inference_retry_base_ms: u64,

    /// Most bytes one tool call may write to disk. Unset is unlimited.
    ///
    /// **Unset in code, set by `lev setup`.** How much an agent should write is
    /// a judgement about what you are doing with it, so nothing is imposed on a
    /// user who never opened this file - but a fresh install gets a concrete
    /// number written here, where it is visible and can be deleted outright.
    ///
    /// The incident behind it (issue #252) was a single shell call appending in
    /// a loop until the 60-second timeout: about 14 GB, from one call that
    /// looked ordinary.
    ///
    /// A shell redirect is measured *after* the call, since the bytes go from
    /// the shell to the file without passing through Leviath. So this stops the
    /// call after the one that overran, not the one that did. `write_file` is
    /// measured before, and is stopped outright.
    ///
    /// Running out of disk is checked separately and is never configurable: see
    /// [`leviath_core::write_limits::MIN_FREE_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_call_write_bytes: Option<u64>,

    /// Most bytes a whole run may write to disk. Unset is unlimited.
    ///
    /// The companion to `max_tool_call_write_bytes`, and the one that catches
    /// what a per-call ceiling cannot: three calls of 12-14 GB each are
    /// individually plausible and collectively a full disk. Same defaulting -
    /// unset in code, written by `lev setup`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_write_bytes: Option<u64>,

    /// How many agents one run may create, sub-agents included. `0`, the
    /// default, is no ceiling.
    ///
    /// ```toml
    /// [limits]
    /// max_agents_per_run = 20
    /// ```
    ///
    /// What a run costs is very nearly its headcount: measured across four
    /// finished research runs, cost per agent stayed between $5.37 and $9.05
    /// while the count ranged from 10 to 42. `max_child_depth` bounds how deep
    /// the tree goes and a fan-out stage's `max_items` bounds one split, but
    /// nothing bounded the total, so the price was decided by how many workers
    /// each generation happened to think were worth spawning.
    ///
    /// A run that reaches the ceiling stops widening and finishes on what it
    /// has. It is not failed: the work already done is worth keeping, and the
    /// merge still runs on the workers that did start.
    #[serde(default)]
    pub max_agents_per_run: usize,

    /// Dollar figures at which a running agent emits a spend event.
    ///
    /// ```toml
    /// [limits]
    /// notify_spend_usd = [5, 25, 100]
    /// ```
    ///
    /// Each is reported once per run, the first time that run's total passes it,
    /// with the stage that was running at the time. Empty by default: nothing is
    /// emitted for an operator who has not asked.
    ///
    /// This is the reporting half only. It does not stop a run - killing one
    /// mid-stage throws away work, which is a different decision from being told
    /// what is happening (#573).
    ///
    /// A run whose models have no published price reports what it could price
    /// and says the figure is not exact, rather than reporting a confident zero.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notify_spend_usd: Vec<f64>,

    /// Per-model overrides of `max_concurrent_inferences`, keyed by model id.
    ///
    /// ```toml
    /// [limits.max_concurrent_inferences_by_model]
    /// "gpt-oss-120b" = 2
    /// ```
    ///
    /// A model listed here uses its own number; every other model uses the
    /// global one. This is the per-model pool the engine has always had - it
    /// simply had no way to be configured.
    ///
    /// A `BTreeMap` so the file this is written back to keeps its order.
    ///
    /// Read once at daemon start, so a change needs a daemon restart.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub max_concurrent_inferences_by_model: BTreeMap<String, usize>,

    /// Caps on concurrent inference requests to one provider, across every
    /// model it serves, keyed by provider name.
    ///
    /// ```toml
    /// [limits.max_concurrent_inferences_by_provider]
    /// cerebras = 1
    /// ```
    ///
    /// A *second*, coarser bound than the per-model pool, not a replacement for
    /// it: a request needs a slot in both. It exists because the per-model cap
    /// cannot express "spend no more than one request at a time at this metered
    /// third-party API", and because lowering the global number to get that
    /// would throttle Anthropic and OpenAI on the same machine too.
    ///
    /// Distinct from `[rate_limits.<provider>]`, which shapes how *fast*
    /// requests are sent. This bounds how many are in flight at once.
    ///
    /// A provider absent from here has no pool of its own. The global fallback
    /// is deliberately not applied per provider: it is a per-model number, and
    /// applying it twice would tighten every install that never asked for it.
    ///
    /// Read once at daemon start, so a change needs a daemon restart.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub max_concurrent_inferences_by_provider: BTreeMap<String, usize>,
}

/// A pool of `0` would take no request ever, so every run needing it parks
/// for the life of the daemon as *ordinary backpressure* - never failed, never
/// reported as an error, because waiting for a busy pool is exactly what the
/// engine is supposed to do. A wedge with no message is the worst answer
/// available, so it is clamped to one and said out loud.
///
/// One rather than unbounded, deliberately: someone writing `0` in a table of
/// ceilings meant "as little as possible", and the documented way to say "no
/// limit" for these keys is to omit them. The schema declares `minimum: 1`;
/// this is what enforces it, since nothing validates config values against the
/// schema at run time.
///
/// Mirrors the tool lane, which clamps its own width the same way
/// (`ToolLane::new`).
fn usable_pool_limit(limit: usize, what: &str) -> usize {
    if limit == 0 {
        tracing::warn!(
            limit = %what,
            "a concurrency limit of 0 would park every request on this pool \
             forever, so it is being treated as 1; omit the key entirely for \
             no limit"
        );
        return 1;
    }
    limit
}

impl LimitsConfig {
    /// The inference pools these limits describe, for the engine: the global
    /// fallback, the per-model overrides, and the per-provider caps.
    ///
    /// A `0` anywhere here is read as `1`: a pool of nothing would park every
    /// request on it for the life of the daemon, since waiting for a full pool
    /// is backpressure the engine never fails. Deleting the key is how a limit
    /// is lifted.
    pub fn inference_pools(&self) -> leviath_runtime::inference_pool::InferencePoolConfig {
        let mut config = leviath_runtime::inference_pool::InferencePoolConfig::new().with_default(
            self.max_concurrent_inferences
                .map(|limit| usable_pool_limit(limit, "[limits] max_concurrent_inferences")),
        );
        for (model, limit) in &self.max_concurrent_inferences_by_model {
            config.set_limit(
                model,
                usable_pool_limit(
                    *limit,
                    &format!("[limits.max_concurrent_inferences_by_model] {model}"),
                ),
            );
        }
        for (provider, limit) in &self.max_concurrent_inferences_by_provider {
            config.set_provider_limit(
                provider,
                usable_pool_limit(
                    *limit,
                    &format!("[limits.max_concurrent_inferences_by_provider] {provider}"),
                ),
            );
        }
        config
    }

    /// The write ceilings in effect, for the engine.
    pub fn write_limits(&self) -> leviath_core::write_limits::WriteLimits {
        leviath_core::write_limits::WriteLimits {
            per_call: self.max_tool_call_write_bytes,
            per_run: self.max_run_write_bytes,
        }
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_concurrent_inferences: default_max_concurrent_inferences(),
            max_concurrent_tools: default_max_concurrent_tools(),
            // Empty: nothing is emitted for an operator who has not asked.
            notify_spend_usd: Vec::new(),
            // No ceiling, which is what every install has today.
            max_agents_per_run: 0,
            default_max_iterations: default_default_max_iterations(),
            exact_token_counting: false,
            script_shell_timeout_secs: default_script_shell_timeout_secs(),
            script_http_timeout_secs: default_script_http_timeout_secs(),
            script_http_max_per_host: default_script_http_max_per_host(),
            stall_timeout_secs: default_stall_timeout_secs(),
            dead_cycles_before_relief: default_dead_cycles_before_relief(),
            finished_retention_secs: default_finished_retention_secs(),
            mcp_idle_disconnect_secs: default_mcp_idle_disconnect_secs(),
            wedge_timeout_secs: default_wedge_timeout_secs(),
            provider_failures_before_open: default_provider_failures_before_open(),
            provider_circuit_cooldown_secs: default_provider_circuit_cooldown_secs(),
            interaction_timeout_secs: default_interaction_timeout_secs(),
            inference_retry_attempts: default_inference_retry_attempts(),
            inference_retry_base_ms: default_inference_retry_base_ms(),
            max_concurrent_inferences_by_model: BTreeMap::new(),
            max_concurrent_inferences_by_provider: BTreeMap::new(),
            // Deliberately `None` here and concrete in `lev setup`: the code
            // imposes no ceiling on a user who never opened the config, and a
            // fresh install gets a number written where it can be seen and
            // deleted. See the field docs.
            max_tool_call_write_bytes: None,
            max_run_write_bytes: None,
        }
    }
}

fn default_webhook_max_retries() -> u32 {
    3
}

fn default_webhook_base_delay_ms() -> u64 {
    500
}

fn default_webhook_max_delay_ms() -> u64 {
    30_000
}

fn default_webhook_timeout_secs() -> u64 {
    10
}

/// Completion-webhook delivery tuning.
///
/// A completion webhook is POSTed when a run reaches a terminal status. Delivery
/// retries on transient failures (network errors, timeouts, 5xx, 429, 408) with
/// exponential backoff. Each field has a safe default so `[webhook]` can be
/// omitted entirely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// Number of retries **after** the first attempt (so total sends is
    /// `max_retries + 1`). Defaults to `3`. Set `0` to disable retries.
    #[serde(default = "default_webhook_max_retries")]
    pub max_retries: u32,

    /// Base backoff before the first retry, in milliseconds. Subsequent retries
    /// double it (capped at `max_delay_ms`). Defaults to `500`.
    #[serde(default = "default_webhook_base_delay_ms")]
    pub base_delay_ms: u64,

    /// Upper bound on any single backoff delay, in milliseconds. Defaults to
    /// `30_000` (30s).
    #[serde(default = "default_webhook_max_delay_ms")]
    pub max_delay_ms: u64,

    /// Per-attempt request timeout, in seconds. Defaults to `10`.
    #[serde(default = "default_webhook_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            max_retries: default_webhook_max_retries(),
            base_delay_ms: default_webhook_base_delay_ms(),
            max_delay_ms: default_webhook_max_delay_ms(),
            timeout_secs: default_webhook_timeout_secs(),
        }
    }
}
