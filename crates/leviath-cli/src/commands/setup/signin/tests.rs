//! The sign-in lane, driven with a canned authorizer so nothing opens a
//! browser or binds a port.

use super::*;
use leviath_providers::ProviderGrant;

/// An authorizer that answers from a script.
struct Canned {
    /// What a sign-in returns: the identity, or the message to fail with.
    sign_in: Result<String, String>,
    /// What a sign-out returns.
    sign_out: Result<(), String>,
    /// The URL to announce before answering, when there is one.
    url: Option<String>,
}

impl Canned {
    fn signing_in_as(who: &str) -> Self {
        Self {
            sign_in: Ok(who.to_string()),
            sign_out: Ok(()),
            url: Some("https://auth.example/authorize?x=1".to_string()),
        }
    }
}

impl ProviderAuthorizer for Canned {
    async fn sign_in(
        &self,
        _provider_id: &str,
        announce: codex_login::Announce,
    ) -> anyhow::Result<String> {
        if let Some(url) = &self.url {
            announce(url);
        }
        self.sign_in
            .clone()
            .map_err(|e| anyhow::anyhow!(e).context("could not sign in"))
    }

    async fn sign_out(&self, _provider_id: &str) -> anyhow::Result<()> {
        self.sign_out.clone().map_err(|e| anyhow::anyhow!(e))
    }
}

/// Run one request through the lane and collect everything it reported.
async fn drive(authorizer: Canned, action: SigninAction) -> Vec<SigninEvent> {
    let (request_tx, request_rx) = mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    request_tx
        .send(SigninRequest {
            provider_id: "codex".to_string(),
            action,
        })
        .unwrap();
    // Dropping the sender ends the loop once the one request is served.
    drop(request_tx);
    signin_loop(authorizer, request_rx, event_tx).await;
    let mut events = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        events.push(event);
    }
    events
}

/// The URL comes back before the answer does, which is the whole reason the
/// lane reports more than once per request.
#[tokio::test]
async fn a_sign_in_announces_its_url_and_then_who_it_signed_in() {
    let events = drive(Canned::signing_in_as("a@b.c (plus plan)"), SigninAction::In).await;
    assert_eq!(events.len(), 2, "{events:?}");
    match &events[0] {
        SigninEvent::Opened { provider_id, url } => {
            assert_eq!(provider_id, "codex");
            assert!(url.starts_with("https://auth.example/"), "{url}");
        }
        other => panic!("expected the URL first, got {other:?}"),
    }
    match &events[1] {
        SigninEvent::SignedIn { who, .. } => assert_eq!(who, "a@b.c (plus plan)"),
        other => panic!("expected a signed-in event, got {other:?}"),
    }
}

/// A sign-out has no URL to announce, so it reports once.
#[tokio::test]
async fn a_sign_out_reports_only_that_it_finished() {
    let events = drive(Canned::signing_in_as("nobody"), SigninAction::Out).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(
        matches!(events[0], SigninEvent::SignedOut { .. }),
        "{events:?}"
    );
}

/// The cause is kept, because the cause is the half that says what to do.
#[tokio::test]
async fn a_failed_sign_in_reports_the_whole_chain() {
    let events = drive(
        Canned {
            sign_in: Err("could not listen on port 1455".to_string()),
            sign_out: Ok(()),
            url: None,
        },
        SigninAction::In,
    )
    .await;
    match &events[..] {
        [SigninEvent::Failed { message, .. }] => {
            assert!(message.contains("could not sign in"), "{message}");
            assert!(message.contains("port 1455"), "{message}");
        }
        other => panic!("expected one failure, got {other:?}"),
    }
}

/// A failed sign-out is reported the same way.
#[tokio::test]
async fn a_failed_sign_out_is_reported_too() {
    let events = drive(
        Canned {
            sign_in: Ok(String::new()),
            sign_out: Err("the keychain refused".to_string()),
            url: None,
        },
        SigninAction::Out,
    )
    .await;
    match &events[..] {
        [SigninEvent::Failed { message, .. }] => {
            assert!(message.contains("keychain"), "{message}");
        }
        other => panic!("expected one failure, got {other:?}"),
    }
}

