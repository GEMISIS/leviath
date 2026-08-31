//! The provider end to end, against a local socket.
//!
//! The diagnostics matter as much as the happy path here. A 403 on this route
//! is almost always the client identity, and the stock remedy sends the reader
//! to check model permissions instead, so the message is tested rather than
//! assumed.

use super::*;
use crate::codex::store::{ProviderAuthStore, ProviderGrant};
use crate::codex::token::RefreshError;
use crate::provider::{Message, MessageContent, SystemBlock};
use leviath_testkit::{spawn_mock_sequence, spawn_mock_server};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A token source with no file behind it, for the paths that never refresh.
struct Static {
    token: String,
    plan: Option<String>,
    refreshes: AtomicUsize,
}

impl Static {
    fn new(token: &str) -> Arc<Self> {
        Arc::new(Self {
            token: token.to_string(),
            plan: Some("plus".to_string()),
            refreshes: AtomicUsize::new(0),
        })
    }

    fn with_plan(token: &str, plan: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            token: token.to_string(),
            plan: plan.map(str::to_string),
            refreshes: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl TokenSource for Static {
    async fn credentials(&self) -> std::result::Result<Credentials, RefreshError> {
        Ok(Credentials {
            access_token: self.token.clone(),
            account_id: Some("acct-1".to_string()),
        })
    }

    async fn refresh_stale(&self, _: &str) -> std::result::Result<Credentials, RefreshError> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(Credentials {
            access_token: "refreshed-token".to_string(),
            account_id: Some("acct-1".to_string()),
        })
    }

    fn grant(&self) -> Option<ProviderGrant> {
        Some(ProviderGrant {
            access_token: self.token.clone(),
            plan_type: self.plan.clone(),
            ..Default::default()
        })
    }
}

/// A source whose refresh has already failed for good.
struct Dead;

#[async_trait]
impl TokenSource for Dead {
    async fn credentials(&self) -> std::result::Result<Credentials, RefreshError> {
        Err(RefreshError::Terminal(
            "run `lev auth login codex` to sign in again".to_string(),
        ))
    }

    async fn refresh_stale(&self, _: &str) -> std::result::Result<Credentials, RefreshError> {
        Err(RefreshError::Terminal("gone".to_string()))
    }

