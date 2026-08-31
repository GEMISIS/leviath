//! The sign-in flow, driven end to end without a browser.
//!
//! The opener is a stub that plays the browser's part: it takes the authorize
//! URL, checks it, and issues the redirect itself. That is what makes the whole
//! round trip testable, callback and code exchange included, with nothing
//! launching and nothing reaching the real issuer.

use super::*;
use leviath_core::CredentialStore as _;
use leviath_testkit::spawn_mock_server;
use std::sync::Mutex;

/// A stub opener that plays the browser: it parses the authorize URL and hits
/// the loopback callback with the code and the state it was given.
fn browser_that_redirects(recorder: Arc<Mutex<Vec<String>>>) -> leviath_mcp::BrowserOpener {
    Arc::new(move |url: &str| {
        recorder.lock().expect("recorder").push(url.to_string());
        let parsed = url::Url::parse(url).expect("a URL");
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        let redirect = pairs.get("redirect_uri").expect("a redirect").clone();
        let state = pairs.get("state").expect("a state").clone();
        // `127.0.0.1`, not the `localhost` in the redirect URI. The listener
        // binds the v4 loopback (as the Codex CLI does), and `localhost`
        // resolves to `::1` first on some machines, which would connect to
        // nothing.
        let port = redirect
            .rsplit(':')
            .next()
            .and_then(|rest| rest.split('/').next())
            .and_then(|p| p.parse::<u16>().ok())
            .expect("a port in the redirect URI");
        // On the runtime rather than an OS thread. `#[tokio::test]` gives a
        // current-thread runtime, and a blocking thread that the executor
        // cannot interleave with the accept loop starves under test
        // parallelism: these passed alone and timed out together.
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            if let Ok(mut socket) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                let request = format!(
                    "GET {}?code=the-code&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n",
                    codex::CALLBACK_PATH
                );
                let _ = socket.write_all(request.as_bytes()).await;
                let _ = socket.flush().await;
                // Held open until the server answers and drops its end.
                let mut sink = Vec::new();
                use tokio::io::AsyncReadExt as _;
                let _ = socket.read_to_end(&mut sink).await;
            }
        });
        true
    })
}

/// A browser that does nothing, for the paths that never get that far.
fn browser_that_does_nothing() -> leviath_mcp::BrowserOpener {
    Arc::new(|_: &str| false)
}

/// A signed-in-looking id token.
///
/// Encoded by hand rather than through a crate: `base64` is not a dependency
/// of the CLI, and adding one for a single test fixture is a poor trade.
fn id_token() -> String {
    let claims = serde_json::json!({
        "email": "someone@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "acct-1",
            "chatgpt_plan_type": "plus",
        },
    })
    .to_string();
    format!("aGVhZGVy.{}.c2ln", base64url(claims.as_bytes()))
}

/// Base64url, no padding. The alphabet is the whole of it.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut buf = [0u8; 3];
        buf[..chunk.len()].copy_from_slice(chunk);
        let n = u32::from(buf[0]) << 16 | u32::from(buf[1]) << 8 | u32::from(buf[2]);
        let symbols = chunk.len() + 1;
        for i in 0..symbols {
            let shift = 18 - 6 * i;
            out.push(ALPHABET[((n >> shift) & 0x3f) as usize] as char);
        }
    }
    out
}

fn env_for(issuer: &str, dir: &tempfile::TempDir, opener: leviath_mcp::BrowserOpener) -> LoginEnv {
    LoginEnv {
        opener,
        store_path: dir.path().join("provider-auth.json"),
        credential_store: None,
        client: reqwest::Client::new(),
        issuer: issuer.to_string(),
        announce: Arc::new(|_| {}),
        // Port zero: the registered ports are the production value, and a test
        // that bound them would fight the developer's own Codex CLI.
        ports: vec![0],
    }
}

