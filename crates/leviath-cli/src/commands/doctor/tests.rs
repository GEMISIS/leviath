//! Tests for `lev doctor`.
//!
//! Every test that reaches `Config::load()`, the runs directory, or the data
//! root goes through [`with_env`], which redirects all three at once. No test
//! can make a billed call: the provider registry is always injected, and the
//! isolation clears every provider key from the environment anyway.

use super::*;

use leviath_providers::{
    FinishReason, InferenceResponse, ModelCapabilities, ProviderError, TokenUsage,
};
use leviath_runtime::components::AgentStatus;
use leviath_runtime::control_socket::{ControlId, bind_control_listener, control_id};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;

// ─── Harness ──────────────────────────────────────────────────────────────────

/// Run `f` with the config path, data root, and runs directory all redirected
/// into one fresh temp root, and every provider key cleared.
///
/// One `temp_env` call, not two: it serializes process-wide and holds its lock
/// across the future, so nesting `with_isolated_config_path_async` inside a
/// second call would deadlock.
async fn with_env<R, Fut>(f: impl FnOnce(PathBuf) -> Fut) -> R
where
    Fut: std::future::Future<Output = R>,
{
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir.path().to_path_buf();
    let mut vars = crate::config::config_isolation_vars(&root);
    vars.push(("LEVIATH_HOME", Some(root.clone().into_os_string())));
    vars.push(("LEVIATH_RUNS_DIR", Some(root.join("runs").into_os_string())));
    // The search check reads this, so a developer who has one exported would
    // otherwise see a different check list than CI does.
    vars.push(("BRAVE_API_KEY", None));
    temp_env::async_with_vars(vars, f(root)).await
}

/// A provider whose single inference is decided up front.
struct StubProvider {
    reply: Result<String, ProviderError>,
}

impl StubProvider {
    fn replying(content: &str) -> Arc<dyn Provider> {
        Arc::new(Self {
            reply: Ok(content.to_string()),
        })
    }

    fn failing(message: &str) -> Arc<dyn Provider> {
        Arc::new(Self {
            reply: Err(ProviderError::ApiError(message.to_string())),
        })
    }
}

#[async_trait::async_trait]
impl Provider for StubProvider {
    async fn infer(
        &self,
        _request: &InferenceRequest,
    ) -> leviath_providers::Result<InferenceResponse> {
        match &self.reply {
            Ok(content) => Ok(InferenceResponse {
                content: content.clone(),
                tool_calls: Vec::new(),
                tokens_used: TokenUsage {
                    prompt_tokens: 12,
                    completion_tokens: 4,
                    total_tokens: 16,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                    reported_cost_usd: None,
                },
                finish_reason: FinishReason::Complete,
            }),
            Err(e) => Err(ProviderError::ApiError(e.to_string())),
        }
    }

    async fn count_tokens(&self, text: &str, _model: &str) -> usize {
        text.len()
    }

    fn max_context_tokens(&self, _model: &str) -> usize {
        8192
    }