    fn grant(&self) -> Option<ProviderGrant> {
        None
    }
}

fn provider(url: &str, tokens: Arc<dyn TokenSource>) -> CodexProvider {
    CodexProvider::new(reqwest::Client::new(), tokens).with_base_url(Some(url.to_string()))
}

fn request() -> InferenceRequest {
    InferenceRequest {
        system: vec![SystemBlock {
            text: "## task\ndo it".to_string(),
            cache_hint: leviath_core::CacheHint::Always,
            region: "task".to_string(),
            volatility: leviath_core::Volatility::Stable,
        }],
        messages: vec![Message {
            role: "user".to_string(),
            content: MessageContent::Text("go".to_string()),
            cache_breakpoint: false,
            reasoning: None,
        }],
        model: "gpt-5.6-sol".to_string(),
        max_tokens: 1024,
        temperature: 0.7,
        tools: vec![],
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    }
}

/// A minimal successful SSE response.
fn ok_stream() -> Vec<u8> {
    let mut body = String::new();
    body.push_str("data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n");
    body.push_str(
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\
         \"output\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n\n",
    );
    body.into_bytes()
}

#[tokio::test]
async fn a_successful_turn_collects_into_a_response() {
    let url = spawn_mock_server(200, "OK", ok_stream()).await;
    let response = provider(&url, Static::new("t"))
        .infer(&request())
        .await
        .expect("inference");
    assert_eq!(response.content, "ok");
    assert_eq!(response.tokens_used.completion_tokens, 2);
}

#[tokio::test]
async fn a_401_refreshes_once_and_retries_with_the_new_token() {
    // The 401 is intercepted before `check_http_response`, which would map it
    // to a provider-fatal AuthFailed and trip the breaker. On this route an
    // expired token is routine.
    let (url, _bodies) = spawn_mock_sequence(vec![
        (401, "Unauthorized", b"{\"detail\":\"expired\"}".to_vec()),
        (200, "OK", ok_stream()),
    ])
    .await;
    let tokens = Static::new("stale");
    let response = provider(&url, tokens.clone())
        .infer(&request())
        .await
        .expect("inference");
    assert_eq!(response.content, "ok");
    assert_eq!(tokens.refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_401_that_survives_the_refresh_is_reported_as_auth_failed() {
    let (url, _bodies) = spawn_mock_sequence(vec![
        (401, "Unauthorized", b"nope".to_vec()),
        (401, "Unauthorized", b"still nope".to_vec()),
    ])
    .await;
    let err = provider(&url, Static::new("stale"))
        .infer(&request())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ProviderError::Unavailable {
                reason: UnavailableReason::AuthFailed,
                ..
            }
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_401_whose_refresh_fails_reports_the_refresh_failure() {
    // The retry never goes out: there is no token to send it with.
    struct RefusesToRefresh;
    #[async_trait]
    impl TokenSource for RefusesToRefresh {
        async fn credentials(&self) -> std::result::Result<Credentials, RefreshError> {
            Ok(Credentials {
                access_token: "stale".to_string(),
                account_id: None,
            })
        }
        async fn refresh_stale(&self, _: &str) -> std::result::Result<Credentials, RefreshError> {
            Err(RefreshError::Terminal(
                "the ChatGPT session has expired".to_string(),
            ))
        }
        fn grant(&self) -> Option<ProviderGrant> {
            None
        }
    }

    let url = spawn_mock_server(401, "Unauthorized", b"expired".to_vec()).await;
    let err = provider(&url, Arc::new(RefusesToRefresh))
        .infer(&request())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("session has expired"), "got {err}");
}

#[tokio::test]
async fn a_retry_that_cannot_be_sent_is_a_transport_failure() {
    // One response in the sequence, so the listener is gone by the time the
    // refreshed request goes out.
    let (url, _bodies) =
        spawn_mock_sequence(vec![(401, "Unauthorized", b"expired".to_vec())]).await;
    let err = provider(&url, Static::new("stale"))
        .infer(&request())
        .await
        .unwrap_err();
    assert!(err.failure_kind().is_some(), "got {err:?}");
}

#[tokio::test]
async fn a_dead_grant_says_how_to_sign_in_again() {
    let url = spawn_mock_server(200, "OK", ok_stream()).await;
    let err = provider(&url, Arc::new(Dead))
        .infer(&request())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("lev auth login codex"),
        "got {err}"
    );
}

#[tokio::test]
async fn a_403_names_the_identity_that_was_actually_sent() {
    // The one fact that makes this debuggable; the stock Forbidden remedy
    // points at model permissions, which is the wrong place to look.
    let url = spawn_mock_server(403, "Forbidden", b"{\"detail\":\"no\"}".to_vec()).await;
    let err = provider(&url, Static::new("t"))
        .with_originator(Some("leviath".to_string()))
        .infer(&request())
        .await
        .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("originator: leviath"), "got {message}");
    assert!(message.contains("codex_originator"), "got {message}");
    assert!(
        matches!(
            err,
            ProviderError::Unavailable {
                reason: UnavailableReason::Forbidden,
                ..
            }
        ),
        "a 403 must fail over rather than kill the run"
    );
}

#[tokio::test]
async fn a_plan_gated_model_names_the_plan_and_the_alternatives() {
    let body = b"{\"detail\":\"The 'gpt-5.3-codex-spark' model is not supported when using \
                 Codex with a ChatGPT account.\"}"
        .to_vec();
    let url = spawn_mock_server(400, "Bad Request", body).await;
    let mut req = request();
    req.model = "gpt-5.3-codex-spark".to_string();

    let err = provider(&url, Static::new("t"))
        .infer(&req)
        .await
        .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("plus plan does not include"),
        "got {message}"
    );
    assert!(message.contains("gpt-5.6-sol"), "got {message}");
}

#[tokio::test]
async fn a_gating_message_about_a_different_model_is_not_mistaken_for_this_one() {
    let body = b"{\"detail\":\"The 'some-other-model' model is not supported when using \
                 Codex with a ChatGPT account.\"}"
        .to_vec();
    let url = spawn_mock_server(400, "Bad Request", body).await;
    let err = provider(&url, Static::new("t"))
        .infer(&request())
        .await
        .unwrap_err();
    assert!(!err.to_string().contains("does not include"), "got {err}");
}

