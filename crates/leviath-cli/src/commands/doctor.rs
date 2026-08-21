//! `lev doctor` - prove the provider wiring works, one layer at a time.
//!
//! Five checks run in order and each one is reported, so a failure names the
//! layer that broke instead of leaving the caller to guess:
//!
//! 1. `config` - the config file parses and a provider registry can be built.
//! 2. `search` - the bundled research agents can reach a search engine. The odd
//!    one out: it warns rather than fails, and it asks the *daemon* what it can
//!    see rather than inspecting this process, because the daemon's environment
//!    is fixed at exec time and is not the shell `doctor` was typed in.
//! 3. `resolve` - the user's defaults pick a provider that is actually
//!    registered. This is the check that catches a stage resolving to
//!    `anthropic/claude-sonnet-4-6` (the hard-coded last resort in
//!    [`ModelConfig::provider`](leviath_core::blueprint::ModelConfig::provider))
//!    on a machine with no Anthropic key - the root cause of a fleet of runs
//!    that spawned, sat at iteration 0, and never took a turn.
//! 4. `inference` - one real call to that provider, straight through
//!    [`Provider::infer`]. No world, no run, nothing on disk.
//! 5. `daemon` - a throwaway one-stage agent spawned over the control socket
//!    and waited on, then deleted. This is the only check that exercises the
//!    handoff, which is why it is worth the second billed call: checks 1-4
//!    passing while this one fails is exactly the "credentials are fine, the
//!    daemon is wedged" verdict that used to take a hand-built canary agent to
//!    establish.
//!
//! The daemon-touching I/O lives behind [`crate::dispatch::RiskyExecutors`], so
//! the cores here are driven by unit tests against injected registries and a
//! scripted control socket. The registry builder is a `&dyn Fn` rather than a
//! generic for the coverage reason documented on
//! [`crate::commands::models`]'s equivalent seam.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::bail;
use leviath_core::blueprint::ModelConfig;
use leviath_providers::{InferenceRequest, Message, Provider};
use leviath_runtime::ProviderRegistry;
use leviath_runtime::control_socket::{
    ControlClient, ControlRequest, ControlResponse, DaemonIdentity,
};
use leviath_runtime::pipeline::{bare_default_model, providers_tried, resolve_stage_model};

use crate::commands::run::session::build_provider_registry_from_config;
use crate::config::Config;
use crate::daemon::spawn::model_defaults;

/// `lev doctor --help`. What each check proves, and what a failure at it means.
pub const DOCTOR_LONG_ABOUT: &str = "\
Check that provider wiring works, end to end.

Five checks run in order, and the first failure stops the rest. The check that
fails is the diagnosis:

  config     the config file parses and a provider registry can be built.
             Fails on a malformed config.toml.
  search     the bundled research agents can reach a search engine. Asks the
             daemon what it can see, not this shell, since only the daemon's
             environment decides. Warns (never fails) when BRAVE_API_KEY is
             unset, when the daemon was started before it existed, or when it
             is missing from [security] allow_env_vars - each of which silently
             downgrades every web_search to a keyless Wikipedia lookup.
  resolve    your default provider/model picks a provider that is actually
             registered. Fails when a key is missing or misspelled - and
             catches the case where a blueprint with no model falls back to
             anthropic on a machine that has no Anthropic key.
  inference  one real call to that provider. Fails on a bad key, an unknown
             model id, or a billing problem; the provider's own error is
             printed verbatim, status line and response body included.
  daemon     a one-stage agent spawned over the control socket, waited on,
             then deleted. Fails when the handoff is broken even though the
             credentials are fine.

So config/resolve/inference OK with daemon FAIL means the daemon is the
problem, not your keys - the distinction this command exists to make.

`--model` takes the same forms `lev run --model` does: `provider/model` picks
both (the way to reach a Rhai script provider by name), and a bare model id
pairs with your default_provider. Use it to try a model string before wiring it
into a blueprint.

Two inferences are billed per run, capped at 64 output tokens each.
`--no-daemon` stops after the third check and bills one.