    fn name(&self) -> &str {
        "stub"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A registry holding one stub under `name`.
fn registry_with(name: &str, provider: Arc<dyn Provider>) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    registry.register(name.to_string(), provider);
    registry
}

/// Bind a control listener under `dir` and serve `responses` one per incoming
/// connection - the client opens a fresh one per request. Once they run out the
/// listener drops, so a further request fails to connect, which is how the
/// "lost contact" path is driven.
fn scripted_daemon(dir: &Path, responses: Vec<String>) -> (ControlId, JoinHandle<()>) {
    let id = control_id(dir);
    let mut listener = bind_control_listener(&id).expect("bind a fresh control socket");
    let handle = tokio::spawn(async move {
        for line in responses {
            let Ok(Some(stream)) = listener.accept().await else {
                return;
            };
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _request = lines.next_line().await;
            let _ = write_half.write_all(line.as_bytes()).await;
            let _ = write_half.write_all(b"\n").await;
        }
    });
    (id, handle)
}

/// The reply to the `List` the search check sends to force a handshake. Its
/// contents are discarded - only the identity the handshake carries is wanted -
/// but the daemon still has to answer it, so a scripted daemon must queue one.
const LISTING: &str = r#"{"result":"list","runs":[],"finished":[]}"#;
const SPAWNED: &str = r#"{"result":"spawned","run_id":"doctor-1-abc"}"#;
const COMPLETE: &str = r#"{"result":"status","status":"Complete"}"#;
const ACTIVE: &str = r#"{"result":"status","status":"Active"}"#;

/// Stage a canary manifest under `root` and hand back its path, for the tests
/// that drive `spawn_and_wait` directly.
fn staged(root: &Path) -> PathBuf {
    stage_canary(root, "stub", "m").expect("staging into a fresh temp dir succeeds")
}

/// Write a `meta.json` for `run_id` so the on-disk fallbacks have something to
/// read - one iteration in, and whatever terminal state the test is after.
fn write_meta(run_id: &str, status: leviath_core::run_meta::RunStatus, error: Option<&str>) {
    let dir = crate::runstate::run_dir(run_id);
    std::fs::create_dir_all(&dir).expect("create the run dir");
    let mut meta = leviath_core::run_meta::RunMeta::new(
        run_id.to_string(),
        "doctor".to_string(),
        String::new(),
        PROBE_PROMPT.to_string(),
        None,
        String::new(),
        1,
    );
    meta.status = status;
    meta.error = error.map(str::to_string);
    meta.iteration = 1;
    std::fs::write(
        dir.join("meta.json"),
        serde_json::to_string(&meta).expect("RunMeta serializes"),
    )
    .expect("write meta.json");
}

// ─── format_report ────────────────────────────────────────────────────────────

#[test]
fn format_report_of_nothing_is_a_pass() {
    // Degenerate, but the width computation has to survive it.
    assert!(format_report(&[]).contains("doctor passed"));
}

#[test]
fn format_report_renders_timings_and_the_pass_line() {
    let checks = vec![
        Check::ok("config", "default_provider=stub"),
        Check::ok("inference", "12 in / 4 out / 16 total, replied PONG")
            .timed(Duration::from_millis(1234)),
    ];
    let out = format_report(&checks);
    assert!(out.contains("config     OK"), "columns pad: {out}");
    assert!(out.contains("(1.2s)"), "timing rendered: {out}");
    assert!(out.ends_with("doctor passed\n"), "{out}");
}

#[test]
fn format_report_of_a_failure_has_no_pass_line() {
    let checks = vec![
        Check::ok("config", "fine"),
        Check::fail("resolve", "resolved to 'nope'"),
    ];
    let out = format_report(&checks);
    assert!(out.contains("FAIL"), "{out}");
    assert!(!out.contains("doctor passed"), "{out}");
}

#[test]
fn format_report_of_a_warning_still_passes() {
    // A degraded layer is a note, not a verdict: the exit code and the `passed`
    // field both say this run is fine, and the table has to agree with them.
    let checks = vec![
        Check::ok("config", "fine"),
        Check::warn("search", "no search engine configured"),
    ];
    let out = format_report(&checks);
    assert!(out.contains("search  WARN"), "columns pad: {out}");
    assert!(out.ends_with("doctor passed\n"), "{out}");
}

// ─── search_check ─────────────────────────────────────────────────────────────

/// A daemon that reports seeing exactly `names`.
fn daemon_seeing(names: &[&str]) -> DaemonIdentity {
    DaemonIdentity::this_process("test")
        .with_tool_env(names.iter().map(|s| (*s).to_string()).collect())
}

/// Run `search_check` against `daemon`, with `BRAVE_API_KEY` set to `key` in
/// *this* process and the allowlist set to `allowed`.
fn search_with_daemon(
    key: Option<&str>,
    allowed: &[&str],
    daemon: Option<&DaemonIdentity>,
) -> Check {
    let mut config = Config::default();
    config.security.allow_env_vars = allowed.iter().map(|s| (*s).to_string()).collect();
    temp_env::with_var("BRAVE_API_KEY", key, || search_check(&config, daemon))
}

/// The common case: no daemon reported, so the check falls back to this
/// process's own environment.
fn search_with(key: Option<&str>, allowed: &[&str]) -> Check {
    search_with_daemon(key, allowed, None)
}

#[test]
fn search_check_passes_when_the_key_is_set_and_allowlisted() {
    let check = search_with(Some("sk-brave"), &["BRAVE_API_KEY"]);
    assert_eq!(check.status, CheckStatus::Ok);
    assert!(check.detail.contains("brave"), "{}", check.detail);
}

#[test]
fn search_check_warns_when_the_key_is_set_but_not_allowlisted() {
    // The trap this check exists for: the name ends in KEY, so the script host
    // refuses it and every search silently becomes a Wikipedia lookup.
    let check = search_with(Some("sk-brave"), &[]);
    assert_eq!(check.status, CheckStatus::Warn);
    assert!(
        check.detail.contains("allow_env_vars"),
        "names the fix: {}",
        check.detail
    );
    // The grant is config, which the daemon re-reads per spawn - telling the
    // user to restart here would be busywork.
    assert!(
        check.detail.contains("no restart is needed"),
        "{}",
        check.detail
    );
}

#[test]
fn search_check_warns_when_there_is_no_key_at_all() {
    let check = search_with(None, &["BRAVE_API_KEY"]);
    assert_eq!(check.status, CheckStatus::Warn);
    assert!(
        check.detail.contains("Wikipedia"),
        "says what happens instead: {}",
        check.detail
    );
}

#[test]
fn search_check_treats_an_empty_key_as_no_key() {
    // An exported-but-empty variable reads as configured and is not.
    let check = search_with(Some(""), &["BRAVE_API_KEY"]);
    assert_eq!(check.status, CheckStatus::Warn);
    assert!(check.detail.contains("not readable"), "{}", check.detail);
}

#[test]
fn search_check_without_a_daemon_says_whose_environment_it_read() {
    // Answering for the wrong process silently is the bug this check exists to
    // catch, so when it cannot reach the right one it must say so.
    let check = search_with(Some("sk-brave"), &["BRAVE_API_KEY"]);
    assert!(
        check.detail.contains("the daemon did not report"),
        "{}",
        check.detail
    );
}

#[test]
fn search_check_believes_the_daemon_over_this_process() {
    // The daemon has the key and this shell does not. Nothing is wrong: the
    // process that runs the tool is the one that matters.
    let check = search_with_daemon(
        None,
        &["BRAVE_API_KEY"],
        Some(&daemon_seeing(&["BRAVE_API_KEY"])),
    );
    assert_eq!(check.status, CheckStatus::Ok);
    assert!(check.detail.contains("the daemon"), "{}", check.detail);
}

#[test]
fn search_check_catches_a_daemon_started_before_the_key_existed() {
    // The false negative a CLI-only check could never see: key here, grant in
    // place, and the daemon still blind to it. This is the whole reason the
    // daemon is asked.
    let check = search_with_daemon(
        Some("sk-brave"),
        &["BRAVE_API_KEY"],
        Some(&daemon_seeing(&[])),
    );
    assert_eq!(check.status, CheckStatus::Warn);
    assert!(
        check.detail.contains("the daemon cannot see it"),
        "names the real problem: {}",
        check.detail
    );
    assert!(
        check.detail.contains("lev daemon restart"),
        "names the remedy: {}",
        check.detail
    );
}

#[test]
fn search_check_reports_a_key_nobody_has_as_unconfigured() {
    // Neither side has it: that is not a restart problem, it is a setup one,
    // and pointing at `lev daemon restart` would send the user in circles.
    let check = search_with_daemon(None, &["BRAVE_API_KEY"], Some(&daemon_seeing(&[])));
    assert_eq!(check.status, CheckStatus::Warn);
    assert!(
        check.detail.contains("brave.com/search/api"),
        "{}",
        check.detail
    );
    assert!(
        !check.detail.contains("lev daemon restart"),
        "no restart advice when there is no key to load: {}",
        check.detail
    );
}

// ─── config_check ─────────────────────────────────────────────────────────────

#[test]
fn config_check_lists_registered_providers_sorted() {
    let config = Config::default();
    let mut registry = registry_with("openrouter", StubProvider::replying("hi"));
    registry.register("anthropic".to_string(), StubProvider::replying("hi"));
    let check = config_check(&config, &registry);
    assert_eq!(check.status, CheckStatus::Ok);
    assert!(
        check.detail.contains("registered: anthropic, openrouter"),
        "got: {}",
        check.detail
    );
}

#[test]
fn config_check_says_none_when_nothing_is_registered() {
    let check = config_check(&Config::default(), &ProviderRegistry::new());
    assert!(
        check.detail.contains("registered: none"),
        "{}",
        check.detail
    );
}

/// A `[rate_limits]` entry naming no provider throttles nothing, and the
/// unknown-key check cannot see it: the table takes arbitrary keys, so the
/// typo deserializes perfectly.
#[test]
fn config_check_names_a_rate_limit_for_a_provider_that_does_not_exist() {
    let mut config = Config::default();
    let limit = leviath_providers::RateLimitConfig {
        requests_per_minute: 10,
        tokens_per_minute: 1000,
    };
    config
        .rate_limits
        .insert("anthropc".to_string(), limit.clone());
    // The control: the correctly spelled one beside it must stay quiet, or the
    // note would just be reporting every rate limit anyone set.
    config.rate_limits.insert("anthropic".to_string(), limit);

    let check = config_check(&config, &ProviderRegistry::new());
    assert_eq!(check.status, CheckStatus::Ok, "not broken wiring");
    assert!(
        check.detail.contains("rate_limits.anthropc"),
        "got: {}",
        check.detail
    );
    assert!(
        !check.detail.contains("rate_limits.anthropic,")
            && !check.detail.ends_with("rate_limits.anthropic"),
        "the real provider must not be reported: {}",
        check.detail
    );
}

/// Two of them read as two, not as "1 key".
#[test]
fn config_check_counts_more_than_one_unread_key() {
    let mut config = Config::default();
    for name in ["anthropc", "opennai"] {
        config.rate_limits.insert(
            name.to_string(),
            leviath_providers::RateLimitConfig {
                requests_per_minute: 10,
                tokens_per_minute: 1000,
            },
        );
    }
    let check = config_check(&config, &ProviderRegistry::new());
    assert!(
        check
            .detail
            .contains("2 keys in config.toml are read by nothing"),
        "got: {}",
        check.detail
    );
}

#[test]
fn config_check_is_quiet_when_every_rate_limit_names_a_real_provider() {
    let mut config = Config::default();
    config.rate_limits.insert(
        "openrouter".to_string(),
        leviath_providers::RateLimitConfig {
            requests_per_minute: 10,
            tokens_per_minute: 1000,
        },
    );
    let check = config_check(&config, &ProviderRegistry::new());
    assert!(
        !check.detail.contains("read by nothing"),
        "got: {}",
        check.detail
    );
}

// ─── resolve_check ────────────────────────────────────────────────────────────

#[test]
fn resolve_check_uses_the_configured_default() {
    let config = Config {
        default_provider: "stub".to_string(),
        default_model: Some("m-1".to_string()),
        ..Config::default()
    };
    let registry = registry_with("stub", StubProvider::replying("hi"));
    let (check, resolved) = resolve_check(&config, None, &registry);
    assert_eq!(check.status, CheckStatus::Ok);
    assert_eq!(check.detail, "stub / m-1");
    let resolved = resolved.expect("a registered provider resolves");
    assert_eq!(
        (resolved.provider_name.as_str(), resolved.model.as_str()),
        ("stub", "m-1")
    );
}

#[test]
fn resolve_check_honours_a_provider_slash_model_override() {
    let config = Config {
        default_provider: "anthropic".to_string(),
        ..Config::default()
    };
    let registry = registry_with("stub", StubProvider::replying("hi"));
    let (check, resolved) = resolve_check(&config, Some("stub/m-2"), &registry);
    assert_eq!(check.detail, "stub / m-2");
    assert!(resolved.is_some());
}

#[test]
fn resolve_check_reports_an_unconfigured_provider_and_what_was_tried() {
    // The exact shape of the incident this command exists for: nothing is
    // registered, so the chain falls through to its hard-coded last resort.
    let config = Config {
        default_provider: "openrouter".to_string(),
        ..Config::default()
    };
    let (check, resolved) = resolve_check(&config, None, &ProviderRegistry::new());
    assert_eq!(check.status, CheckStatus::Fail);
    assert!(check.detail.contains("not configured"), "{}", check.detail);
    assert!(check.detail.contains("openrouter"), "{}", check.detail);
    assert!(resolved.is_none());
}

#[test]
fn resolve_check_says_so_when_the_configured_default_never_wins() {
    // `default_provider = "openrouter"` with no `default_model`: this check
    // resolves no blueprint, so there is no model to send and the configured
    // provider loses here. The note has to say that WITHOUT implying it holds
    // for real runs, where a blueprint listing openrouter has that entry
    // promoted and every stage goes there. The old wording said the provider
    // "is never chosen", which read as a statement about the user's runs and
    // sent an investigation of a downgraded run in the wrong direction.
    let config = Config {
        default_provider: "openrouter".to_string(),
        default_model: None,
        ..Config::default()
    };
    let mut registry = registry_with("openrouter", StubProvider::replying("hi"));
    registry.register("anthropic".to_string(), StubProvider::replying("hi"));
    let (check, _) = resolve_check(&config, None, &registry);
    assert_eq!(check.status, CheckStatus::Ok);
    assert!(
        check.detail.contains("no blueprint"),
        "the note must scope itself to this check: {}",
        check.detail
    );
    assert!(
        check.detail.contains("A real run is different"),
        "the note must say what real runs do: {}",
        check.detail
    );
    assert!(
        !check.detail.contains("never chosen"),
        "the claim that started this: {}",
        check.detail
    );
    assert!(check.detail.contains("openrouter"), "{}", check.detail);
}

#[test]
fn resolve_check_is_quiet_when_the_configured_default_does_win() {
    let config = Config {
        default_provider: "openrouter".to_string(),
        default_model: Some("openai/gpt-4o-mini".to_string()),
        ..Config::default()
    };
    let registry = registry_with("openrouter", StubProvider::replying("hi"));
    let (check, _) = resolve_check(&config, None, &registry);
    assert_eq!(check.detail, "openrouter / openai/gpt-4o-mini");
}

#[test]
fn resolve_check_reads_a_qualified_default_model_bare_and_says_so() {
    // `default_model = "ollama/qwen3.8:latest"` next to `default_provider =
    // "ollama"`: the run goes to `qwen3.8:latest`, and the note says how the
    // setting was read so the config can be tidied.
    let config = Config {
        default_provider: "stub".to_string(),
        default_model: Some("stub/m-1".to_string()),
        ..Config::default()
    };
    let registry = registry_with("stub", StubProvider::replying("hi"));
    let (check, resolved) = resolve_check(&config, None, &registry);
    assert_eq!(check.status, CheckStatus::Ok);
    assert!(
        check.detail.starts_with("stub / m-1  (note:"),
        "{}",
        check.detail
    );
    assert!(check.detail.contains("'stub/m-1'"), "{}", check.detail);
    assert!(
        check.detail.contains("drop the 'stub/'"),
        "{}",
        check.detail
    );
    assert_eq!(resolved.expect("resolves").model, "m-1");

    // `--model` sidelines the default entirely, note included.
    let (check, _) = resolve_check(&config, Some("stub/m-2"), &registry);
    assert_eq!(check.detail, "stub / m-2");
}

#[test]
fn resolve_check_does_not_second_guess_an_unregistered_default_provider() {
    // Landing somewhere other than a default provider that has no key is not
    // news - the `config` line already lists what is registered, and this
    // check's fail arm covers the case where nothing usable is left. Saying it
    // again here would put a note on every install with a stale provider name
    // in its config.
    for default_model in [None, Some("m-1".to_string())] {
        let config = Config {
            default_provider: "ghost".to_string(),
            default_model,
            ..Config::default()
        };
        let registry = registry_with("anthropic", StubProvider::replying("hi"));
        let (check, _) = resolve_check(&config, None, &registry);
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(!check.detail.contains("note:"), "{}", check.detail);
    }
}

#[test]
fn resolve_check_stays_quiet_under_an_explicit_model_override() {
    // `--model` is the caller overriding on purpose; telling them their
    // default was passed over is noise.
    let config = Config {
        default_provider: "openrouter".to_string(),
        default_model: None,
        ..Config::default()
    };
    let registry = registry_with("stub", StubProvider::replying("hi"));
    let (check, _) = resolve_check(&config, Some("stub/m-2"), &registry);
    assert_eq!(check.detail, "stub / m-2");
}

// ─── inference_check ──────────────────────────────────────────────────────────

#[tokio::test]
async fn inference_check_reports_usage_and_the_echo() {
    let provider = StubProvider::replying("PONG");
    let check = inference_check(provider.as_ref(), "m").await;
    assert_eq!(check.status, CheckStatus::Ok);
    assert!(
        check
            .detail
            .contains("12 in / 4 out / 16 total, replied PONG"),
        "got: {}",
        check.detail
    );
    assert!(check.elapsed_ms.is_some());
}

#[tokio::test]
async fn inference_check_passes_even_without_the_expected_word() {
    // The call is what is being checked. What the model chose to say is a note,
    // not a verdict - otherwise this command would be flaky across providers.
    let provider = StubProvider::replying("Sure! Hello there.");
    let check = inference_check(provider.as_ref(), "m").await;
    assert_eq!(check.status, CheckStatus::Ok);
    assert!(
        check.detail.contains("no PONG in the reply"),
        "{}",
        check.detail
    );
}

#[tokio::test]
async fn inference_check_reports_the_provider_error_verbatim() {
    let raw = r#"HTTP 402 Payment Required: {"error":{"message":"credit balance too low"}}"#;
    let provider = StubProvider::failing(raw);
    let check = inference_check(provider.as_ref(), "m").await;
    assert_eq!(check.status, CheckStatus::Fail);
    assert!(
        check.detail.contains("credit balance too low") && check.detail.contains("402"),
        "the body is the diagnosis and must survive: {}",
        check.detail
    );
}

// ─── The canary blueprint ─────────────────────────────────────────────────────

#[test]
fn the_canary_manifest_is_a_valid_blueprint() {
    let manifest = canary_manifest("openrouter", "anthropic/claude-sonnet-4.5");
    let blueprint =
        leviath_core::manifest::parse_manifest(&manifest).expect("the canary manifest parses");
    blueprint.validate().expect("the canary manifest validates");
    assert_eq!(blueprint.stages.len(), 1, "one stage, one turn");
    let stage = &blueprint.stages[0];
    assert_eq!(stage.max_iterations, Some(1));
    assert!(
        stage.available_tools.is_empty(),
        "a probe with a file tool would be judged for not having used it"
    );
    assert!(stage.transitions.is_none(), "nothing to transition to");
    assert_eq!(stage.model.provider(), "openrouter");
    assert_eq!(stage.model.model(), "anthropic/claude-sonnet-4.5");
}

#[test]
fn the_canary_manifest_escapes_hostile_names() {
    // Provider names come from config, and a quote in one must not be able to
    // close the TOML literal and inject the rest of the file.
    let manifest = canary_manifest("ev\"il", "m\\1");
    let blueprint =
        leviath_core::manifest::parse_manifest(&manifest).expect("an escaped name still parses");
    assert_eq!(blueprint.stages[0].model.provider(), "ev\"il");
    assert_eq!(blueprint.stages[0].model.model(), "m\\1");
}

#[test]
fn stage_canary_reports_a_root_it_cannot_write_into() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"i am a file").expect("write the blocker");
    let err = stage_canary(&blocker, "stub", "m").expect_err("a file is not a directory");
    assert!(!err.to_string().is_empty());
}

