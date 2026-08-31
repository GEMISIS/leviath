//! Holding a bearer token that expires, and replacing it exactly once.
//!
//! The ChatGPT grant rotates: every refresh mints a new refresh token and
//! invalidates the one presented. Presenting a spent refresh token is not a
//! retryable error, it is the end of the grant, and the user has to sign in
//! through a browser again. That single fact shapes everything here.
//!
//! Leviath runs up to eight inferences per model concurrently. When a token
//! lapses they do not fail one at a time, they fail together, so a naive
//! "notice the 401, refresh, retry" would fire eight refreshes against one
//! grant and poison it. Guarding with a plain mutex is not enough either: eight
//! tasks queue on the lock and each still concludes, correctly at the time it
//! checked, that a refresh is needed.
//!
//! So [`TokenSource::refresh_stale`] takes *the token that failed*. Under the
//! lock it compares that against what is cached now, and a caller whose stale
//! token has already been replaced gets the replacement instead of a second
//! network round trip. Proactive refresh routes through the same call with the
//! currently-cached token, so early and reactive refresh cannot race each other
//! either.
//!
//! Across processes the same shape is repeated with a lock file and a re-read
//! from disk, because a daemon and an ad-hoc `lev run` hold separate caches
//! over one grant file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use super::store::{ProviderAuthStore, ProviderGrant};

/// What a request needs in order to authenticate.
///
/// `Debug` is hand-written so the token cannot be printed.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    /// The bearer token for this request.
    pub access_token: String,
    /// The workspace to act in, sent as `ChatGPT-Account-Id`.
    pub account_id: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_token", &"<redacted>")
            .field("account_id", &self.account_id)
            .finish()
    }
}

/// Why a refresh failed, and whether anything is worth trying afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshError {
    /// The grant is gone: the refresh token was expired, revoked, or already
    /// spent. Nothing recovers this except signing in again, so the caller must
    /// not retry and must not fail over hoping for better luck.
    Terminal(String),
    /// The refresh itself did not complete: the network, a 5xx, a timeout. The
    /// grant is presumably still good.
    Transient(String),
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Terminal(m) | Self::Transient(m) => f.write_str(m),
        }
    }
}

impl RefreshError {
    /// Whether the grant is unrecoverable.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal(_))
    }
}

/// A fresh set of tokens from the issuer.
#[derive(Debug, Clone, Default)]
pub struct RefreshedTokens {
    /// The new bearer.
    pub access_token: String,
    /// The new refresh token. The issuer rotates, but treats a response that
    /// omits one as "keep the old", so this is optional.
    pub refresh_token: Option<String>,
    /// A re-issued id token, when one came back.
    pub id_token: Option<String>,
}

/// How the refresh request actually reaches the issuer.
///
/// A trait so the single-flight logic above can be tested exhaustively without
/// a socket, which is the only way to prove the eight-concurrent-callers case.
#[async_trait]
pub trait RefreshTransport: Send + Sync {
    /// Exchange `refresh_token` for a new set.
    async fn refresh(&self, refresh_token: &str) -> Result<RefreshedTokens, RefreshError>;
}

/// Where credentials come from, and how they get replaced.
#[async_trait]
pub trait TokenSource: Send + Sync {
    /// The credentials to use for the next request, refreshing first if the
    /// token is inside its expiry margin.
    async fn credentials(&self) -> Result<Credentials, RefreshError>;

    /// Replace `stale` and return what to use instead.
    ///
    /// `stale` is the token whose request came back 401. Passing it is what
    /// makes this safe to call from every concurrent request at once: a caller
    /// whose token has already been replaced is handed the replacement rather
    /// than triggering a second rotation.
    async fn refresh_stale(&self, stale: &str) -> Result<Credentials, RefreshError>;

    /// The grant's account facts, for display and for gating the catalog.
    /// `None` when nobody has signed in.
    fn grant(&self) -> Option<ProviderGrant>;
}

/// Reads the wall clock. Injected so expiry is testable without waiting.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The system clock, in Unix seconds.
pub fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    })
}

/// How long a lock file may sit before it is treated as abandoned.
///
/// A refresh that has not finished in half a minute has failed; leaving the
/// lock would wedge every later refresh in every process.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(30);

