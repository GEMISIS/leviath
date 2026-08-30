//! The refresh path, driven without a socket.
//!
//! The single-flight test is the one that matters: rotation makes a double
//! refresh terminal, so "eight concurrent callers produce exactly one network
//! round trip" is the property the whole design exists to hold.

use super::*;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::sync::atomic::AtomicUsize;

/// A JWT-shaped access token that expires at `exp`.
fn token_expiring_at(exp: u64) -> String {
    format!(
        "aGVhZGVy.{}.c2ln",
        URL_SAFE_NO_PAD.encode(serde_json::json!({ "exp": exp }).to_string())
    )
}

fn grant(access: &str, refresh: &str) -> ProviderGrant {
    ProviderGrant {
        access_token: access.to_string(),
        refresh_token: refresh.to_string(),
        account_id: Some("acct-1".to_string()),
        ..Default::default()
    }
}

/// Writes a grant file and hands back its path plus the temp dir that owns it.
fn store_with(grant: ProviderGrant) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("provider-auth.json");
    let mut store = ProviderAuthStore::default();
    store.set(super::super::PROVIDER_NAME, grant);
    store.save(&path).expect("save");
    (dir, path)
}

/// Counts refreshes and hands back a distinct token each time.
struct Counting {
    calls: AtomicUsize,
    delay: Duration,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            delay: Duration::from_millis(0),
        })
    }

    fn slow() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            // Long enough that every concurrent caller is parked on the gate
            // before the first refresh returns. Without this the test could
            // pass by accident, one caller at a time.
            delay: Duration::from_millis(80),
        })
    }

    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl RefreshTransport for Counting {
    async fn refresh(&self, _refresh_token: &str) -> Result<RefreshedTokens, RefreshError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(RefreshedTokens {
            access_token: format!("access-{n}"),
            refresh_token: Some(format!("refresh-{n}")),
            id_token: None,
        })
    }
}

/// Always fails, with a caller-chosen verdict.
struct Failing(RefreshError);

#[async_trait]
impl RefreshTransport for Failing {
    async fn refresh(&self, _refresh_token: &str) -> Result<RefreshedTokens, RefreshError> {
        Err(self.0.clone())
    }
}

fn source(path: &Path, transport: Arc<dyn RefreshTransport>) -> CodexTokenSource {
    CodexTokenSource::new(path.to_path_buf(), transport)
}

#[tokio::test]
async fn eight_concurrent_callers_refresh_exactly_once() {
    // The whole reason refresh_stale takes the failing token. A second
    // rotation would invalidate the first one's refresh token and end the
    // grant, so anything above 1 here is a broken feature, not a slow one.
    let (_dir, path) = store_with(grant("stale", "rt-0"));
    let transport = Counting::slow();
    let src = Arc::new(source(&path, transport.clone()));

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let src = Arc::clone(&src);
        tasks.push(tokio::spawn(
            async move { src.refresh_stale("stale").await },
        ));
    }
    let mut tokens = Vec::new();
    for task in tasks {
        tokens.push(task.await.expect("task").expect("refresh"));
    }

    assert_eq!(transport.count(), 1, "refreshed more than once");
    // And every caller got the same replacement, not a queue of rotations.
    for creds in &tokens {
        assert_eq!(creds.access_token, "access-1");
    }
}

#[tokio::test]
async fn a_caller_whose_token_was_already_replaced_does_not_refresh_again() {
    let (_dir, path) = store_with(grant("stale", "rt-0"));
    let transport = Counting::new();
    let src = source(&path, transport.clone());

    let first = src.refresh_stale("stale").await.expect("first");
    assert_eq!(first.access_token, "access-1");
    assert_eq!(transport.count(), 1);

    // A straggler arrives holding the token that was already retired.
    let second = src.refresh_stale("stale").await.expect("second");
    assert_eq!(second.access_token, "access-1");
    assert_eq!(transport.count(), 1, "the straggler triggered a rotation");
}