Exits non-zero on failure, so it works as a CI gate. --json prints the same
checks as {\"checks\": [...], \"passed\": bool}.";

/// Arguments for `lev doctor`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct DoctorArgs {
    /// Model to test (`provider/model`, or a bare model id paired with the
    /// configured default provider). Defaults to your configured default.
    #[arg(short, long)]
    pub model: Option<String>,

    /// Stop after the direct inference check: never contact (or start) the
    /// daemon, and create no run.
    #[arg(long)]
    pub no_daemon: bool,

    /// Print the checks as JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

/// How long the daemon check waits for its throwaway run to reach a terminal
/// status before calling the handoff wedged.
const DAEMON_TIMEOUT: Duration = Duration::from_secs(90);

/// How often the daemon check re-asks for the run's status.
const DAEMON_POLL: Duration = Duration::from_millis(250);

/// Output cap for both probe inferences. The prompt is four tokens and the
/// wanted answer is one word, so this only bounds a model that ignores both.
const PROBE_MAX_TOKENS: usize = 64;

/// What the probe asks for, in both the direct call and the daemon run.
const PROBE_PROMPT: &str = "Reply with exactly: PONG";

/// The word a cooperating model comes back with. Reported, never required:
/// a model that answers something else has still proved the wiring, and
/// failing on it would make this command flaky across providers.
const PROBE_EXPECTED: &str = "PONG";

// ─── Report types ─────────────────────────────────────────────────────────────

/// Whether a check proved what it set out to prove.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    /// The layer works.
    Ok,
    /// The layer works, in a degraded way the user should know about.
    ///
    /// Distinct from [`Self::Fail`] because it neither stops the checks after
    /// it nor fails the command: a machine with no search key is a working
    /// install for everyone not running a research agent, and turning that into
    /// a red CI gate would teach people to ignore `doctor`.
    Warn,
    /// The layer is broken; nothing after it ran.
    Fail,
}

impl CheckStatus {
    /// The cell as it appears in the table.
    fn label(&self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }
}

/// One layer's verdict, with whatever detail makes it actionable.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Check {
    /// The layer's short name (`config`, `resolve`, `inference`, `daemon`).
    pub name: &'static str,
    /// Whether it passed.
    pub status: CheckStatus,
    /// What was found: the resolved names, the token usage, or the raw error.
    pub detail: String,
    /// Wall-clock cost, for the checks that make a network call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

impl Check {
    /// A passing check with no timing (the two offline checks).
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Ok,
            detail: detail.into(),
            elapsed_ms: None,
        }
    }

    /// A check that passed in a degraded state worth naming.
    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Warn,
            detail: detail.into(),
            elapsed_ms: None,
        }
    }

    /// A failing check with no timing.
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Fail,
            detail: detail.into(),
            elapsed_ms: None,
        }
    }

    /// Attach how long the call took.
    fn timed(mut self, elapsed: Duration) -> Self {
        self.elapsed_ms = Some(elapsed.as_millis() as u64);
        self
    }
}

// ─── Rendering ────────────────────────────────────────────────────────────────

/// Render the checks as the table `lev doctor` prints.
///
/// Pure: everything that varies between runs (timings, resolved names) is
/// already baked into the `Check`s, so the same input always renders the same
/// output and the tests can assert on it exactly.
pub fn format_report(checks: &[Check]) -> String {
    let name_width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let status_width = checks
        .iter()
        .map(|c| c.status.label().len())
        .max()
        .unwrap_or(0);

    let mut out = String::from("\n");
    for check in checks {
        out.push_str(&format!(
            "  {:<name_width$}  {:<status_width$}  {}",
            check.name,
            check.status.label(),
            check.detail,
        ));
        if let Some(ms) = check.elapsed_ms {
            out.push_str(&format!("  ({:.1}s)", ms as f64 / 1000.0));
        }
        out.push('\n');
    }
    // Keyed on "nothing failed", to agree with the exit code and the `passed`
    // field in --json. A warning is a note about a degraded layer, not a verdict
    // on the run, and a table that withheld "doctor passed" over one would read
    // as a failure the exit code disagreed with.
    if !checks.iter().any(|c| c.status == CheckStatus::Fail) {
        out.push_str("\ndoctor passed\n");
    }
    out
}

// ─── Check 2: search ──────────────────────────────────────────────────────────

/// The environment variable the bundled `web_search` script reads.
const SEARCH_KEY: &str = "BRAVE_API_KEY";