/// A wizard that has already closed leaves the lane nothing to report to, and
/// it stops rather than looping on a dead channel.
#[tokio::test]
async fn the_lane_stops_when_the_wizard_is_gone() {
    let (request_tx, request_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    for _ in 0..2 {
        request_tx
            .send(SigninRequest {
                provider_id: "codex".to_string(),
                action: SigninAction::Out,
            })
            .unwrap();
    }
    drop(request_tx);
    drop(event_rx);
    // Returns rather than hanging or panicking on the closed reply channel.
    signin_loop(Canned::signing_in_as("x"), request_rx, event_tx).await;
}

/// Every event knows which row it belongs to, including the two that carry
/// nothing else.
#[test]
fn every_event_names_its_provider() {
    let events = [
        SigninEvent::Opened {
            provider_id: "a".to_string(),
            url: String::new(),
        },
        SigninEvent::SignedIn {
            provider_id: "b".to_string(),
            who: String::new(),
        },
        SigninEvent::SignedOut {
            provider_id: "c".to_string(),
        },
        SigninEvent::Failed {
            provider_id: "d".to_string(),
            message: String::new(),
        },
    ];
    let named: Vec<&str> = events.iter().map(SigninEvent::provider_id).collect();
    assert_eq!(named, ["a", "b", "c", "d"]);
}

/// The identity line prefers the stored fields, falls back to the token's
/// claims, and still says something for a grant that carries neither.
#[test]
fn the_identity_line_falls_back_all_the_way_to_signed_in() {
    let bare = ProviderGrant {
        access_token: "at".to_string(),
        refresh_token: "rt".to_string(),
        ..Default::default()
    };
    assert_eq!(describe(&bare), "signed in");

    let with_email = ProviderGrant {
        email: Some("someone@example.com".to_string()),
        ..bare.clone()
    };
    assert_eq!(describe(&with_email), "someone@example.com");

    let with_plan = ProviderGrant {
        plan_type: Some("plus".to_string()),
        ..with_email
    };
    assert_eq!(describe(&with_plan), "someone@example.com (plus plan)");
}

/// Only Codex signs in with a browser, and anything else is told so rather
/// than being handed to a flow written for one provider.
#[tokio::test]
async fn a_provider_that_does_not_sign_in_is_refused() {
    let authorizer = LiveAuthorizer {
        opener: Arc::new(|_: &str| true),
        store_path: None,
        credential_store: Ok(None),
        client: reqwest::Client::new(),
        issuer: "http://127.0.0.1:1".to_string(),
        ports: vec![0],
    };
    let announce: codex_login::Announce = Arc::new(|_: &str| {});
    let error = authorizer
        .sign_in("anthropic", announce)
        .await
        .expect_err("anthropic has no browser sign-in");
    assert!(error.to_string().contains("anthropic"), "{error}");
    let error = authorizer
        .sign_out("anthropic")
        .await
        .expect_err("nor a browser sign-out");
    assert!(error.to_string().contains("anthropic"), "{error}");
}

/// With no home there is nowhere to put a grant, and that is said rather than
/// guessed at.
#[tokio::test]
async fn no_home_is_reported_before_a_browser_opens() {
    let authorizer = LiveAuthorizer {
        opener: Arc::new(|_: &str| panic!("no browser may open")),
        store_path: None,
        credential_store: Ok(None),
        client: reqwest::Client::new(),
        issuer: "http://127.0.0.1:1".to_string(),
        ports: vec![0],
    };
    let announce: codex_login::Announce = Arc::new(|_: &str| {});
    let error = authorizer
        .sign_in("codex", announce)
        .await
        .expect_err("nowhere to write");
    assert!(error.to_string().contains("LEVIATH_HOME"), "{error}");
    let error = authorizer
        .sign_out("codex")
        .await
        .expect_err("nowhere to read");
    assert!(error.to_string().contains("LEVIATH_HOME"), "{error}");
}

/// A credential store that could not be opened stops the sign-in instead of
/// quietly writing the grant to a file the user asked not to use.
#[tokio::test]
async fn an_unreachable_keychain_stops_the_sign_in() {
    let authorizer = LiveAuthorizer {
        opener: Arc::new(|_: &str| panic!("no browser may open")),
        store_path: Some(PathBuf::from("/nowhere/provider-auth.json")),
        credential_store: Err("no keychain on this machine".to_string()),
        client: reqwest::Client::new(),
        issuer: "http://127.0.0.1:1".to_string(),
        ports: vec![0],
    };
    let announce: codex_login::Announce = Arc::new(|_: &str| {});
    let error = authorizer
        .sign_in("codex", announce)
        .await
        .expect_err("the keychain is the store");
    assert!(error.to_string().contains("no keychain"), "{error}");
    let error = authorizer
        .sign_out("codex")
        .await
        .expect_err("and the sign-out reads it too");
    assert!(error.to_string().contains("no keychain"), "{error}");
}

/// The live authorizer, end to end: a local issuer, a stub browser that plays
/// the redirect, and a temp file for the grant.
///
/// This is the path the wizard actually takes, so it is worth driving rather
/// than only its refusals. Port zero because the two registered ports are the
/// production value and a test that bound them would fight whatever the
/// developer has signed in.
#[tokio::test]
async fn a_whole_sign_in_through_the_live_authorizer_reports_the_account() {
    use crate::commands::auth::codex::tests::{browser_that_redirects, id_token};

    let body = serde_json::json!({
        "access_token": "at-1",
        "refresh_token": "rt-1",
        "id_token": id_token(),
        "expires_in": 3600,
    });
    let issuer = leviath_testkit::spawn_mock_server(200, "OK", body.to_string().into_bytes()).await;
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("provider-auth.json");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let authorizer = LiveAuthorizer {
        opener: browser_that_redirects(Arc::clone(&seen)),
        store_path: Some(store_path.clone()),
        credential_store: Ok(None),
        client: reqwest::Client::new(),
        issuer,
        ports: vec![0],
    };

    let announced = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&announced);
    let announce: codex_login::Announce = Arc::new(move |url: &str| {
        sink.lock().expect("sink").push(url.to_string());
    });

    let who = authorizer
        .sign_in("codex", announce)
        .await
        .expect("the whole round trip");

    assert_eq!(who, "someone@example.com (plus plan)");
    assert_eq!(
        announced.lock().expect("sink").len(),
        1,
        "the URL is announced once, before the browser is asked"
    );
    // And the grant really landed, so the sign-out below has something to
    // remove rather than reporting success over an empty store.
    let stored = leviath_providers::codex::ProviderAuthStore::load(&store_path)
        .expect("the store reads back");
    assert!(stored.get("codex").is_some());

    authorizer.sign_out("codex").await.expect("signs out");
    let stored = leviath_providers::codex::ProviderAuthStore::load(&store_path)
        .expect("the store still reads back");
    assert!(
        stored.get("codex").is_none(),
        "the grant outlived the sign-out"
    );
}