#[tokio::test]
async fn a_rejected_parameter_is_reported_as_a_leviath_bug() {
    // The user cannot act on this and should not be sent looking at their
    // account for it.
    let url = spawn_mock_server(
        400,
        "Bad Request",
        b"{\"detail\":\"Unsupported parameter: temperature\"}".to_vec(),
    )
    .await;
    let err = provider(&url, Static::new("t"))
        .infer(&request())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("bug in Leviath"), "got {err}");
}

#[tokio::test]
async fn a_429_with_a_retry_after_uses_it() {
    let url = leviath_testkit::spawn_mock_server_with_headers(
        429,
        "Too Many Requests",
        "Content-Type: application/json\r\nRetry-After: 42\r\n",
        b"slow down".to_vec(),
    )
    .await;
    let err = provider(&url, Static::new("t"))
        .infer(&request())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ProviderError::RateLimitExceeded {
                retry_after_secs: Some(42)
            }
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_429_with_no_reachable_quota_still_reports_a_rate_limit() {
    // Nothing to add about when to retry, but the error must still be a rate
    // limit rather than a generic failure: the two are handled differently.
    let url = spawn_mock_server(429, "Too Many Requests", b"slow down".to_vec()).await;
    let err = provider(&url, Static::new("t"))
        .with_usage_url(Some("http://127.0.0.1:1".to_string()))
        .infer(&request())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ProviderError::RateLimitExceeded {
                retry_after_secs: None
            }
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_429_tells_the_rate_limiter_about_it() {
    // The limiter backs off on the provider's own answer rather than on a
    // guess, which is the whole point of carrying the header through.
    let url = leviath_testkit::spawn_mock_server_with_headers(
        429,
        "Too Many Requests",
        "Content-Type: application/json\r\nRetry-After: 7\r\n",
        b"slow down".to_vec(),
    )
    .await;
    let err = provider(&url, Static::new("t"))
        .with_rate_limit(Some(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        }))
        .with_usage_url(Some("http://127.0.0.1:1".to_string()))
        .infer(&request())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ProviderError::RateLimitExceeded {
                retry_after_secs: Some(7)
            }
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn an_ordinary_failure_keeps_its_status_and_body() {
    let url = spawn_mock_server(500, "Internal Server Error", b"boom".to_vec()).await;
    let err = provider(&url, Static::new("t"))
        .infer(&request())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("500"), "got {err}");
    assert!(err.to_string().contains("boom"), "got {err}");
}

#[tokio::test]
async fn a_drained_account_still_fails_over_rather_than_dying() {
    let url = spawn_mock_server(402, "Payment Required", b"out of credits".to_vec()).await;
    let err = provider(&url, Static::new("t"))
        .infer(&request())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ProviderError::Unavailable {
                reason: UnavailableReason::CreditsExhausted,
                ..
            }
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn an_unreachable_host_is_a_transport_failure() {
    // Port 0 never listens, so this is a genuine connect error rather than a
    // status the server chose.
    let err = provider("http://127.0.0.1:1", Static::new("t"))
        .infer(&request())
        .await
        .unwrap_err();
    assert!(err.failure_kind().is_some(), "got {err:?}");
}

#[tokio::test]
async fn the_quota_route_is_read_and_parsed() {
    let body = br#"{"plan_type":"plus","rate_limit":{"primary_window":
        {"used_percent":10,"limit_window_seconds":18000,"reset_at":99}}}"#
        .to_vec();
    let url = spawn_mock_server(200, "OK", body).await;
    let quota = provider("http://x", Static::new("t"))
        .with_usage_url(Some(url))
        .quota()
        .await
        .expect("quota");
    assert_eq!(quota.plan_type.as_deref(), Some("plus"));
    assert_eq!(quota.primary.expect("primary").window_secs, 18_000);
}