/// Holds the cross-process refresh lock for as long as it is alive.
///
/// The lock is a file created with `create_new`, which is atomic and behaves
/// identically on Unix and Windows. `flock` would be the more conventional
/// choice and would force a `#[cfg(unix)]` path with no Windows twin.
struct LockFile {
    path: PathBuf,
}

impl LockFile {
    /// Take the lock, breaking an abandoned one.
    ///
    /// Failure to take it is not fatal. The lock narrows a cross-process race;
    /// the compare-and-swap against the on-disk token is what actually prevents
    /// a double rotation, and that runs either way.
    fn acquire(path: PathBuf) -> Option<Self> {
        if let Some(lock) = Self::create(&path) {
            return Some(lock);
        }
        // Somebody holds it. If they abandoned it, break it and try once more;
        // a second failure means another process won the race, which is fine.
        match Self::break_if_stale(&path) {
            true => Self::create(&path),
            false => None,
        }
    }

    /// Create the lock file, or `None` if it is already there.
    ///
    /// Any other IO error is `None` too: both mean the lock was not taken, and
    /// the caller treats them the same.
    fn create(path: &Path) -> Option<Self> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .ok()
            .map(|_| Self {
                path: path.to_path_buf(),
            })
    }

    /// Remove the lock if it is older than [`LOCK_STALE_AFTER`], reporting
    /// whether it is worth trying to take it again.
    fn break_if_stale(path: &Path) -> bool {
        let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
            return false;
        };
        let Ok(age) = SystemTime::now().duration_since(modified) else {
            return false;
        };
        age > LOCK_STALE_AFTER && std::fs::remove_file(path).is_ok()
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        // Best effort: a lock left behind by a crash is broken by age above.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The real token source: a cached grant, a rotating refresh, and a file.
pub struct CodexTokenSource {
    /// The grant as this process last saw it.
    ///
    /// Taken through [`leviath_core::sync::lock`], whose docs explain why
    /// recovering from poisoning is sound: the sections below clone an
    /// `Option` and nothing else.
    cached: Mutex<Option<ProviderGrant>>,
    /// Serialises refresh within this process. A tokio mutex because it is held
    /// across the refresh await.
    gate: tokio::sync::Mutex<()>,
    /// Set once the grant is known to be unrecoverable, so later calls fail
    /// fast instead of hammering the issuer with a dead refresh token.
    poisoned: AtomicBool,
    /// Where the grant file lives.
    store_path: PathBuf,
    /// The OS credential store, when configured.
    credential_store: Option<Arc<dyn leviath_core::CredentialStore>>,
    /// How the refresh reaches the issuer.
    transport: Arc<dyn RefreshTransport>,
    /// Reads the wall clock.
    clock: Clock,
    /// Which provider's grant this is, for the store key and error text.
    provider: String,
}

impl CodexTokenSource {
    /// Build a source over the grant file at `store_path`.
    pub fn new(store_path: PathBuf, transport: Arc<dyn RefreshTransport>) -> Self {
        Self {
            cached: Mutex::new(None),
            gate: tokio::sync::Mutex::new(()),
            poisoned: AtomicBool::new(false),
            store_path,
            credential_store: None,
            transport,
            clock: system_clock(),
            provider: super::PROVIDER_NAME.to_string(),
        }
    }

    /// Read and write the grant through the OS credential store.
    #[must_use]
    pub fn with_credential_store(
        mut self,
        store: Option<Arc<dyn leviath_core::CredentialStore>>,
    ) -> Self {
        self.credential_store = store;
        self
    }

    /// Override the clock. Tests drive expiry with this.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Name this source's provider, for the grant key and error text.
    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    /// The lock file guarding cross-process refresh of this grant.
    fn lock_path(&self) -> PathBuf {
        let mut path = self.store_path.clone().into_os_string();
        path.push(".lock");
        PathBuf::from(path)
    }

    /// Every provider's grant, or `None` when the store cannot be read.
    ///
    /// The whole store rather than just this provider's grant, because a write
    /// has to put the others back untouched. Reading once and handing the store
    /// to [`Self::persist`] is also what closes the window a second read would
    /// open between them.
    fn load_store(&self) -> Option<ProviderAuthStore> {
        let store = self.credential_store.as_deref();
        match ProviderAuthStore::load_with(&self.store_path, store) {
            Ok(all) => Some(all),
            Err(e) => {
                tracing::warn!("could not read the provider auth store: {e}");
                None
            }
        }
    }