#[tokio::test]
async fn an_expired_token_refreshes_before_the_request() {
    let (_dir, path) = store_with(grant(&token_expiring_at(1_000), "rt-0"));
    let transport = Counting::new();
    // Inside the margin: expiry is 1000, margin is 120, so 900 is due.
    let src = source(&path, transport.clone()).with_clock(Arc::new(|| 900));

    let creds = src.credentials().await.expect("credentials");
    assert_eq!(creds.access_token, "access-1");
    assert_eq!(transport.count(), 1);
}

#[tokio::test]
async fn a_token_outside_the_margin_is_used_as_is() {
    let access = token_expiring_at(1_000);
    let (_dir, path) = store_with(grant(&access, "rt-0"));
    let transport = Counting::new();
    let src = source(&path, transport.clone()).with_clock(Arc::new(|| 879));

    let creds = src.credentials().await.expect("credentials");
    assert_eq!(creds.access_token, access);
    assert_eq!(
        transport.count(),
        0,
        "refreshed a token that was still good"
    );
}

#[tokio::test]
async fn the_margin_boundary_refreshes() {
    // 1000 - 120 = 880 is the first second that counts as due.
    let (_dir, path) = store_with(grant(&token_expiring_at(1_000), "rt-0"));
    let transport = Counting::new();
    let src = source(&path, transport.clone()).with_clock(Arc::new(|| 880));
    src.credentials().await.expect("credentials");
    assert_eq!(transport.count(), 1);
}

#[tokio::test]
async fn a_rotated_refresh_token_is_persisted_before_it_is_used() {
    let (_dir, path) = store_with(grant("stale", "rt-0"));
    let src = source(&path, Counting::new());
    src.refresh_stale("stale").await.expect("refresh");

    // Read the file back rather than the cache: a crash after the network call
    // must not leave the dead refresh token as the only one on disk.
    let reloaded = ProviderAuthStore::load(&path).expect("load");
    let stored = reloaded.get(super::super::PROVIDER_NAME).expect("grant");
    assert_eq!(stored.refresh_token, "refresh-1");
    assert_eq!(stored.access_token, "access-1");
}

#[tokio::test]
async fn a_refresh_that_omits_a_new_refresh_token_keeps_the_old_one() {
    struct KeepsRefresh;
    #[async_trait]
    impl RefreshTransport for KeepsRefresh {
        async fn refresh(&self, _: &str) -> Result<RefreshedTokens, RefreshError> {
            Ok(RefreshedTokens {
                access_token: "access-new".to_string(),
                refresh_token: None,
                id_token: None,
            })
        }
    }
    let (_dir, path) = store_with(grant("stale", "rt-keep"));
    let src = source(&path, Arc::new(KeepsRefresh));
    src.refresh_stale("stale").await.expect("refresh");

    let reloaded = ProviderAuthStore::load(&path).expect("load");
    let stored = reloaded.get(super::super::PROVIDER_NAME).expect("grant");
    assert_eq!(stored.refresh_token, "rt-keep");
}

#[tokio::test]
async fn a_re_issued_id_token_updates_the_account_facts() {
    struct NewIdToken;
    #[async_trait]
    impl RefreshTransport for NewIdToken {
        async fn refresh(&self, _: &str) -> Result<RefreshedTokens, RefreshError> {
            let id = format!(
                "aGVhZGVy.{}.c2ln",
                URL_SAFE_NO_PAD.encode(
                    serde_json::json!({
                        "https://api.openai.com/auth": {
                            "chatgpt_account_id": "acct-2",
                            "chatgpt_plan_type": "pro",
                        },
                    })
                    .to_string()
                )
            );
            Ok(RefreshedTokens {
                access_token: "access-new".to_string(),
                refresh_token: None,
                id_token: Some(id),
            })
        }
    }
    let (_dir, path) = store_with(grant("stale", "rt-0"));
    let src = source(&path, Arc::new(NewIdToken));
    let creds = src.refresh_stale("stale").await.expect("refresh");

    assert_eq!(creds.account_id.as_deref(), Some("acct-2"));
    let stored = ProviderAuthStore::load(&path)
        .expect("load")
        .get(super::super::PROVIDER_NAME)
        .cloned()
        .expect("grant");
    assert_eq!(stored.plan_type.as_deref(), Some("pro"));
}