#[tokio::test]
async fn a_whole_sign_in_stores_a_usable_grant() {
    let body = serde_json::json!({
        "access_token": "at-1",
        "refresh_token": "rt-1",
        "id_token": id_token(),
    });
    let issuer = spawn_mock_server(200, "OK", body.to_string().into_bytes()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let seen = Arc::new(Mutex::new(Vec::new()));

    let grant = login(&env_for(
        &issuer,
        &dir,
        browser_that_redirects(Arc::clone(&seen)),
    ))
    .await
    .expect("sign-in");

    assert_eq!(grant.access_token, "at-1");
    assert_eq!(grant.refresh_token, "rt-1");
    assert_eq!(grant.plan_type.as_deref(), Some("plus"));
    assert_eq!(grant.account_id.as_deref(), Some("acct-1"));
    assert_eq!(grant.email.as_deref(), Some("someone@example.com"));

    // And it is on disk, where the daemon will look for it.
    let stored = ProviderAuthStore::load(&dir.path().join("provider-auth.json"))
        .expect("load")
        .get("codex")
        .cloned()
        .expect("a stored grant");
    assert_eq!(stored.refresh_token, "rt-1");
}

#[tokio::test]
async fn the_authorize_url_carries_what_the_issuer_needs() {
    let body = serde_json::json!({ "access_token": "at-1", "refresh_token": "rt-1" });
    let issuer = spawn_mock_server(200, "OK", body.to_string().into_bytes()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let seen = Arc::new(Mutex::new(Vec::new()));

    login(&env_for(
        &issuer,
        &dir,
        browser_that_redirects(Arc::clone(&seen)),
    ))
    .await
    .expect("sign-in");

    let url = seen.lock().expect("recorder")[0].clone();
    let parsed = url::Url::parse(&url).expect("a URL");
    let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
    assert_eq!(pairs["response_type"], "code");
    assert_eq!(pairs["client_id"], codex::CLIENT_ID);
    assert_eq!(pairs["code_challenge_method"], "S256");
    assert_eq!(pairs["scope"], codex::SCOPE);
    // The workspace list is what carries the account id the route wants.
    assert_eq!(pairs["id_token_add_organizations"], "true");
    assert!(
        pairs["code_challenge"].len() >= 43,
        "PKCE challenge too short"
    );
    // `localhost`, not `127.0.0.1`: the registered redirect is the literal
    // string and the issuer compares it as one.
    assert!(
        pairs["redirect_uri"].starts_with("http://localhost:"),
        "got {}",
        pairs["redirect_uri"]
    );
    assert!(pairs["redirect_uri"].ends_with("/auth/callback"));
}

#[tokio::test]
async fn the_url_is_announced_before_the_browser_is_asked() {
    // A headless or SSH session has to have something to copy even when the
    // opener does nothing at all.
    let body = serde_json::json!({ "access_token": "at-1", "refresh_token": "rt-1" });
    let issuer = spawn_mock_server(200, "OK", body.to_string().into_bytes()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let announced: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&announced);

    let mut env = env_for(
        &issuer,
        &dir,
        browser_that_redirects(Arc::new(Mutex::new(Vec::new()))),
    );
    env.announce = Arc::new(move |url: &str| sink.lock().expect("sink").push(url.to_string()));
    login(&env).await.expect("sign-in");

    let announced = announced.lock().expect("announced").clone();
    assert_eq!(announced.len(), 1);
    assert!(
        announced[0].contains("/oauth/authorize?"),
        "got {}",
        announced[0]
    );
}

#[tokio::test]
async fn a_refused_exchange_is_reported_with_the_status() {
    let issuer = spawn_mock_server(
        400,
        "Bad Request",
        b"{\"error\":\"invalid_grant\"}".to_vec(),
    )
    .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let err = login(&env_for(
        &issuer,
        &dir,
        browser_that_redirects(Arc::new(Mutex::new(Vec::new()))),
    ))
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("400"), "got {err}");
    assert!(err.contains("invalid_grant"), "got {err}");
}

#[tokio::test]
async fn a_reply_with_no_access_token_is_refused() {
    let issuer = spawn_mock_server(200, "OK", b"{\"id_token\":\"only-this\"}".to_vec()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let err = login(&env_for(
        &issuer,
        &dir,
        browser_that_redirects(Arc::new(Mutex::new(Vec::new()))),
    ))
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("no access token"), "got {err}");
}

#[tokio::test]
async fn a_reply_that_is_not_json_is_refused() {
    let issuer = spawn_mock_server(200, "OK", b"not json".to_vec()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let err = login(&env_for(
        &issuer,
        &dir,
        browser_that_redirects(Arc::new(Mutex::new(Vec::new()))),
    ))
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("not JSON"), "got {err}");
}

#[tokio::test]
async fn an_unreachable_issuer_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = login(&env_for(
        "http://127.0.0.1:1",
        &dir,
        browser_that_redirects(Arc::new(Mutex::new(Vec::new()))),
    ))
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("could not reach"), "got {err}");
}

