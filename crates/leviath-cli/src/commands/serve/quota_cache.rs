//! The subscription usage `GET /api/providers?quota=true` answers from.
//!
//! Reading a subscription's quota means asking the account over the network,
//! which is why the parameter is opt-in. But there was nothing to read a
//! recent answer from, so every page open, reload and reconnect paid the whole
//! round trip again - and a console with nowhere to put the cost keeps the
//! request off any path that matters, which is a feature silently unused.
//!
//! So a reading is kept, the same way the model catalogue keeps a listing: the
//! accounts are asked once, the answer is served from memory for a minute
//! (fifteen seconds when an account could not be read, because that gap is
//! likely a blip), and a request past the window gets the reading in hand with
//! the next one starting behind it. `?refresh=1` asks again and waits, for the
//! "check again" button. The keeping itself is
//! [`Refreshing`](super::refreshing).
//!
//! ## What a reading is of
//!
//! Not of the config alone. A quota reading is of the *accounts*, and which
//! accounts there are lives in the grant file the sign-in routes write, which
//! nothing in the config reflects. So the key is the config plus the enabled,
//! signed-in providers and the account each one names - which means a sign-in
//! and a sign-out both rotate the key by construction, and neither `login` nor
//! `logout` has to reach in here and invalidate anything.
//!
//! What is deliberately *not* in the key is the grant file's mtime and the
//! token expiry. Both move on every background token refresh, including the
//! ones a quota read itself triggers, so a key carrying them would invalidate
//! itself on nearly every use. The gap that leaves is narrow: signing out and
//! back in, inside a minute, as a different account whose grant names no
//! address. `?refresh=1` answers it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderName;
use leviath_runtime::ProviderRegistry;
use leviath_runtime::provider_creds::ProviderCreds;

use super::refreshing::{Build, Cached, Freshness, Refreshing, SameAnswer};
use crate::commands::providers::quota::{self, Usage};
use crate::config::Config;

/// How long a complete reading is served without asking the accounts again.
/// Short, because quota moves as the user runs agents, unlike a model listing.
const FRESH_FOR: Duration = Duration::from_secs(60);
/// How long a reading missing an account's answer is served before another try.
const RETRY_AFTER: Duration = Duration::from_secs(15);
/// How long each account gets to answer. Shorter than the ten seconds
/// `lev providers quota` allows, for the reason
/// [`PROVIDER_TIMEOUT`](super::model_catalog::PROVIDER_TIMEOUT) is five: this
/// bounds a page load, not a person at a terminal.
pub(super) const ACCOUNT_TIMEOUT: Duration = Duration::from_secs(5);

/// Response header: seconds since the accounts were read.
pub(super) const QUOTA_AGE: HeaderName = HeaderName::from_static("x-leviath-quota-age");
/// Response header: whether every account answered when they were.
pub(super) const QUOTA_COMPLETE: HeaderName = HeaderName::from_static("x-leviath-quota-complete");

/// One account a reading covers.
///
/// The address is here and not only the id: signing out and back in as
/// somebody else is a different reading of the same provider.
#[derive(Clone, PartialEq)]
pub(super) struct Asked {
    /// The provider's registry name.
    pub(super) id: String,
    /// The account the grant names, when it names one.
    pub(super) account: Option<String>,
}

/// One reading's inputs, and the key it is kept under.
#[derive(Clone)]
pub(super) struct Accounts {
    config: Arc<Config>,
    /// Where the sign-in routes wrote the grants.
    ///
    /// Resolved by the handler and carried here rather than looked up when the
    /// reading is taken, because a reading runs in a detached task and a
    /// detached task does not inherit the task-local the tests scope
    /// [`admin_paths`](super::mcp::admin_paths) through - so a lookup in there
    /// would read the real home and pass without proving anything.
    ///
    /// This is not the file location [`ProviderAdmin`] refuses to hold: that
    /// rule is about a path a *request* can influence, and nothing here comes
    /// from the request.
    ///
    /// [`ProviderAdmin`]: super::providers::ProviderAdmin
    grants: PathBuf,
    /// The enabled, signed-in providers, in catalog order.
    asked: Vec<Asked>,
}

impl Accounts {
    pub(super) fn new(config: Arc<Config>, grants: PathBuf, asked: Vec<Asked>) -> Self {
        Self {
            config,
            grants,
            asked,
        }
    }