#[tokio::test]
async fn a_terminal_refusal_poisons_the_source() {
    // Eight callers must not each present the same dead refresh token.
    let (_dir, path) = store_with(grant("stale", "rt-0"));
    let transport = Arc::new(Failing(RefreshError::Terminal("reused".to_string())));
    let src = source(&path, transport);

    let first = src.refresh_stale("stale").await.unwrap_err();
    assert!(first.is_terminal());

    let second = src.refresh_stale("stale").await.unwrap_err();
    assert!(second.is_terminal());
    assert!(
        second.to_string().contains("lev auth login"),
        "got: {second}"
    );
}

#[tokio::test]
async fn a_transient_refusal_leaves_the_source_usable() {
    let (_dir, path) = store_with(grant("stale", "rt-0"));
    let src = source(
        &path,
        Arc::new(Failing(RefreshError::Transient("502".to_string()))),
    );
    let err = src.refresh_stale("stale").await.unwrap_err();
    assert!(!err.is_terminal());
    // Not poisoned: a later attempt reaches the transport again rather than
    // failing fast on a remembered verdict.
    let again = src.refresh_stale("stale").await.unwrap_err();
    assert_eq!(again, RefreshError::Transient("502".to_string()));
}

#[tokio::test]
async fn a_missing_grant_says_how_to_sign_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("provider-auth.json");
    let src = source(&path, Counting::new());

    let err = src.credentials().await.unwrap_err();
    assert!(err.is_terminal());
    assert!(
        err.to_string().contains("lev auth login codex"),
        "got: {err}"
    );

    let err = src.refresh_stale("anything").await.unwrap_err();
    assert!(
        err.to_string().contains("lev auth login codex"),
        "got: {err}"
    );
    assert!(src.grant().is_none());
}

#[tokio::test]
async fn a_grant_rotated_by_another_process_is_adopted_without_refreshing() {
    // A daemon and an ad-hoc `lev run` hold separate caches over one file.
    // The second must notice the first's rotation rather than spending its own.
    let (_dir, path) = store_with(grant("stale", "rt-0"));
    let transport = Counting::new();
    let src = source(&path, transport.clone());

    // Prime the cache, then let "another process" rotate the file underneath.
    assert_eq!(src.grant().expect("grant").access_token, "stale");
    let mut store = ProviderAuthStore::load(&path).expect("load");
    store.set(super::super::PROVIDER_NAME, grant("fresh", "rt-1"));
    store.save(&path).expect("save");

    let creds = src.refresh_stale("stale").await.expect("refresh");
    assert_eq!(creds.access_token, "fresh");
    assert_eq!(
        transport.count(),
        0,
        "rotated a token another process replaced"
    );
}

#[tokio::test]
async fn an_unwritable_store_surfaces_after_the_refresh() {
    // The token was rotated upstream, so the failure has to be reported rather
    // than swallowed: the refresh token on disk is now dead.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("provider-auth.json");
    let mut store = ProviderAuthStore::default();
    store.set(super::super::PROVIDER_NAME, grant("stale", "rt-0"));
    store.save(&path).expect("save");

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

    let src = source(&path, Counting::new()).with_credential_store(Some(Arc::new(Refusing)));
    let err = src.refresh_stale("stale").await.unwrap_err();
    assert!(!err.is_terminal());
    assert!(err.to_string().contains("could not write"), "got: {err}");
}