/// Whether the bundled research agents can actually search the web.
///
/// Two independent things have to be true, and getting either wrong is silent:
/// the key has to be in **the daemon's** environment, *and* `[security]
/// allow_env_vars` has to list it. The name ends in `KEY`, so the script host
/// classes it as a credential and refuses to hand it to a Rhai tool that was
/// not explicitly granted it - which means a user who exports the key and
/// nothing else gets the keyless Wikipedia fallback with no error anywhere.
///
/// That combination produced a run whose 47 searches all came back empty and
/// which still wrote a confident, fully cited report. Worth a line in `doctor`
/// precisely because nothing else in the system says it out loud.
///
/// `daemon` is the identity from the control handshake, and the key half of
/// this check: a daemon's environment is fixed at exec time and is not this
/// process's, so asking `std::env` answers for the shell `doctor` was typed in.
/// The two disagree exactly when the bug is present - a key exported now,
/// against a daemon started before it - so the daemon's answer wins whenever
/// there is one, and the fallback says whose environment it is describing.
///
/// The allowlist half needs no such care: it comes from `config.toml`, which
/// both processes read, and which the daemon re-reads per spawn.
///
/// Warns rather than fails: an install with no search key is perfectly good for
/// everyone not running a research agent.
fn search_check(config: &Config, daemon: Option<&DaemonIdentity>) -> Check {
    let allowlisted = leviath_core::script_env_allowed(SEARCH_KEY, &config.security.allow_env_vars);
    let grant_fix = format!(
        "Add `allow_env_vars = [\"{SEARCH_KEY}\"]` under `[security]` in ~/.leviath/config.toml"
    );

    // Whose environment we are describing, and what it holds. A daemon that did
    // not report (older build, or an embedder) leaves `None` and we fall back to
    // this process, saying so.
    let in_env = daemon.and_then(|d| d.sees_tool_env(SEARCH_KEY));
    let (present, whose) = match in_env {
        Some(seen) => (seen, "the daemon"),
        None => (
            std::env::var_os(SEARCH_KEY).is_some_and(|v| !v.is_empty()),
            "this shell (the daemon did not report; it may differ)",
        ),
    };

    match (present, allowlisted) {
        (true, true) => Check::ok(
            "search",
            format!("brave ({SEARCH_KEY} readable by {whose})"),
        ),
        (true, false) => Check::warn(
            "search",
            format!(
                "{SEARCH_KEY} is readable by {whose} but not granted to script tools, so every \
                 web_search falls back to Wikipedia. {grant_fix} - the daemon re-reads it, so no \
                 restart is needed"
            ),
        ),
        // The case a CLI-only check could never see: the key exists here, the
        // grant exists, and the process that will run the tool still cannot
        // read it. Naming the restart is the whole value of asking the daemon.
        (false, _) if in_env == Some(false) && std::env::var_os(SEARCH_KEY).is_some() => {
            Check::warn(
                "search",
                format!(
                    "{SEARCH_KEY} is set in this shell but the daemon cannot see it - it was \
                     started before the key existed, and a process does not inherit an \
                     environment it already has. Run `lev daemon restart` from a shell that \
                     exports {SEARCH_KEY}; until then every web_search falls back to Wikipedia"
                ),
            )
        }
        (false, _) => Check::warn(
            "search",
            format!(
                "no search engine configured: {SEARCH_KEY} is not readable by {whose}, so \
                 research agents fall back to a keyless Wikipedia lookup. Get a key from \
                 https://brave.com/search/api/ and export it in the shell the daemon starts \
                 from. {grant_fix}"
            ),
        ),
    }
}

// ─── Check 1: config ──────────────────────────────────────────────────────────

/// `[rate_limits.<name>]` entries naming no provider that exists.
///
/// The unknown-key check cannot see these: the table is a map with arbitrary
/// keys, so `[rate_limits.anthropc]` deserializes perfectly and simply throttles
/// nothing. The set of names that mean anything here is closed - script
/// providers set theirs under `[model_providers.<name>] rate_limit` instead -
/// and the wizard's catalog is already the one list of it, so this needs no
/// second list to fall out of date.
fn misdirected_rate_limits(config: &Config) -> Vec<String> {
    let known: Vec<&str> = crate::commands::setup::catalog::providers()
        .iter()
        .map(|p| p.id)
        .collect();
    let mut misdirected: Vec<String> = config
        .rate_limits
        .keys()
        .filter(|name| !known.contains(&name.as_str()))
        .map(|name| format!("rate_limits.{name}"))
        .collect();
    // A map iterates in arbitrary order; the report should not.
    misdirected.sort_unstable();
    misdirected
}