/// A sign-in that gets as far as the issuer and is refused there reports that,
/// rather than the wizard sitting on "waiting for your browser" for ever.
#[tokio::test]
async fn an_issuer_that_refuses_the_exchange_fails_the_sign_in() {
    use crate::commands::auth::codex::tests::browser_that_redirects;

    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let authorizer = LiveAuthorizer {
        opener: browser_that_redirects(Arc::clone(&seen)),
        store_path: Some(dir.path().join("provider-auth.json")),
        credential_store: Ok(None),
        client: reqwest::Client::new(),
        // Nothing listens on port 1, so the code exchange cannot succeed.
        issuer: "http://127.0.0.1:1".to_string(),
        ports: vec![0],
    };
    let announce: codex_login::Announce = Arc::new(|_: &str| {});

    let error = authorizer
        .sign_in("codex", announce)
        .await
        .expect_err("the issuer never answered");
    assert!(!error.to_string().is_empty());
}

/// A grant file that will not parse stops the sign-out rather than reporting
/// that there was nothing to remove.
#[tokio::test]
async fn a_corrupt_grant_file_fails_the_sign_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("provider-auth.json");
    std::fs::write(&path, "{ not json").unwrap();
    let authorizer = LiveAuthorizer {
        opener: Arc::new(|_: &str| panic!("no browser may open")),
        store_path: Some(path),
        credential_store: Ok(None),
        client: reqwest::Client::new(),
        issuer: "http://127.0.0.1:1".to_string(),
        ports: vec![0],
    };
    assert!(authorizer.sign_out("codex").await.is_err());
}

/// The real authorizer resolves this machine rather than being handed paths.
#[test]
fn the_real_authorizer_points_at_this_machine() {
    let dir = tempfile::tempdir().unwrap();
    let authorizer =
        LiveAuthorizer::real(Arc::new(|_: &str| true), &dir.path().join("config.toml"));
    assert!(
        authorizer.store_path.is_some(),
        "a home resolves in a test run"
    );
    // A config that does not exist is the default one, whose backend is the
    // file store: resolving it must not fail.
    assert!(authorizer.credential_store.is_ok());
    assert_eq!(authorizer.ports, leviath_providers::codex::CALLBACK_PORTS);
}