#[tokio::test]
async fn the_grant_is_read_through_the_credential_store_when_one_is_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("provider-auth.json");
    let keychain: Arc<dyn leviath_core::CredentialStore> =
        Arc::new(leviath_core::MemoryStore::default());
    let mut store = ProviderAuthStore::default();
    store.set(
        super::super::PROVIDER_NAME,
        grant("kept-in-keychain", "rt-0"),
    );
    store
        .save_with(&path, Some(keychain.as_ref()))
        .expect("save");

    let src = source(&path, Counting::new()).with_credential_store(Some(Arc::clone(&keychain)));
    assert_eq!(src.grant().expect("grant").access_token, "kept-in-keychain");
}

#[tokio::test]
async fn a_corrupt_store_reads_as_signed_out_rather_than_panicking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("provider-auth.json");
    std::fs::write(&path, "{ not json").expect("write");
    let src = source(&path, Counting::new());
    assert!(src.grant().is_none());
    assert!(src.credentials().await.unwrap_err().is_terminal());
}

#[tokio::test]
async fn a_source_can_be_named_for_another_provider() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("provider-auth.json");
    let mut store = ProviderAuthStore::default();
    store.set("other", grant("other-token", "rt-0"));
    store.save(&path).expect("save");

    let src = source(&path, Counting::new()).with_provider("other");
    assert_eq!(src.grant().expect("grant").access_token, "other-token");
    // And the default-named source sees nothing, so the key really is used.
    let default = source(&path, Counting::new());
    assert!(default.grant().is_none());
}

#[test]
fn credentials_debug_never_prints_the_token() {
    let creds = Credentials {
        access_token: "at-super-secret".to_string(),
        account_id: Some("acct-1".to_string()),
    };
    let rendered = format!("{creds:?}");
    assert!(!rendered.contains("super-secret"), "leaked: {rendered}");
    assert!(rendered.contains("acct-1"));
}

#[test]
fn a_refresh_error_renders_its_message_either_way() {
    assert_eq!(
        RefreshError::Terminal("gone".to_string()).to_string(),
        "gone"
    );
    assert_eq!(
        RefreshError::Transient("later".to_string()).to_string(),
        "later"
    );
    assert!(RefreshError::Terminal(String::new()).is_terminal());
    assert!(!RefreshError::Transient(String::new()).is_terminal());
}

#[test]
fn the_system_clock_reports_a_plausible_now() {
    // Sanity only: the point is that the default clock is wired at all, since
    // every other test replaces it.
    assert!((system_clock())() > 1_700_000_000);
}

#[test]
fn the_lock_is_released_when_it_is_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("grant.lock");

    let held = LockFile::acquire(path.clone()).expect("first acquire");
    assert!(path.exists());
    assert!(
        LockFile::acquire(path.clone()).is_none(),
        "took a lock somebody else holds"
    );
    drop(held);
    assert!(!path.exists(), "the lock outlived its guard");
    assert!(
        LockFile::acquire(path.clone()).is_some(),
        "could not retake"
    );
}

#[test]
fn an_abandoned_lock_is_broken_by_age() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("grant.lock");
    std::fs::write(&path, b"").expect("write");
    // Backdate it well past the staleness window.
    let old = SystemTime::now() - LOCK_STALE_AFTER - Duration::from_secs(60);
    filetime_set(&path, old);

    let taken = LockFile::acquire(path.clone());
    assert!(taken.is_some(), "a stale lock wedged every later refresh");
}

#[test]
fn a_lock_in_an_unwritable_place_is_simply_not_taken() {
    // Not fatal: the compare-and-swap against the on-disk token is what
    // actually prevents a double rotation, and it runs either way.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("no-such-dir").join("grant.lock");
    assert!(LockFile::acquire(path).is_none());
}

#[test]
fn breaking_a_lock_that_is_gone_reports_no_retry() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(!LockFile::break_if_stale(&dir.path().join("absent.lock")));
}

/// Set a file's mtime, so the staleness branch can be reached without waiting.
///
/// Written against `std` rather than pulling in a crate: `set_times` is stable
/// and portable, so this needs no `#[cfg(unix)]` twin.
fn filetime_set(path: &Path, when: SystemTime) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for touch");
    file.set_times(std::fs::FileTimes::new().set_modified(when))
        .expect("set mtime");
}