/// Report what the config named and what the registry ended up holding.
///
/// Only native providers can be listed - a Rhai script provider is resolved by
/// name on demand and never enumerated - so the line says so rather than
/// implying the user's `.rhai` providers are missing.
fn config_check(config: &Config, registry: &ProviderRegistry) -> Check {
    let mut names = registry.provider_names();
    names.sort_unstable();
    let registered = match names.is_empty() {
        true => "none".to_string(),
        false => names.join(", "),
    };
    let detail = format!(
        "default_provider={}; registered: {} (script providers resolve by name)",
        config.default_provider, registered
    );

    // "Which keys were ignored" was previously only answerable by catching the
    // start-up warning as it scrolled past, and only if you were looking. A
    // note on an OK line rather than a failure, matching how the `resolve`
    // check reports a config that works but probably is not what was meant:
    // the rest of the file still applies, so this is not broken wiring.
    let mut unread = Config::unread_keys_at(&Config::config_path());
    unread.extend(misdirected_rate_limits(config));
    if unread.is_empty() {
        return Check::ok("config", detail);
    }
    let subject = match unread.len() {
        1 => "1 key in config.toml is",
        n => &format!("{n} keys in config.toml are"),
    };
    Check::ok(
        "config",
        format!(
            "{detail}  (note: {subject} read by nothing - check the spelling: {})",
            unread.join(", ")
        ),
    )
}

// ─── Check 2: resolve ─────────────────────────────────────────────────────────

/// What check 2 settled on, when it settled on something usable. The handle is
/// carried rather than looked up again so the later checks cannot disagree with
/// the one that reported.
struct Resolved {
    provider_name: String,
    model: String,
    provider: Arc<dyn Provider>,
}

/// Run the real stage-model fallback chain against an **empty** [`ModelConfig`],
/// so what comes back is what the user's config alone would pick for a stage
/// that states no preference of its own.
///
/// The guard afterwards is the one [`leviath_runtime::pipeline::resolve_stages`]
/// applies at spawn: the last resort in the chain is unchecked and hands back
/// `anthropic`/`claude-sonnet-4-6` whether or not anything answers to that name.
/// Resolving through [`ProviderRegistry::get`] rather than `has` makes the same
/// decision (native first, then the script layer) while keeping the handle, so
/// the provider cannot go missing between deciding to use it and using it.
fn resolve_check(
    config: &Config,
    model_override: Option<&str>,
    registry: &ProviderRegistry,
) -> (Check, Option<Resolved>) {
    let empty = ModelConfig {
        models: Vec::new(),
        allow_user_default: true,
        parameters: std::collections::HashMap::new(),
        request_timeout_secs: None,
    };
    let defaults = model_defaults(config);
    let (provider_name, model) = resolve_stage_model(&empty, model_override, &defaults, registry);

    match registry.get(&provider_name) {
        Some(provider) => (
            Check::ok(
                "resolve",
                format!(
                    "{provider_name} / {model}{}{}",
                    default_provider_note(config, &provider_name, model_override, registry),
                    qualified_default_model_note(config, model_override),
                ),
            ),
            Some(Resolved {
                provider_name,
                model,
                provider,
            }),
        ),
        None => (
            Check::fail(
                "resolve",
                format!(
                    "resolved to '{provider_name}', which is not configured (tried: {}). \
                     Configure it with `lev setup`, or add it to config.toml.",
                    providers_tried(&empty, model_override, &defaults)
                ),
            ),
            None,
        ),
    }
}

/// The note appended when the resolved provider is not the one the user named
/// as their default.
///
/// This check resolves an empty `ModelConfig`, so `default_provider` really
/// does lose here without a `default_model`: there is no blueprint entry to
/// promote and no model to send. A real run is the opposite case, and the note
/// used to describe only the first one - it said the default provider "is never
/// chosen", which someone reasonably read as a statement about their runs.
///
/// It is not. `resolve_stage_candidates` moves every registered candidate on
/// the default provider to the front of the blueprint's list, so
/// `default_provider = "openrouter"` sends every stage of every bundled
/// blueprint to that blueprint's OpenRouter entry. A run that had been quietly
/// executing on a fallback model for weeks looked, from here, like a config
/// line that did nothing at all.
///
/// Not a failure: the resolution is legitimate and the run will work. It is
/// only worth saying because it is not what the config appears to ask for.
/// Silent while `--model` is in play, which is the caller overriding on purpose.
fn default_provider_note(
    config: &Config,
    resolved: &str,
    model_override: Option<&str>,
    registry: &ProviderRegistry,
) -> String {
    if model_override.is_some() || resolved == config.default_provider {
        return String::new();
    }
    // The missing model is the only reason a registered default provider loses
    // from here: this check resolves an empty `ModelConfig`, so one with a
    // model set has no competition to lose to. An *unregistered* default
    // provider is a different complaint, and one the `config` line already
    // makes by listing what is registered.
    if config.default_model.is_some() || !registry.has(&config.default_provider) {
        return String::new();
    }
    let named = &config.default_provider;
    format!(
        "  (note: this check resolves no blueprint, so with no `default_model` \
         set there is nothing to send to '{named}' and it loses here. A real run \
         is different: a blueprint that lists '{named}' has that entry moved to \
         the front, so your runs use '{named}' with whatever model the blueprint \
         names for each stage. Set `default_model` only to pin one model across \
         every stage, which overrides the per-stage choices a blueprint makes.)"
    )
}