    /// Load this provider's grant from wherever it lives.
    fn load(&self) -> Option<ProviderGrant> {
        self.load_store()?.get(&self.provider).cloned()
    }

    /// Write `grant` into `all` and save it, preserving every other provider's.
    fn persist(
        &self,
        all: &mut ProviderAuthStore,
        grant: &ProviderGrant,
    ) -> Result<(), RefreshError> {
        all.set(&self.provider, grant.clone());
        all.save_with(&self.store_path, self.credential_store.as_deref())
            .map_err(|e| {
                RefreshError::Transient(format!("could not write the provider auth store: {e}"))
            })
    }

    /// The cached grant, loading it from disk on first use.
    fn current(&self) -> Option<ProviderGrant> {
        if let Some(grant) = leviath_core::sync::lock(&self.cached).clone() {
            return Some(grant);
        }
        let loaded = self.load()?;
        *leviath_core::sync::lock(&self.cached) = Some(loaded.clone());
        Some(loaded)
    }

    /// The error a caller sees when nobody has signed in.
    fn not_signed_in(&self) -> RefreshError {
        RefreshError::Terminal(format!(
            "no {} credentials are stored; run `lev auth login {}` to sign in",
            self.provider, self.provider
        ))
    }
}

/// Turn a grant into the credentials a request carries.
fn credentials_of(grant: &ProviderGrant) -> Credentials {
    Credentials {
        access_token: grant.access_token.clone(),
        account_id: grant
            .account_id
            .clone()
            .or_else(|| grant.claims().account_id),
    }
}

#[async_trait]
impl TokenSource for CodexTokenSource {
    async fn credentials(&self) -> Result<Credentials, RefreshError> {
        let grant = self.current().ok_or_else(|| self.not_signed_in())?;
        if grant.is_expired_at((self.clock)()) {
            // Proactive and reactive refresh share one path, so the two can
            // never both decide to rotate the same token.
            return self.refresh_stale(&grant.access_token.clone()).await;
        }
        Ok(credentials_of(&grant))
    }

    async fn refresh_stale(&self, stale: &str) -> Result<Credentials, RefreshError> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(RefreshError::Terminal(format!(
                "the {} session was rejected and cannot be recovered; \
                 run `lev auth login {}` to sign in again",
                self.provider, self.provider
            )));
        }

        let _guard = self.gate.lock().await;

        // Somebody in this process may have won the race while we waited.
        if let Some(grant) = leviath_core::sync::lock(&self.cached).clone()
            && grant.access_token != stale
        {
            return Ok(credentials_of(&grant));
        }

        // Another process may have rotated it. The lock narrows the window;
        // this re-read is what actually decides.
        let _lock = LockFile::acquire(self.lock_path());
        let mut all = self.load_store().ok_or_else(|| self.not_signed_in())?;
        let on_disk = all
            .get(&self.provider)
            .cloned()
            .ok_or_else(|| self.not_signed_in())?;
        if on_disk.access_token != stale {
            *leviath_core::sync::lock(&self.cached) = Some(on_disk.clone());
            return Ok(credentials_of(&on_disk));
        }

        let refreshed = match self.transport.refresh(&on_disk.refresh_token).await {
            Ok(tokens) => tokens,
            Err(e) => {
                if e.is_terminal() {
                    // Remember it: without this, every one of the eight waiting
                    // callers presents the same dead refresh token in turn.
                    self.poisoned.store(true, Ordering::Release);
                }
                return Err(e);
            }
        };

        let mut next = on_disk;
        next.access_token = refreshed.access_token;
        if let Some(token) = refreshed.refresh_token {
            next.refresh_token = token;
        }
        if let Some(id_token) = refreshed.id_token {
            next.id_token = id_token;
        }
        let claims = next.claims();
        if claims.account_id.is_some() {
            next.account_id = claims.account_id.clone();
        }
        if claims.plan_type.is_some() {
            next.plan_type = claims.plan_type.clone();
        }

        // Persist before publishing. If the process dies in between, the
        // rotated refresh token is already on disk; the other order would leave
        // a token the issuer has invalidated as the only one we still know.
        self.persist(&mut all, &next)?;
        *leviath_core::sync::lock(&self.cached) = Some(next.clone());
        Ok(credentials_of(&next))
    }

    fn grant(&self) -> Option<ProviderGrant> {
        self.current()
    }
}

#[cfg(test)]
mod tests;