#[tokio::test]
async fn a_callback_that_never_arrives_times_out_rather_than_hanging() {
    // The opener does nothing, so nothing ever redirects. A very short list of
    // ports and the real timeout would make this a five-minute test, so the
    // wait is driven through `wait_for_callback`'s own deadline elsewhere;
    // here the point is that the flow does not panic when nobody answers.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut env = env_for("http://127.0.0.1:1", &dir, browser_that_does_nothing());
    env.ports = vec![0];
    let flow = login(&env);
    let outcome = tokio::time::timeout(Duration::from_millis(200), flow).await;
    assert!(outcome.is_err(), "the flow returned without a callback");
}

#[tokio::test]
async fn both_registered_ports_are_tried_before_giving_up() {
    // Bind one of a two-port list and check the other is taken.
    let held = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind");
    let taken = held.local_addr().expect("addr").port();
    let spare = {
        let probe = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind");
        probe.local_addr().expect("addr").port()
    };

    let (_listener, port) = bind(&[taken, spare]).await.expect("one of the two");
    assert_eq!(port, spare, "the free port was not tried");
}

#[tokio::test]
async fn neither_port_available_names_the_codex_cli() {
    // The likely cause, and not one a person would guess from `EADDRINUSE`.
    let held = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind");
    let taken = held.local_addr().expect("addr").port();
    let err = bind(&[taken]).await.unwrap_err().to_string();
    assert!(err.contains("Codex CLI"), "got {err}");
    assert!(err.contains(&taken.to_string()), "got {err}");
}

#[test]
fn signing_out_removes_the_grant_and_reports_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("provider-auth.json");
    let mut store = ProviderAuthStore::default();
    store.set(
        "codex",
        ProviderGrant {
            access_token: "at".to_string(),
            refresh_token: "rt".to_string(),
            ..Default::default()
        },
    );
    store.save(&path).expect("save");

    assert!(logout(&path, None).expect("logout"));
    assert!(
        ProviderAuthStore::load(&path)
            .expect("load")
            .get("codex")
            .is_none()
    );
    // And a second attempt says there was nothing to do.
    assert!(!logout(&path, None).expect("logout"));
}

#[tokio::test]
async fn a_sign_in_with_no_free_port_is_refused_before_anything_else() {
    let held = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind");
    let taken = held.local_addr().expect("addr").port();
    let dir = tempfile::tempdir().expect("tempdir");
    let mut env = env_for("http://127.0.0.1:1", &dir, browser_that_does_nothing());
    env.ports = vec![taken];
    let err = login(&env).await.unwrap_err().to_string();
    assert!(err.contains("Codex CLI"), "got {err}");
}

#[tokio::test]
async fn a_forged_callback_is_refused() {
    // The state is what binds the redirect to the browser session that started
    // it; a mismatch is a forged or stale callback, not a sign-in.
    let dir = tempfile::tempdir().expect("tempdir");
    let forging_browser: leviath_mcp::BrowserOpener = Arc::new(move |url: &str| {
        let parsed = url::Url::parse(url).expect("a URL");
        let pairs: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        let port: u16 = pairs["redirect_uri"]
            .rsplit(':')
            .next()
            .and_then(|rest| rest.split('/').next())
            .and_then(|p| p.parse().ok())
            .expect("a port");
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;
            use tokio::io::AsyncWriteExt as _;
            if let Ok(mut socket) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                let request = format!(
                    "GET {}?code=c&state=forged HTTP/1.1\r\n\r\n",
                    codex::CALLBACK_PATH
                );
                let _ = socket.write_all(request.as_bytes()).await;
                let mut sink = Vec::new();
                let _ = socket.read_to_end(&mut sink).await;
            }
        });
        true
    });
    let env = env_for("http://127.0.0.1:1", &dir, forging_browser);
    let err = login(&env).await.unwrap_err().to_string();
    assert!(err.contains("state mismatch"), "got {err}");
}