/// The note appended when `default_model` is written as `provider/model`.
///
/// `default_model` is a bare model id that pairs with `default_provider`, but
/// `--model` and `fallback_order` take the qualified form and an OpenRouter id
/// already contains a slash, so `default_model = "ollama/qwen3.8:latest"` is
/// an easy thing to write. The resolver drops the redundant prefix, so the run
/// works; this says what it was read as, so the config can be tidied and so
/// the line above is not a mystery. Silent under `--model`, when the default
/// is not in play at all.
fn qualified_default_model_note(config: &Config, model_override: Option<&str>) -> String {
    if model_override.is_some() {
        return String::new();
    }
    let Some(written) = config.default_model.as_deref() else {
        return String::new();
    };
    let bare = bare_default_model(&config.default_provider, written);
    if bare == written {
        return String::new();
    }
    let provider = &config.default_provider;
    format!(
        "  (note: default_model is written as '{written}', but it takes a bare model id \
         and pairs with default_provider - it is read as '{bare}'; drop the '{provider}/' \
         in config.toml)"
    )
}

// ─── Check 3: inference ───────────────────────────────────────────────────────

/// One real call to the resolved provider. No context window, no blueprint, no
/// world: the point is to isolate "can this credential reach this model" from
/// everything the framework layers on top of it.
async fn inference_check(provider: &dyn Provider, model: &str) -> Check {
    let caps = provider.capabilities(model);
    let request = InferenceRequest {
        system: Vec::new(),
        messages: vec![Message {
            role: "user".to_string(),
            content: PROBE_PROMPT.into(),
            cache_breakpoint: false,
        }],
        model: model.to_string(),
        max_tokens: PROBE_MAX_TOKENS.min(caps.max_output_tokens),
        // Deterministic where it is allowed; providers that reject the
        // parameter outright get the same value and ignore it.
        temperature: 0.0,
        tools: Vec::new(),
        extra: serde_json::Value::Null,
        request_timeout_secs: Some(60),
    };

    let started = Instant::now();
    match provider.infer(&request).await {
        Ok(response) => {
            let usage = response.tokens_used;
            let echo = match response.content.contains(PROBE_EXPECTED) {
                true => format!("replied {PROBE_EXPECTED}"),
                // Not a failure. The call succeeded, which is the thing being
                // checked; what the model chose to say is a note.
                false => format!("no {PROBE_EXPECTED} in the reply"),
            };
            Check::ok(
                "inference",
                format!(
                    "{} in / {} out / {} total, {echo}",
                    usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
                ),
            )
            .timed(started.elapsed())
        }
        // Verbatim. Every provider formats a non-2xx as `HTTP <status>: <body>`,
        // and that body is the whole diagnosis for the cases this command is
        // for - a 402 naming the exhausted credit, a 404 naming the model.
        // Summarising it here would throw away the answer.
        Err(e) => Check::fail("inference", e.to_string()).timed(started.elapsed()),
    }
}

// ─── Check 4: daemon ──────────────────────────────────────────────────────────