#[tokio::test]
async fn a_429_with_no_retry_after_falls_back_to_the_window_reset() {
    // The highest-value use of the quota route: without it the retry loop
    // guesses with backoff against a limit that resets on a wall clock.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_secs();
    let usage = format!(
        r#"{{"rate_limit":{{"primary_window":{{"used_percent":100,
           "limit_window_seconds":18000,"reset_at":{}}}}}}}"#,
        now + 90
    );
    let usage_url = spawn_mock_server(200, "OK", usage.into_bytes()).await;
    let url = spawn_mock_server(429, "Too Many Requests", b"slow down".to_vec()).await;

    let err = provider(&url, Static::new("t"))
        .with_usage_url(Some(usage_url))
        .infer(&request())
        .await
        .unwrap_err();
    match err {
        ProviderError::RateLimitExceeded {
            retry_after_secs: Some(secs),
        } => assert!((80..=90).contains(&secs), "got {secs}"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn the_provider_answers_to_its_registry_name() {
    let p = provider("http://x", Static::new("t"));
    assert_eq!(p.name(), "codex");
}

#[test]
fn a_subscription_call_costs_a_known_zero_rather_than_an_unknown() {
    // `None` would put every run of a subscription in the "cost unavailable"
    // bucket, which is a different and worse wrong answer than zero.
    let pricing = provider("http://x", Static::new("t"))
        .pricing("gpt-5.6-sol")
        .expect("a known zero");
    assert_eq!(pricing.input_per_mtok, 0.0);
    assert_eq!(pricing.output_per_mtok, 0.0);
}

#[test]
fn this_provider_never_wins_a_bare_model_name() {
    // Enabling a subscription transport must not silently re-route existing
    // stages onto the subscription.
    assert!(provider("http://x", Static::new("t")).explicit_route_only());
}

#[test]
fn no_model_is_offered_with_a_temperature() {
    let p = provider("http://x", Static::new("t"));
    assert!(!p.capabilities("gpt-5.6-sol").supports_temperature);
    assert_eq!(p.max_context_tokens("gpt-5.6-sol"), 400_000);
}

#[test]
fn an_operator_override_corrects_only_what_it_names() {
    let mut overrides = std::collections::HashMap::new();
    overrides.insert(
        "gpt-5.6-sol".to_string(),
        ModelCapabilityOverride {
            max_context_tokens: Some(123_456),
            ..Default::default()
        },
    );
    let p = provider("http://x", Static::new("t")).with_overrides(Some(overrides));
    let caps = p.capabilities("gpt-5.6-sol");
    assert_eq!(caps.max_context_tokens, 123_456);
    // Still not a temperature: an override that names one field must not reset
    // the rest of the row.
    assert!(!caps.supports_temperature);
    assert_eq!(caps.max_output_tokens, 128_000);
}

#[test]
fn a_plus_plan_does_not_claim_the_pro_preview() {
    let p = provider("http://x", Static::with_plan("t", Some("plus")));
    assert_eq!(
        p.serves_model("gpt-5.6-sol").as_deref(),
        Some("gpt-5.6-sol")
    );
    assert!(p.serves_model("gpt-5.3-codex-spark").is_none());
    assert!(p.serves_model("claude-opus-5").is_none());
}

#[test]
fn an_unknown_plan_claims_everything_in_the_catalog() {
    let p = provider("http://x", Static::with_plan("t", None));
    assert!(p.serves_model("gpt-5.3-codex-spark").is_some());
}

#[test]
fn an_override_makes_an_unlisted_model_servable() {
    let mut overrides = std::collections::HashMap::new();
    overrides.insert(
        "gpt-5.7-unreleased".to_string(),
        ModelCapabilityOverride::default(),
    );
    let p = provider("http://x", Static::new("t")).with_overrides(Some(overrides));
    assert!(p.serves_model("gpt-5.7-unreleased").is_some());
}

#[tokio::test]
async fn the_catalog_is_not_promised_until_the_plan_is_known() {
    // `Some` is a completeness promise the lint turns into a hard error, so
    // "cannot say" has to stay `None` until there is something to say.
    let p = provider("http://x", Static::new("t"));
    assert!(p.served_catalog().is_none());
    p.prime_capabilities().await.expect("priming never fails");
    let served = p.served_catalog().expect("primed");
    assert!(served.contains(&"gpt-5.6-sol".to_string()));
    assert!(!served.contains(&"gpt-5.3-codex-spark".to_string()));
}

#[tokio::test]
async fn priming_without_a_grant_does_not_stop_the_daemon() {
    // A provider that cannot reach its own API degrades to its table.
    let p = provider("http://127.0.0.1:1", Arc::new(Dead));
    p.prime_capabilities()
        .await
        .expect("priming is best effort");
}

#[tokio::test]
async fn the_listing_reports_the_compiled_table_as_such() {
    let p = provider("http://x", Static::new("t"));
    let models = p.list_models().await.expect("listing");
    assert_eq!(models.len(), crate::codex::catalog::CATALOG.len());
    for model in &models {
        assert_eq!(model.provider, "codex");
        assert!(model.display_name.is_some());
        assert!(!model.learned, "a compiled table is not a listing");
    }
}

#[tokio::test]
async fn the_listing_narrows_once_the_plan_is_known() {
    let p = provider("http://x", Static::new("t"));
    p.prime_capabilities().await.expect("priming");
    let models = p.list_models().await.expect("listing");
    assert!(!models.iter().any(|m| m.id == "gpt-5.3-codex-spark"));
}

#[tokio::test]
async fn counting_tokens_uses_the_openai_tokenizer_rather_than_a_byte_guess() {
    let p = provider("http://x", Static::new("t"));
    let count = p.count_tokens("hello world", "gpt-5.6-sol").await;
    assert!(count > 0 && count < 10, "got {count}");
}

#[test]
fn the_builders_reject_values_the_route_would_refuse() {
    let p = provider("http://x", Static::new("t"))
        .with_reasoning(Some("nonsense".to_string()), Some("nonsense".to_string()));
    assert_eq!(
        p.reasoning_effort, "medium",
        "an invalid effort must not be sent"
    );
    assert_eq!(p.verbosity, "medium");

    let p = provider("http://x", Static::new("t"))
        .with_reasoning(Some("xhigh".to_string()), Some("high".to_string()));
    assert_eq!(p.reasoning_effort, "xhigh");
    assert_eq!(p.verbosity, "high");
}

#[test]
fn an_empty_originator_leaves_the_default_alone() {
    let p = provider("http://x", Static::new("t")).with_originator(Some("  ".to_string()));
    assert_eq!(p.originator, "leviath");
    let p = provider("http://x", Static::new("t")).with_originator(None);
    assert_eq!(p.originator, "leviath");
}

#[test]
fn the_user_agent_follows_the_originator_unless_set_outright() {
    let p =
        provider("http://x", Static::new("t")).with_originator(Some("Codex Leviath".to_string()));
    assert!(
        p.user_agent.starts_with("Codex Leviath/"),
        "got {}",
        p.user_agent
    );

    let p = provider("http://x", Static::new("t"))
        .with_originator(Some("Codex Leviath".to_string()))
        .with_user_agent(Some("custom/1.0".to_string()));
    assert_eq!(p.user_agent, "custom/1.0");

    // An empty override is not an override.
    let p = provider("http://x", Static::new("t")).with_user_agent(Some(" ".to_string()));
    assert!(p.user_agent.starts_with("leviath/"));
    let p = provider("http://x", Static::new("t")).with_user_agent(None);
    assert!(p.user_agent.starts_with("leviath/"));
}

#[test]
fn the_base_url_loses_a_trailing_slash_so_the_path_join_stays_clean() {
    let p = provider("http://x", Static::new("t")).with_base_url(Some("http://y/".to_string()));
    assert_eq!(p.base_url, "http://y");
    let p = provider("http://x", Static::new("t")).with_base_url(None);
    assert_eq!(p.base_url, "http://x");
}

#[test]
fn the_remaining_builders_are_wired() {
    let p = provider("http://x", Static::new("t"))
        .with_reasoning_replay(false)
        .with_request_timeout(Some(30))
        .with_rate_limit(Some(&RateLimitConfig {
            requests_per_minute: 10,
            tokens_per_minute: 1000,
        }));
    assert!(!p.replay_reasoning);
    assert_eq!(p.request_timeout_secs, Some(30));
    assert!(p.rate_limiter.is_some());

    let p = provider("http://x", Static::new("t")).with_rate_limit(None);
    assert!(p.rate_limiter.is_none());
}

#[tokio::test]
async fn a_rate_limited_provider_still_sends() {
    // The limiter is acquired before the request; a misconfigured one must not
    // deadlock the call.
    let url = spawn_mock_server(200, "OK", ok_stream()).await;
    let response = provider(&url, Static::new("t"))
        .with_rate_limit(Some(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        }))
        .infer(&request())
        .await
        .expect("inference");
    assert_eq!(response.content, "ok");
}

#[tokio::test]
async fn a_stored_grant_supplies_the_plan_without_a_network_call() {
    // The tier is already in the id token, so priming costs nothing in the
    // common case; only an unreadable token sends us to the usage route.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("provider-auth.json");
    let mut store = ProviderAuthStore::default();
    store.set(
        "codex",
        ProviderGrant {
            access_token: "t".to_string(),
            plan_type: Some("pro".to_string()),
            ..Default::default()
        },
    );
    store.save(&path).expect("save");

    struct Source(ProviderGrant);
    #[async_trait]
    impl TokenSource for Source {
        async fn credentials(&self) -> std::result::Result<Credentials, RefreshError> {
            Ok(Credentials::default())
        }
        async fn refresh_stale(&self, _: &str) -> std::result::Result<Credentials, RefreshError> {
            Ok(Credentials::default())
        }
        fn grant(&self) -> Option<ProviderGrant> {
            Some(self.0.clone())
        }
    }

    let grant = ProviderAuthStore::load(&path)
        .unwrap()
        .get("codex")
        .cloned()
        .unwrap();
    let p = provider("http://127.0.0.1:1", Arc::new(Source(grant)));
    p.prime_capabilities().await.expect("priming");
    // Pro reaches the preview, so the catalog is not narrowed.
    assert!(
        p.served_catalog()
            .expect("primed")
            .contains(&"gpt-5.3-codex-spark".to_string())
    );
}

#[test]
fn a_refreshed_token_is_never_the_one_a_refresh_error_names() {
    // Guards the mapping from a refresh failure to a provider error: the
    // remedy has to survive, since it is the only thing that tells a user to
    // sign in again.
    let err = unavailable(RefreshError::Terminal(
        "run `lev auth login codex`".to_string(),
    ));
    assert!(
        err.to_string().contains("lev auth login codex"),
        "got {err}"
    );
    assert!(matches!(
        err,
        ProviderError::Unavailable {
            reason: UnavailableReason::AuthFailed,
            ..
        }
    ));
}

#[test]
fn the_gating_check_needs_both_the_phrase_and_the_model() {
    assert!(plan_gated(
        "The 'gpt-5.3-codex-spark' model is not supported when using Codex with a ChatGPT account.",
        "gpt-5.3-codex-spark"
    ));
    assert!(!plan_gated("some other 400", "gpt-5.6-sol"));
    assert!(!plan_gated(
        "The 'other' model is not supported when using Codex with a ChatGPT account.",
        "gpt-5.6-sol"
    ));
}

#[tokio::test]
async fn an_unreachable_quota_route_is_an_error_rather_than_a_hang() {
    let p = provider("http://x", Static::new("t"))
        .with_usage_url(Some("http://127.0.0.1:1".to_string()));
    assert!(p.quota().await.is_err());
}

#[tokio::test]
async fn a_quota_body_in_an_unknown_shape_is_reported() {
    let url = spawn_mock_server(200, "OK", b"not json".to_vec()).await;
    let p = provider("http://x", Static::new("t")).with_usage_url(Some(url));
    let err = p.quota().await.unwrap_err();
    assert!(err.to_string().contains("known shape"), "got {err}");
}

#[test]
fn an_empty_usage_url_leaves_the_default_alone() {
    let p = provider("http://x", Static::new("t")).with_usage_url(Some("  ".to_string()));
    assert_eq!(p.usage_url, crate::codex::USAGE_URL);
    let p = provider("http://x", Static::new("t")).with_usage_url(None);
    assert_eq!(p.usage_url, crate::codex::USAGE_URL);
}