#[test]
fn stage_canary_reports_a_manifest_it_cannot_write() {
    // The directory is fine; the manifest path itself is occupied by a
    // directory, so the write is what fails.
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::create_dir_all(dir.path().join("doctor").join("agent.leviath"))
        .expect("occupy the manifest path");
    let err = stage_canary(dir.path(), "stub", "m").expect_err("cannot write over a directory");
    assert!(!err.to_string().is_empty());
}

// ─── Terminal-status reporting ────────────────────────────────────────────────

#[tokio::test]
async fn finished_reads_the_iteration_count_off_disk() {
    with_env(|_root| async {
        write_meta("r-ok", leviath_core::run_meta::RunStatus::Complete, None);
        let DaemonOutcome::Complete(detail) = finished("r-ok", &AgentStatus::Complete) else {
            panic!("a Complete run reports complete");
        };
        assert!(detail.contains("1 iteration(s)"), "{detail}");
    })
    .await;
}

#[tokio::test]
async fn finished_reports_an_errored_run() {
    with_env(|_root| async {
        let outcome = finished(
            "r-err",
            &AgentStatus::Error {
                message: "no usable provider".to_string(),
            },
        );
        let DaemonOutcome::Failed(detail) = outcome else {
            panic!("an errored run fails the check");
        };
        assert!(detail.contains("no usable provider"), "{detail}");
    })
    .await;
}