    /// The accounts this reading is of, so a test's stand-in builder can make
    /// clients for exactly them the way [`live_registry`] does.
    #[cfg(test)]
    pub(super) fn asked(&self) -> &[Asked] {
        &self.asked
    }
}

impl SameAnswer for Accounts {
    /// The config by identity, the accounts by value, and the grant location
    /// not at all: that is where a reading is taken from, not what it is of,
    /// and it is one path on a running daemon.
    fn same_answer(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.config, &other.config) && self.asked == other.asked
    }
}

/// One reading of every signed-in account, as of one moment.
pub(super) type Reading = Cached<Accounts, Vec<Usage>>;

/// How the clients a reading asks are built. Injectable so a test can hand in
/// subscriptions that answer, fail or hang without a network. `None` is a
/// registry that could not be built at all, which is not the same as one with
/// nothing to say.
type RegistryBuilder = Arc<dyn Fn(&Accounts) -> Option<ProviderRegistry> + Send + Sync>;

/// The readings, shared by every handler through `AppState`.
#[derive(Clone)]
pub(super) struct QuotaCache {
    readings: Refreshing<Accounts, Vec<Usage>>,
}

impl Default for QuotaCache {
    fn default() -> Self {
        Self::with_builder(Arc::new(live_registry))
    }
}

impl QuotaCache {
    pub(super) fn with_builder(build_registry: RegistryBuilder) -> Self {
        Self::with_timings(
            build_registry,
            FRESH_FOR,
            RETRY_AFTER,
            ACCOUNT_TIMEOUT,
            ACCOUNT_TIMEOUT + Duration::from_secs(1),
        )
    }

    fn with_timings(
        build_registry: RegistryBuilder,
        fresh_for: Duration,
        retry_after: Duration,
        account_timeout: Duration,
        cold_wait: Duration,
    ) -> Self {
        // `_force` is ignored: there is nothing a second read would skip. A
        // forced read is already a read of the accounts; the flag only means
        // the caller wants to wait for it rather than be handed the last one.
        let build: Build<Accounts, Vec<Usage>> = Arc::new(move |accounts, _force| {
            let build_registry = Arc::clone(&build_registry);
            Box::pin(async move {
                let Some(registry) = build_registry(&accounts) else {
                    // No clients means no reading, which is not the same as
                    // every account having nothing to say - hence `false`.
                    return (Vec::new(), false);
                };
                let usage = quota::usage_within(&accounts.config, &registry, account_timeout).await;
                // Complete means every account that was asked produced a
                // report. One that answered "nothing to say" answered, and is
                // simply absent from the reading.
                let complete = usage.iter().all(|u| u.report.is_ok());
                (usage, complete)
            })
        });
        Self {
            readings: Refreshing::new(build, fresh_for, retry_after, cold_wait),
        }
    }

    /// The reading for `accounts`, and how it was got.
    ///
    /// `force` asks the accounts again and waits for them, for the console's
    /// "check again".
    pub(super) async fn report(
        &self,
        accounts: Accounts,
        force: bool,
    ) -> (Arc<Reading>, Freshness) {
        self.readings.get(accounts, force).await
    }

    /// The latest reading as it lands, for a caller that wants to be told.
    #[cfg(test)]
    pub(super) fn subscribe(&self) -> tokio::sync::watch::Receiver<Option<Arc<Reading>>> {
        self.readings.subscribe()
    }
}

/// The clients a live reading asks: one per signed-in account, built the way a
/// run builds one but over the grant file the sign-in routes wrote.
fn live_registry(accounts: &Accounts) -> Option<ProviderRegistry> {
    let creds: Vec<ProviderCreds> = accounts
        .asked
        .iter()
        .map(|account| {
            let mut options =
                crate::commands::run::session::signin_options(&accounts.config, &account.id);
            options.insert(
                "auth_store_path".to_string(),
                accounts.grants.display().to_string(),
            );
            ProviderCreds {
                name: account.id.clone(),
                api_key: None,
                base_url: None,
                model_capabilities: HashMap::new(),
                request_timeout_secs: Some(20),
                rate_limit: None,
                options,
            }
        })
        .collect();
    leviath_runtime::provider_creds::build_provider_registry(&creds).ok()
}

#[cfg(test)]
#[path = "quota_cache_tests.rs"]
mod tests;