/// The throwaway blueprint the daemon check spawns: one autonomous stage, one
/// iteration, no tools, no transitions.
///
/// A single stage with no transitions is a valid "pure linear" blueprint and
/// goes `Complete` after one text-only turn. Advertising no tools also keeps it
/// out of the empty-output report - an agent with no way to modify a file is
/// never judged for not having modified one.
///
/// `provider` and `model` are serialised as JSON strings, whose escaping is a
/// subset of TOML's basic-string escaping, so a name containing a quote or a
/// backslash cannot break out of the literal.
fn canary_manifest(provider: &str, model: &str) -> String {
    let provider = serde_json::to_string(provider).expect("a str always serializes to JSON");
    let model = serde_json::to_string(model).expect("a str always serializes to JSON");
    format!(
        r#"[agent]
name = "doctor"
version = "0.0.1"
description = "One-turn provider probe spawned by `lev doctor`, deleted when it finishes."
entry_stage = "ping"

[stages.ping]
mode = "autonomous"
model = {{ models = [{{ provider = {provider}, model = {model} }}] }}
description = "Answer once, in text."
available_tools = []
max_iterations = 1
system_prompt = "Reply with exactly: {PROBE_EXPECTED}. Call no tools."

[context.regions]
task = {{ kind = "pinned", max_tokens = 1000, seed = "task" }}
conversation = {{ kind = "sliding_window", max_items = 4, max_tokens = 2000 }}
"#
    )
}

/// Delete everything the canary run left behind.
///
/// Ordered the way the dashboard's delete is: record the run terminal on disk
/// *before* removing it, so that if an in-flight persist job loses the race and
/// recreates the directory, it reappears as a finished run rather than a live
/// one nobody will ever collect.
fn cleanup_run(run_id: &str) {
    let _ = crate::runstate::force_cancel(run_id);
    let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    let _ = leviath_core::paths::data_dir().map(|d| {
        let _ = std::fs::remove_dir_all(d.join("state").join(run_id));
    });
}

/// The daemon check's own outcome, before it is turned into a [`Check`], so the
/// spawn/wait/cleanup steps each have one thing to say.
enum DaemonOutcome {
    /// The run finished successfully; carries the summary line.
    Complete(String),
    /// The run reached the daemon but did not end well.
    Failed(String),
}

/// Write the canary manifest under `root`, returning its path.
///
/// The manifest's parent directory names the agent, and so prefixes the run id:
/// the canary is identifiable as `doctor-...` for as long as it exists.
fn stage_canary(
    root: &std::path::Path,
    provider: &str,
    model: &str,
) -> std::io::Result<std::path::PathBuf> {
    let agent_dir = root.join("doctor");
    std::fs::create_dir_all(&agent_dir)?;
    let manifest = agent_dir.join("agent.leviath");
    std::fs::write(&manifest, canary_manifest(provider, model))?;
    Ok(manifest)
}

/// Spawn the canary, wait for it, and report. The run is deleted on every path
/// out of here, including the failing ones.
async fn daemon_check(
    client: &ControlClient,
    provider_name: &str,
    model: &str,
    timeout: Duration,
    poll: Duration,
    root: &std::path::Path,
) -> Check {
    let started = Instant::now();
    let manifest = match stage_canary(root, provider_name, model) {
        Ok(manifest) => manifest,
        Err(e) => return Check::fail("daemon", format!("could not stage a probe agent: {e}")),
    };

    match spawn_and_wait(client, &manifest, root, timeout, poll).await {
        DaemonOutcome::Complete(detail) => Check::ok("daemon", detail).timed(started.elapsed()),
        DaemonOutcome::Failed(detail) => Check::fail("daemon", detail).timed(started.elapsed()),
    }
}

/// Ask the daemon to run the staged canary and wait for a terminal status.
async fn spawn_and_wait(
    client: &ControlClient,
    manifest: &std::path::Path,
    workdir: &std::path::Path,
    timeout: Duration,
    poll: Duration,
) -> DaemonOutcome {
    // `--yolo`: a probe that stops to ask a person for a tool approval has
    // stopped being a probe. It advertises no tools, so this waives nothing
    // that could actually run.
    let args = crate::daemon::client::resolve_spawn_args(crate::daemon::client::LaunchRequest {
        path: &manifest.to_string_lossy(),
        task: Some(PROBE_PROMPT),
        // The probe brings its own task, so the editor fallback is unreachable.
        // Answering "not a terminal" anyway makes that structural rather than
        // incidental: a doctor that opened an editor would be a bad joke.
        stdin_is_terminal: &|| false,
        model: None,
        workdir: &workdir.to_string_lossy(),
        yolo: true,
        allow: Vec::new(),
        max_depth: None,
        regions: std::collections::HashMap::new(),
        no_seed_commands: false,
        output_request: None,
    });
    let args = match args {
        Ok(args) => args,
        Err(e) => return DaemonOutcome::Failed(format!("could not build the spawn request: {e}")),
    };
    let run_id = args.run_id.clone();

    let spawned = match client.spawn(args).await {
        Ok(ControlResponse::Spawned { run_id }) => Ok(run_id),
        Ok(ControlResponse::Error { message }) => {
            Err(format!("the daemon refused the spawn: {message}"))
        }
        Ok(other) => Err(format!("unexpected daemon response to spawn: {other:?}")),
        Err(e) => Err(format!(
            "the daemon is not reachable ({e}); start it with `lev daemon`"
        )),
    };
    let run_id = match spawned {
        Ok(id) => id,
        Err(detail) => {
            // The daemon may have staked out the run directory before failing.
            cleanup_run(&run_id);
            return DaemonOutcome::Failed(detail);
        }
    };

    let outcome = wait_for_run(client, &run_id, timeout, poll).await;
    cleanup_run(&run_id);
    outcome
}