#[tokio::test]
async fn finished_reports_any_other_terminal_status() {
    with_env(|_root| async {
        let DaemonOutcome::Failed(detail) = finished("r-x", &AgentStatus::Cancelled) else {
            panic!("a cancelled probe is not a pass");
        };
        assert!(detail.contains("cancelled"), "{detail}");
    })
    .await;
}

#[tokio::test]
async fn reaped_falls_back_to_a_finished_meta() {
    with_env(|_root| async {
        write_meta(
            "r-reaped",
            leviath_core::run_meta::RunStatus::Complete,
            None,
        );
        let DaemonOutcome::Complete(detail) = reaped("r-reaped") else {
            panic!("a completed run on disk is a pass");
        };
        assert!(detail.contains("Complete after 1 iteration(s)"), "{detail}");
    })
    .await;
}

#[tokio::test]
async fn reaped_surfaces_a_recorded_error() {
    with_env(|_root| async {
        write_meta(
            "r-bad",
            leviath_core::run_meta::RunStatus::Error,
            Some("provider refused"),
        );
        let DaemonOutcome::Failed(detail) = reaped("r-bad") else {
            panic!("a recorded error fails the check");
        };
        assert!(detail.contains("provider refused"), "{detail}");
    })
    .await;
}

#[tokio::test]
async fn reaped_treats_an_unfinished_run_as_a_lost_one() {
    with_env(|_root| async {
        write_meta("r-live", leviath_core::run_meta::RunStatus::Running, None);
        let DaemonOutcome::Failed(detail) = reaped("r-live") else {
            panic!("a run the daemon forgot mid-flight is a handoff failure");
        };
        assert!(detail.contains("vanished"), "{detail}");
    })
    .await;
}