#[tokio::test]
async fn a_corrupt_store_fails_the_sign_in_rather_than_overwriting_it() {
    // Overwriting would drop whatever other provider's grant the file holds.
    let body = serde_json::json!({ "access_token": "at-1", "refresh_token": "rt-1" });
    let issuer = spawn_mock_server(200, "OK", body.to_string().into_bytes()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("provider-auth.json"), "{ not json").expect("write");

    let err = login(&env_for(
        &issuer,
        &dir,
        browser_that_redirects(Arc::new(Mutex::new(Vec::new()))),
    ))
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("corrupt"), "got {err}");
}

#[tokio::test]
async fn a_store_that_refuses_the_write_fails_the_sign_in() {
    let body = serde_json::json!({ "access_token": "at-1", "refresh_token": "rt-1" });
    let issuer = spawn_mock_server(200, "OK", body.to_string().into_bytes()).await;
    let dir = tempfile::tempdir().expect("tempdir");

    let mut env = env_for(
        &issuer,
        &dir,
        browser_that_redirects(Arc::new(Mutex::new(Vec::new()))),
    );
    env.credential_store = Some(Arc::new(Refusing));
    let err = login(&env).await.unwrap_err().to_string();
    assert!(err.contains("failed to store the grant"), "got {err}");
}

#[test]
fn a_store_that_refuses_the_write_fails_the_sign_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("provider-auth.json");
    let mut store = ProviderAuthStore::default();
    // Two grants, so one survives the removal and has to be written back.
    // Removing the only one leaves nothing to write and never asks the store.
    for name in ["codex", "someone-else"] {
        store.set(
            name,
            ProviderGrant {
                access_token: "at".to_string(),
                refresh_token: "rt".to_string(),
                ..Default::default()
            },
        );
    }
    store.save(&path).expect("save");
    assert!(logout(&path, Some(&Refusing)).is_err());
}

/// A credential store that answers reads and refuses writes.
struct Refusing;

impl leviath_core::CredentialStore for Refusing {
    fn get(&self, _: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn set(&self, _: &str, _: &str) -> Result<(), String> {
        Err("locked".to_string())
    }
    fn delete(&self, _: &str) -> Result<bool, String> {
        Ok(false)
    }
}

#[test]
fn signing_out_of_nothing_is_not_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(!logout(&dir.path().join("absent.json"), None).expect("logout"));
}

#[test]
fn signing_out_clears_the_os_entry_too() {
    // Otherwise the file's name index is gone while the OS store still holds a
    // grant nothing points at, and a later sign-in reads a stale one.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("provider-auth.json");
    let keychain = leviath_core::MemoryStore::default();
    let mut store = ProviderAuthStore::default();
    store.set(
        "codex",
        ProviderGrant {
            access_token: "at".to_string(),
            refresh_token: "rt".to_string(),
            ..Default::default()
        },
    );
    store.save_with(&path, Some(&keychain)).expect("save");

    assert!(logout(&path, Some(&keychain)).expect("logout"));
    assert_eq!(
        keychain
            .get(&codex::grant_account("codex"))
            .expect("a readable store"),
        None
    );
}

#[test]
fn the_production_environment_uses_the_registered_ports_and_issuer() {
    let env = LoginEnv::new(
        browser_that_does_nothing(),
        PathBuf::from("/tmp/does-not-matter"),
        None,
        reqwest::Client::new(),
        Arc::new(|_| {}),
    );
    assert_eq!(env.issuer, "https://auth.openai.com");
    assert_eq!(env.ports, vec![1455, 1457]);
}