/// Poll the run until it reaches a terminal status, or the deadline passes.
async fn wait_for_run(
    client: &ControlClient,
    run_id: &str,
    timeout: Duration,
    poll: Duration,
) -> DaemonOutcome {
    let started = Instant::now();
    loop {
        let still = match client.status(run_id).await {
            Ok(ControlResponse::Status {
                status: Some(status),
            }) => {
                if leviath_runtime::pipeline::is_terminal_status(&status) {
                    return finished(run_id, &status);
                }
                status.label()
            }
            // The daemon reaps a finished run, so a run that was live a moment
            // ago and is now unknown has ended - and its meta.json says how.
            Ok(ControlResponse::Status { status: None }) => return reaped(run_id),
            Ok(other) => {
                return DaemonOutcome::Failed(format!("unexpected daemon response: {other:?}"));
            }
            Err(e) => {
                return DaemonOutcome::Failed(format!("lost contact with the daemon: {e}"));
            }
        };
        if started.elapsed() >= timeout {
            return DaemonOutcome::Failed(format!(
                "the run was still '{still}' after {}s - the daemon took the spawn but is not \
                 getting anywhere. Check `lev ps` for the lane footer.",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(poll).await;
    }
}

/// Turn a terminal [`AgentStatus`](leviath_runtime::components::AgentStatus)
/// into the daemon check's verdict, reading the iteration count off disk.
fn finished(run_id: &str, status: &leviath_runtime::components::AgentStatus) -> DaemonOutcome {
    use leviath_runtime::components::AgentStatus;
    let iterations = crate::runstate::read_meta(run_id)
        .map(|m| m.iteration)
        .unwrap_or(0);
    match status {
        AgentStatus::Complete => DaemonOutcome::Complete(format!(
            "run {run_id} complete after {iterations} iteration(s)"
        )),
        AgentStatus::Error { message } => {
            DaemonOutcome::Failed(format!("run {run_id} ended in error: {message}"))
        }
        other => DaemonOutcome::Failed(format!("run {run_id} ended {}", other.label())),
    }
}

/// The run is no longer known to the daemon: fall back to what it wrote.
fn reaped(run_id: &str) -> DaemonOutcome {
    match crate::runstate::read_meta(run_id) {
        Ok(meta) if crate::runstate::is_terminal_status(&meta.status) => match meta.error {
            Some(err) => DaemonOutcome::Failed(format!("run {run_id} ended in error: {err}")),
            None => DaemonOutcome::Complete(format!(
                "run {run_id} {} after {} iteration(s)",
                meta.status, meta.iteration
            )),
        },
        // Never seen, or seen and still unfinished: either way the daemon took
        // the spawn and then lost the run, which is a handoff failure.
        _ => DaemonOutcome::Failed(format!(
            "run {run_id} vanished before it finished; the daemon accepted the spawn but \
             never completed it"
        )),
    }
}

// ─── Orchestration ────────────────────────────────────────────────────────────

/// What the fourth check has to work with.
///
/// `Unavailable` exists so a daemon that will not start is still *reported* as
/// a daemon failure rather than aborting the command: the caller auto-starts
/// one before the checks begin, and if that abort propagated, `lev doctor`
/// would refuse to tell you whether your credentials were fine - which is most
/// of what it is for.
pub enum DaemonTarget<'a> {
    /// Do not run the fourth check at all (`--no-daemon`).
    Skip,
    /// Run it against this daemon.
    Client(&'a ControlClient),
    /// There is no daemon to hand off to, and this is why.
    Unavailable(String),
}

/// Run the checks in order, stopping at the first failure.
///
/// `+ Sync` on the builder so the returned future is `Send`: `lev serve`'s
/// `GET /api/doctor` awaits these same checks inside an axum handler, which
/// requires it. Every caller passes a plain `fn` item, which always is.
pub async fn run_checks(
    args: &DoctorArgs,
    build_registry: &(
         dyn Fn(&Config) -> Result<ProviderRegistry, leviath_providers::ProviderError> + Sync
     ),
    daemon: DaemonTarget<'_>,
) -> Vec<Check> {
    let mut checks = Vec::new();

    // A config that will not parse is itself a finding, and the most common
    // one there is - reporting it as `config FAIL` beats the bare load error.
    let config = match Config::load() {
        Ok(config) => config,
        Err(e) => {
            checks.push(Check::fail("config", e.to_string()));
            return checks;
        }
    };
    for warning in config.validate_keys() {
        eprintln!("Warning: {warning}");
    }

    // A registry that will not build is the most basic thing `doctor` can
    // report, so it becomes a failed check rather than stopping the run.
    let registry = match build_registry(&config) {
        Ok(registry) => registry,
        Err(e) => {
            checks.push(Check::fail(
                "providers",
                format!("could not build any provider client: {e}"),
            ));
            return checks;
        }
    };
    checks.push(config_check(&config, &registry));
    // Ask the daemon who it is before judging its environment. `List` is
    // idempotent and local, and it is only sent to force the handshake that
    // fills `link().daemon` - the reply itself is not the point, and a daemon
    // that will not answer just leaves the identity unknown, which the check
    // reports as such rather than guessing.
    let identity = match &daemon {
        DaemonTarget::Client(client) => {
            let _ = client.request(&ControlRequest::List).await;
            client.link().daemon
        }
        DaemonTarget::Skip | DaemonTarget::Unavailable(_) => None,
    };
    // Runs early enough to be seen even when a later network check fails, and
    // only ever warns, so it never cuts the run short.
    checks.push(search_check(&config, identity.as_ref()));

    let (check, resolved) = resolve_check(&config, args.model.as_deref(), &registry);
    checks.push(check);
    let Some(resolved) = resolved else {
        return checks;
    };

    let check = inference_check(resolved.provider.as_ref(), &resolved.model).await;
    let inference_failed = check.status == CheckStatus::Fail;
    checks.push(check);
    if inference_failed {
        return checks;
    }

    match daemon {
        DaemonTarget::Skip => {}
        DaemonTarget::Unavailable(reason) => checks.push(Check::fail("daemon", reason)),
        DaemonTarget::Client(client) => {
            // `.expect`: a temp directory that cannot be created means the
            // machine has no writable scratch space at all, which every other
            // part of a run would hit first. Nothing here could report it more
            // usefully.
            let stage = tempfile::tempdir().expect("the system temp directory is writable");
            checks.push(
                daemon_check(
                    client,
                    &resolved.provider_name,
                    &resolved.model,
                    DAEMON_TIMEOUT,
                    DAEMON_POLL,
                    stage.path(),
                )
                .await,
            );
        }
    }
    checks
}

/// Print the checks and report the first failure as the command's error, so the
/// process exits non-zero and `lev doctor` works as a CI gate.
async fn execute_with_registry(
    args: DoctorArgs,
    build_registry: &(
         dyn Fn(&Config) -> Result<ProviderRegistry, leviath_providers::ProviderError> + Sync
     ),
    daemon: DaemonTarget<'_>,
) -> anyhow::Result<()> {
    let checks = run_checks(&args, build_registry, daemon).await;
    let failed = checks.iter().find(|c| c.status == CheckStatus::Fail);

    if args.json {
        let report = serde_json::json!({
            "checks": checks,
            "passed": failed.is_none(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("a Check report always serializes")
        );
    } else {
        print!("{}", format_report(&checks));
    }

    match failed {
        Some(check) => bail!("doctor failed at: {}", check.name),
        None => Ok(()),
    }
}

/// `lev doctor`. The binary decides what the fourth check gets to talk to; see
/// [`DaemonTarget`].
pub async fn execute(args: DoctorArgs, daemon: DaemonTarget<'_>) -> anyhow::Result<()> {
    execute_with_registry(args, &build_provider_registry_from_config, daemon).await
}

#[cfg(test)]
mod tests;