#[tokio::test]
async fn reaped_treats_a_run_with_no_meta_as_a_lost_one() {
    with_env(|_root| async {
        let DaemonOutcome::Failed(detail) = reaped("r-never") else {
            panic!("a run with nothing on disk is a handoff failure");
        };
        assert!(detail.contains("vanished"), "{detail}");
    })
    .await;
}

// ─── wait_for_run ─────────────────────────────────────────────────────────────

/// Drive `wait_for_run` against a scripted daemon.
async fn wait_against(responses: Vec<&str>, timeout: Duration) -> DaemonOutcome {
    let responses: Vec<String> = responses.into_iter().map(str::to_string).collect();
    with_env(|root| async move {
        let (id, _server) = scripted_daemon(&root, responses);
        wait_for_run(
            &ControlClient::new(id),
            "r-wait",
            timeout,
            Duration::from_millis(1),
        )
        .await
    })
    .await
}

#[tokio::test]
async fn wait_for_run_returns_as_soon_as_the_run_is_terminal() {
    let DaemonOutcome::Complete(detail) =
        wait_against(vec![COMPLETE], Duration::from_secs(5)).await
    else {
        panic!("a Complete status ends the wait");
    };
    assert!(detail.contains("r-wait"), "{detail}");
}

#[tokio::test]
async fn wait_for_run_polls_until_the_run_finishes() {
    let outcome = wait_against(vec![ACTIVE, ACTIVE, COMPLETE], Duration::from_secs(5)).await;
    assert!(matches!(outcome, DaemonOutcome::Complete(_)));
}

