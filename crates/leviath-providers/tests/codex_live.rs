//! Manual drift check for the `codex` catalog against the live route.
//!
//! Ignored and env-gated: it spends a little of a real ChatGPT subscription and
//! may rotate its refresh token. Run it only against a throwaway home holding a
//! *copy* of a grant:
//!
//! ```text
//! H=$(mktemp -d /tmp/pwl.XXXXXX); mkdir -p $H/.leviath
//! cp ~/.leviath/provider-auth.json $H/.leviath/
//! LEVIATH_LIVE_CODEX=1 LEVIATH_HOME=$H cargo test -p leviath-providers \
//!     --test codex_live -- --ignored --nocapture
//! ```
//!
//! If the token was refreshed, the original grant is invalid: sign in again.

use std::path::PathBuf;
use std::sync::Arc;

use leviath_providers::codex::{DEFAULT_BASE_URL, DEFAULT_ORIGINATOR, PROFILE, PROVIDER_NAME};
use leviath_providers::oauth::{HttpRefresh, OAuthTokenSource, TokenSource};

/// A Codex CLI version string for the `client_version` query parameter.
const CLIENT_VERSION: &str = "0.162.1";

/// Ids compiled into the catalog when this probe was written.
const CATALOG_IDS: &[&str] = &[
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
    "gpt-5.3-codex-spark",
];

/// Ids OpenRouter lists, plus guesses at pro variants.
const CANDIDATES: &[&str] = &[
    "gpt-6.1-sol",
    "gpt-6.1-terra",
    "gpt-6-sol",
    "gpt-6-astra",
    "gpt-6-luna",
    "gpt-6-astra-pro",
    "gpt-6.1-sol-pro",
];

fn home() -> PathBuf {
    let home = std::env::var("LEVIATH_HOME").unwrap_or_default();
    assert!(
        home.starts_with("/tmp/pwl.") || home.starts_with("/private/tmp/pwl."),
        "refusing to run: LEVIATH_HOME must be a throwaway /tmp/pwl.* directory, got {home:?}"
    );
    PathBuf::from(home)
}

#[tokio::test]
#[ignore = "spends real subscription quota; needs LEVIATH_LIVE_CODEX=1"]
async fn probe_the_live_codex_route() {
    if std::env::var("LEVIATH_LIVE_CODEX").as_deref() != Ok("1") {
        eprintln!("LEVIATH_LIVE_CODEX is not 1; skipping");
        return;
    }
    let store = home().join(".leviath").join("provider-auth.json");
    let client = reqwest::Client::new();
    let tokens = OAuthTokenSource::new(
        PROVIDER_NAME,
        store,
        Arc::new(HttpRefresh::new(client.clone(), &PROFILE)),
    );
    let grant = tokens.grant();
    println!(
        "plan_type = {:?}",
        grant
            .as_ref()
            .and_then(|g| g.plan_type.clone().or_else(|| g.claims().plan_type))
    );
    let creds = tokens.credentials().await.expect("credentials");
    let ua = format!("{DEFAULT_ORIGINATOR}/probe");
    let apply = |b: reqwest::RequestBuilder| {
        let b = b
            .header("Authorization", format!("Bearer {}", creds.access_token))
            .header("originator", DEFAULT_ORIGINATOR)
            .header("User-Agent", &ua)
            .header("OpenAI-Beta", "responses=experimental");
        match &creds.account_id {
            Some(a) => b.header("ChatGPT-Account-Id", a),
            None => b,
        }
    };

    // (a) the model listing.
    let url = format!("{DEFAULT_BASE_URL}/models?client_version={CLIENT_VERSION}");
    let resp = apply(client.get(&url).header("Accept", "application/json"))
        .send()
        .await
        .expect("models request");
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    println!("GET /models -> {status}\n{body}\n");

    // (b) one minimal request per id.
    for id in CATALOG_IDS.iter().chain(CANDIDATES) {
        let body = serde_json::json!({
            "model": id,
            "instructions": "Reply with the single word ok.",
            "input": [{"type": "message", "role": "user",
                       "content": [{"type": "input_text", "text": "hi"}]}],
            "store": false,
            "stream": true,
            "reasoning": {"effort": "low"},
        });
        let resp = apply(client.post(format!("{DEFAULT_BASE_URL}/responses")))
            .header("Accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .expect("responses request");
        let status = resp.status();
        // `max_output_tokens` is rejected on this route, so the answer is
        // read for its status only, then dropped.
        let text = if status.is_success() {
            String::new()
        } else {
            resp.text().await.unwrap_or_default()
        };
        println!("{id}: {status} {text}");
    }
}