#[tokio::test]
async fn wait_for_run_gives_up_on_a_run_that_never_moves() {
    // A zero deadline means the very first poll is also the last one.
    let DaemonOutcome::Failed(detail) = wait_against(vec![ACTIVE], Duration::ZERO).await else {
        panic!("a run that never leaves 'active' is a wedged handoff");
    };
    assert!(detail.contains("still 'active'"), "{detail}");
}

#[tokio::test]
async fn wait_for_run_falls_back_to_disk_when_the_run_is_reaped() {
    let outcome = wait_against(
        vec![r#"{"result":"status","status":null}"#],
        Duration::from_secs(5),
    )
    .await;
    // Nothing was written for `r-wait`, so the fallback reports it lost.
    let DaemonOutcome::Failed(detail) = outcome else {
        panic!("an unknown run with no meta is a failure");
    };
    assert!(detail.contains("vanished"), "{detail}");
}

#[tokio::test]
async fn wait_for_run_rejects_an_unexpected_response() {
    let DaemonOutcome::Failed(detail) =
        wait_against(vec![r#"{"result":"ok","ok":true}"#], Duration::from_secs(5)).await
    else {
        panic!("a nonsense reply is a failure");
    };
    assert!(detail.contains("unexpected daemon response"), "{detail}");
}

#[tokio::test]
async fn wait_for_run_reports_a_lost_connection() {
    // No scripted responses: the listener is already gone when we connect.
    let DaemonOutcome::Failed(detail) = wait_against(vec![], Duration::from_secs(5)).await else {
        panic!("an unreachable daemon is a failure");
    };
    assert!(detail.contains("lost contact"), "{detail}");
}

// ─── spawn_and_wait ───────────────────────────────────────────────────────────

/// Drive `spawn_and_wait` against a scripted daemon, with the canary staged
/// into the isolated root.
async fn spawn_against(responses: Vec<&str>) -> DaemonOutcome {
    let responses: Vec<String> = responses.into_iter().map(str::to_string).collect();
    with_env(|root| async move {
        let (id, _server) = scripted_daemon(&root, responses);
        let manifest = staged(&root);
        spawn_and_wait(
            &ControlClient::new(id),
            &manifest,
            &root,
            Duration::from_secs(5),
            Duration::from_millis(1),
        )
        .await
    })
    .await
}

#[tokio::test]
async fn spawn_and_wait_completes_a_healthy_handoff() {
    let outcome = spawn_against(vec![SPAWNED, COMPLETE]).await;
    assert!(matches!(outcome, DaemonOutcome::Complete(_)), "healthy");
}

#[tokio::test]
async fn spawn_and_wait_reports_a_refused_spawn() {
    let DaemonOutcome::Failed(detail) = spawn_against(vec![
        r#"{"result":"error","message":"stage 'ping' has no usable provider"}"#,
    ])
    .await
    else {
        panic!("a refused spawn fails the check");
    };
    assert!(detail.contains("no usable provider"), "{detail}");
}

#[tokio::test]
async fn spawn_and_wait_rejects_an_unexpected_spawn_response() {
    let DaemonOutcome::Failed(detail) = spawn_against(vec![r#"{"result":"ok","ok":true}"#]).await
    else {
        panic!("a nonsense spawn reply fails the check");
    };
    assert!(
        detail.contains("unexpected daemon response to spawn"),
        "{detail}"
    );
}

#[tokio::test]
async fn spawn_and_wait_reports_an_unreachable_daemon() {
    let DaemonOutcome::Failed(detail) = spawn_against(vec![]).await else {
        panic!("an unreachable daemon fails the check");
    };
    assert!(detail.contains("not reachable"), "{detail}");
}

#[tokio::test]
async fn spawn_and_wait_reports_a_manifest_it_cannot_resolve() {
    let outcome = with_env(|root| async move {
        let (id, _server) = scripted_daemon(&root, Vec::new());
        spawn_and_wait(
            &ControlClient::new(id),
            &root.join("nowhere").join("agent.leviath"),
            &root,
            Duration::from_secs(5),
            Duration::from_millis(1),
        )
        .await
    })
    .await;
    let DaemonOutcome::Failed(detail) = outcome else {
        panic!("a missing manifest fails before the daemon is contacted");
    };
    assert!(
        detail.contains("could not build the spawn request"),
        "{detail}"
    );
}

#[tokio::test]
async fn spawn_and_wait_leaves_nothing_behind() {
    // The whole point of a canary is that it cleans up after itself.
    let leftovers = with_env(|root| async move {
        let (id, _server) = scripted_daemon(&root, vec![SPAWNED.to_string(), COMPLETE.to_string()]);
        let manifest = staged(&root);
        let _ = spawn_and_wait(
            &ControlClient::new(id),
            &manifest,
            &root,
            Duration::from_secs(5),
            Duration::from_millis(1),
        )
        .await;
        crate::runstate::run_dir("doctor-1-abc").exists()
    })
    .await;
    assert!(!leftovers, "the probe run must not survive the check");
}

// ─── daemon_check ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn daemon_check_passes_on_a_healthy_daemon() {
    let check = with_env(|root| async move {
        let (id, _server) = scripted_daemon(&root, vec![SPAWNED.to_string(), COMPLETE.to_string()]);
        daemon_check(
            &ControlClient::new(id),
            "stub",
            "m",
            Duration::from_secs(5),
            Duration::from_millis(1),
            &root,
        )
        .await
    })
    .await;
    assert_eq!(check.status, CheckStatus::Ok);
    assert!(check.elapsed_ms.is_some());
}

#[tokio::test]
async fn daemon_check_fails_when_the_daemon_is_unreachable() {
    let check = with_env(|root| async move {
        let (id, _server) = scripted_daemon(&root, Vec::new());
        daemon_check(
            &ControlClient::new(id),
            "stub",
            "m",
            Duration::from_secs(5),
            Duration::from_millis(1),
            &root,
        )
        .await
    })
    .await;
    assert_eq!(check.status, CheckStatus::Fail);
}

#[tokio::test]
async fn daemon_check_fails_when_the_probe_cannot_be_staged() {
    let check = with_env(|root| async move {
        let (id, _server) = scripted_daemon(&root, Vec::new());
        let blocker = root.join("blocked");
        std::fs::write(&blocker, b"i am a file").expect("write the blocker");
        daemon_check(
            &ControlClient::new(id),
            "stub",
            "m",
            Duration::from_secs(5),
            Duration::from_millis(1),
            &blocker,
        )
        .await
    })
    .await;
    assert_eq!(check.status, CheckStatus::Fail);
    assert!(
        check.detail.contains("could not stage a probe agent"),
        "{}",
        check.detail
    );
}

// ─── cleanup_run ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn cleanup_run_removes_the_run_and_its_saved_state() {
    with_env(|root| async move {
        write_meta("r-clean", leviath_core::run_meta::RunStatus::Complete, None);
        let state = root.join(".leviath").join("state").join("r-clean");
        std::fs::create_dir_all(&state).expect("create the saved state dir");
        cleanup_run("r-clean");
        assert!(!crate::runstate::run_dir("r-clean").exists());
        assert!(!state.exists());
    })
    .await;
}

// ─── run_checks ───────────────────────────────────────────────────────────────

/// A registry builder that ignores the config and always yields `registry`.
fn always(
    registry: ProviderRegistry,
) -> impl Fn(&Config) -> Result<ProviderRegistry, leviath_providers::ProviderError> {
    move |_| Ok(registry.clone())
}

/// Write the smallest `config.toml` that parses, naming `provider`/`model` as
/// the defaults and appending `providers_extra` under `[providers]`.
fn write_config(root: &Path, provider: &str, model: Option<&str>, providers_extra: &str) {
    let model_line = model
        .map(|m| format!("default_model = \"{m}\"\n"))
        .unwrap_or_default();
    std::fs::write(
        root.join("config.toml"),
        format!(
            "agent_paths = []\ndefault_provider = \"{provider}\"\n{model_line}\
             \n[providers]\n{providers_extra}"
        ),
    )
    .expect("write config.toml");
}

#[tokio::test]
async fn run_checks_reports_a_config_that_will_not_parse() {
    let checks = with_env(|root| async move {
        std::fs::write(root.join("config.toml"), "this is not = = toml").expect("write");
        let build = always(ProviderRegistry::new());
        run_checks(&DoctorArgs::default(), &build, DaemonTarget::Skip).await
    })
    .await;
    assert_eq!(checks.len(), 1, "nothing runs after a broken config");
    assert_eq!(checks[0].name, "config");
    assert_eq!(checks[0].status, CheckStatus::Fail);
}

#[tokio::test]
async fn run_checks_stops_at_an_unconfigured_provider() {
    let checks = with_env(|root| async move {
        write_config(&root, "openrouter", None, "");
        let build = always(ProviderRegistry::new());
        run_checks(&DoctorArgs::default(), &build, DaemonTarget::Skip).await
    })
    .await;
    assert_eq!(checks.len(), 3, "no inference is attempted: {checks:?}");
    assert_eq!(checks[2].name, "resolve");
    assert_eq!(checks[2].status, CheckStatus::Fail);
}

#[tokio::test]
async fn run_checks_stops_at_a_failing_inference() {
    let checks = with_env(|root| async move {
        write_config(&root, "stub", Some("m"), "");
        let build = always(registry_with("stub", StubProvider::failing("HTTP 402")));
        run_checks(&DoctorArgs::default(), &build, DaemonTarget::Skip).await
    })
    .await;
    assert_eq!(checks.len(), 4, "the daemon is never contacted: {checks:?}");
    assert_eq!(checks[3].status, CheckStatus::Fail);
}

#[tokio::test]
async fn run_checks_skips_the_daemon_when_asked_to() {
    let checks = with_env(|root| async move {
        write_config(&root, "stub", Some("m"), "");
        let build = always(registry_with("stub", StubProvider::replying("PONG")));
        run_checks(&DoctorArgs::default(), &build, DaemonTarget::Skip).await
    })
    .await;
    assert_eq!(checks.len(), 4, "--no-daemon stops after the inference");
    assert!(checks.iter().all(|c| c.status != CheckStatus::Fail));
}

/// `--offline` proves the config and the resolution and then stops: no
/// inference, no daemon, whatever daemon target the caller had in hand.
#[tokio::test]
async fn run_checks_stops_after_resolve_when_offline() {
    let checks = with_env(|root| async move {
        write_config(&root, "stub", Some("m"), "");
        // A provider that would fail the inference if it were called.
        let build = always(registry_with(
            "stub",
            StubProvider::failing("must not be called"),
        ));
        let args = DoctorArgs {
            offline: true,
            ..DoctorArgs::default()
        };
        let target = DaemonTarget::Unavailable("must not be consulted".to_string());
        run_checks(&args, &build, target).await
    })
    .await;
    assert_eq!(checks.len(), 3, "{checks:?}");
    assert_eq!(checks[2].name, "resolve");
    assert!(checks.iter().all(|c| c.status != CheckStatus::Fail));
}

/// A machine with no writable scratch space fails the daemon check instead
/// of panicking the command.
#[tokio::test]
async fn run_checks_reports_a_scratch_dir_it_cannot_create() {
    fn no_scratch() -> std::io::Result<tempfile::TempDir> {
        Err(std::io::Error::other("scratch is read-only"))
    }
    let checks = with_env(|root| async move {
        write_config(&root, "stub", Some("m"), "");
        let build = always(registry_with("stub", StubProvider::replying("PONG")));
        let (id, _server) = scripted_daemon(&root, vec![LISTING.to_string()]);
        let client = ControlClient::new(id);
        run_checks_with(
            &DoctorArgs::default(),
            &build,
            DaemonTarget::Client(&client),
            no_scratch,
        )
        .await
    })
    .await;
    assert_eq!(checks.len(), 5, "{checks:?}");
    assert_eq!(checks[4].name, "daemon");
    assert_eq!(checks[4].status, CheckStatus::Fail);
    assert!(
        checks[4].detail.contains("scratch is read-only"),
        "{checks:?}"
    );
}

#[tokio::test]
async fn run_checks_reports_a_daemon_that_would_not_start() {
    // The credentials are fine and the report says so; the daemon is the
    // problem, and the report says that too. Answering neither - which is what
    // aborting on the auto-start failure would do - is the thing to avoid.
    let checks = with_env(|root| async move {
        write_config(&root, "stub", Some("m"), "");
        let build = always(registry_with("stub", StubProvider::replying("PONG")));
        let target = DaemonTarget::Unavailable("did not start within 5s".to_string());
        run_checks(&DoctorArgs::default(), &build, target).await
    })
    .await;
    assert_eq!(checks.len(), 5, "{checks:?}");
    assert!(checks[..4].iter().all(|c| c.status != CheckStatus::Fail));
    assert_eq!(checks[4].status, CheckStatus::Fail);
    assert!(checks[4].detail.contains("did not start"), "{checks:?}");
}

#[tokio::test]
async fn run_checks_runs_all_five_against_a_healthy_daemon() {
    let checks = with_env(|root| async move {
        // A key of the wrong shape, so the warning branch runs too.
        write_config(
            &root,
            "stub",
            Some("m"),
            "anthropic_api_key = \"not-a-real-shape\"\n",
        );
        let build = always(registry_with("stub", StubProvider::replying("PONG")));
        let (id, _server) = scripted_daemon(
            &root,
            vec![
                LISTING.to_string(),
                SPAWNED.to_string(),
                COMPLETE.to_string(),
            ],
        );
        let client = ControlClient::new(id);
        run_checks(
            &DoctorArgs::default(),
            &build,
            DaemonTarget::Client(&client),
        )
        .await
    })
    .await;
    assert_eq!(checks.len(), 5, "{checks:?}");
    assert!(
        checks.iter().all(|c| c.status != CheckStatus::Fail),
        "{checks:?}"
    );
}

// ─── execute ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn execute_prints_a_table_and_succeeds() {
    let result = with_env(|root| async move {
        write_config(&root, "stub", Some("m"), "");
        let build = always(registry_with("stub", StubProvider::replying("PONG")));
        execute_with_registry(DoctorArgs::default(), &build, DaemonTarget::Skip).await
    })
    .await;
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn execute_prints_json_when_asked() {
    let result = with_env(|root| async move {
        write_config(&root, "stub", Some("m"), "");
        let build = always(registry_with("stub", StubProvider::replying("PONG")));
        let args = DoctorArgs {
            json: true,
            ..DoctorArgs::default()
        };
        execute_with_registry(args, &build, DaemonTarget::Skip).await
    })
    .await;
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn execute_names_the_check_that_failed() {
    let err = with_env(|root| async move {
        write_config(&root, "stub", Some("m"), "");
        let build = always(registry_with("stub", StubProvider::failing("HTTP 402")));
        execute_with_registry(DoctorArgs::default(), &build, DaemonTarget::Skip)
            .await
            .expect_err("a failing inference fails the command")
    })
    .await;
    assert_eq!(err.to_string(), "doctor failed at: inference");
}

#[tokio::test]
async fn execute_wires_the_real_registry_builder() {
    // `execute` differs from `execute_with_registry` only in that builder, and
    // with no credentials in the isolated env it resolves to nothing usable -
    // which is the answer, and proves the production seam is connected.
    let err = with_env(|root| async move {
        write_config(&root, "nothing-here", None, "");
        execute(DoctorArgs::default(), DaemonTarget::Skip)
            .await
            .expect_err("no configured provider fails the command")
    })
    .await;
    assert_eq!(err.to_string(), "doctor failed at: resolve");
}

/// A registry builder that fails the way a machine with an unreadable root
/// certificate store would.
fn cannot_build(_config: &Config) -> Result<ProviderRegistry, leviath_providers::ProviderError> {
    Err(leviath_providers::ProviderError::ClientBuild(
        "no roots".to_string(),
    ))
}

#[tokio::test]
async fn a_registry_that_will_not_build_is_reported_as_a_failed_check() {
    let checks = with_env(|root| async move {
        write_config(&root, "anthropic", None, "");
        run_checks(&DoctorArgs::default(), &cannot_build, DaemonTarget::Skip).await
    })
    .await;
    let providers = checks
        .iter()
        .find(|c| c.name == "providers")
        .expect("a providers check");
    assert!(
        providers
            .detail
            .contains("could not build any provider client")
    );
}
