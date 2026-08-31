//! `lev auth` - inspect and move the secrets Leviath holds.
//!
//! Leviath keeps two kinds of long-lived secret: provider API keys and MCP OAuth
//! grants. `[security] credential_store` decides whether they live in Leviath's
//! own `0600` files or in the OS credential store; this command reports which,
//! checks that the OS store is actually reachable, and moves secrets between the
//! two.

pub mod codex;

use crate::config::Config;
use clap::{Args, Subcommand};
use leviath_core::{CredentialStore, CredentialStoreKind};

/// Arguments for `lev auth`.
#[derive(Debug, Args)]
pub struct AuthArgs {
    #[command(subcommand)]
    command: AuthCommand,
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Show which credential backend is in use and what it holds
    Status,

    /// Sign in to a provider that authenticates with a browser
    ///
    /// Opens the provider's sign-in page, waits for the redirect, and stores
    /// the grant outside `config.toml`. `codex` is the one provider that works
    /// this way today; the argument is required anyway so a second one does
    /// not change what an existing command line means.
    Login {
        /// Which provider to sign in to.
        provider: String,
    },

    /// Forget a provider's stored sign-in
    ///
    /// Leaves `config.toml` alone: signing out is not the same as disabling
    /// the provider, and doing both would surprise anyone meaning to sign back
    /// in.
    Logout {
        /// Which provider to sign out of.
        provider: String,
    },

    /// Move stored secrets into the OS credential store
    ///
    /// Reads the keys currently in `~/.leviath/config.toml`, writes them to the
    /// OS store, and rewrites the config without them.
    Migrate {
        /// Move secrets back out of the OS store into `~/.leviath/config.toml`
        #[arg(long)]
        to_file: bool,

        /// Show what would move without changing anything
        #[arg(long)]
        dry_run: bool,
    },
}

impl AuthArgs {
    /// A `status` invocation, for routing tests in `dispatch`.
    #[cfg(test)]
    pub(crate) fn status_for_test() -> Self {
        Self {
            command: AuthCommand::Status,
        }
    }

    /// A `migrate` invocation, for driving the command end to end.
    #[cfg(test)]
    pub(crate) fn migrate_for_test(to_file: bool, dry_run: bool) -> Self {
        Self {
            command: AuthCommand::Migrate { to_file, dry_run },
        }
    }
}

/// What `lev auth login` needs from the outside world.
///
/// Injected the way [`crate::commands::doctor::execute`] takes its daemon: the
/// binary supplies the real browser and the real issuer, and a test supplies a
/// stub rather than launching one.
pub struct AuthEnv {
    /// Opens the sign-in page.
    pub opener: leviath_mcp::BrowserOpener,
    /// Where the grant is stored. `None` when no home could be resolved, which
    /// is resolved here rather than inside the flow so the flow has one fewer
    /// way to fail and one fewer thing to reach for.
    pub grant_path: Option<std::path::PathBuf>,
    /// The client the token exchange goes out on.
    pub client: reqwest::Client,
    /// The OAuth issuer. Overridden only in tests.
    pub issuer: String,
    /// The loopback ports to bind, in order.
    pub ports: Vec<u16>,
}

impl AuthEnv {
    /// The real browser, the real issuer, and this machine's paths.
    pub fn real() -> Self {
        Self {
            opener: std::sync::Arc::new(leviath_sys::open_url),
            grant_path: leviath_providers::codex::ProviderAuthStore::default_path(),
            // `leviath_net::client` rather than the provider builder: this is
            // one OAuth exchange, not inference, and it cannot fail to build,
            // so there is no error arm here that nothing could ever take.
            client: leviath_net::client(leviath_net::ClientTimeouts::default()),
            issuer: leviath_providers::codex::ISSUER.to_string(),
            ports: leviath_providers::codex::CALLBACK_PORTS.to_vec(),
        }
    }
}

/// Run `lev auth`.
pub async fn execute(args: AuthArgs, env: AuthEnv) -> anyhow::Result<()> {
    let path = Config::config_path();
    match args.command {
        AuthCommand::Status => {
            let config = Config::load_from_path_public(&path)?;
            print!("{}", render_status(&status(&config, &path)));
            Ok(())
        }
        AuthCommand::Login { provider } => {
            let config = Config::load_from_path_public(&path)?;
            login(&config, &provider, env).await
        }
        AuthCommand::Logout { provider } => {
            let config = Config::load_from_path_public(&path)?;
            logout(&config, &provider, env.grant_path)
        }
        AuthCommand::Migrate { to_file, dry_run } => migrate(&path, to_file, dry_run),
    }
}

/// The providers `lev auth login` knows how to sign in to.
const OAUTH_PROVIDERS: &[&str] = &[leviath_providers::codex::PROVIDER_NAME];

/// The error for a provider name that does not sign in with a browser.
fn not_an_oauth_provider(provider: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "'{provider}' does not sign in with a browser. Providers that do: {}. \
         An API key goes in `lev setup` instead.",
        OAUTH_PROVIDERS.join(", ")
    )
}

/// The error for a machine with nowhere to put the grant.
fn no_home() -> anyhow::Error {
    anyhow::anyhow!("no home directory, so there is nowhere to store the sign-in")
}

/// Run the browser sign-in for `provider`.
async fn login(config: &Config, provider: &str, env: AuthEnv) -> anyhow::Result<()> {
    let resolved = crate::credentials::store_for(config.security.credential_store);
    login_with(config, provider, env, resolved).await
}

/// Core of [`login`] with the credential backend already resolved - see
/// [`status_with`] for why the resolution is the caller's.
async fn login_with(
    config: &Config,
    provider: &str,
    env: AuthEnv,
    resolved: crate::credentials::Resolved,
) -> anyhow::Result<()> {
    if provider != leviath_providers::codex::PROVIDER_NAME {
        return Err(not_an_oauth_provider(provider));
    }
    let store = resolved
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .map(std::sync::Arc::from);

    let mut login_env = codex::LoginEnv::new(
        env.opener,
        env.grant_path.ok_or_else(no_home)?,
        store,
        env.client,
        // Printed, not rendered: this path owns the terminal. The wizard
        // supplies its own announce for the same reason.
        std::sync::Arc::new(|url: &str| {
            println!("\nOpen this page to sign in:\n\n  {url}\n");
        }),
    );
    login_env.issuer = env.issuer;
    login_env.ports = env.ports;

    let grant = codex::login(&login_env).await?;
    let who = grant.email.as_deref().unwrap_or("this account");
    match grant.plan_type.as_deref() {
        Some(plan) => println!("Signed in as {who} on the ChatGPT {plan} plan."),
        None => println!("Signed in as {who}."),
    }
    if !config.providers.codex_enabled {
        println!(
            "\nThe provider is not enabled yet. Run `lev setup` and select it, or set \
             `codex_enabled = true` under `[providers]`."
        );
    }
    Ok(())
}

/// Forget a provider's stored sign-in.
fn logout(
    config: &Config,
    provider: &str,
    grant_path: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let resolved = crate::credentials::store_for(config.security.credential_store);
    logout_with(provider, grant_path, resolved)
}

/// Core of [`logout`] with the credential backend already resolved.
fn logout_with(
    provider: &str,
    grant_path: Option<std::path::PathBuf>,
    resolved: crate::credentials::Resolved,
) -> anyhow::Result<()> {
    if provider != leviath_providers::codex::PROVIDER_NAME {
        return Err(not_an_oauth_provider(provider));
    }
    let store = resolved.map_err(|e| anyhow::anyhow!("{e}"))?;
    let removed = codex::logout(&grant_path.ok_or_else(no_home)?, store.as_deref())?;
    match removed {
        true => println!(
            "Signed out of {provider}. It is still enabled in config.toml, so runs will fail \
             until you sign in again or turn it off."
        ),
        false => println!("Not signed in to {provider}."),
    }
    Ok(())
}

/// What `lev auth status` found, separated from how it is printed so the report
/// itself is testable.
#[derive(Debug, PartialEq)]
pub(crate) struct Status {
    /// The configured backend.
    pub kind: CredentialStoreKind,
    /// Whether this build was compiled with OS credential store support.
    pub supported: bool,
    /// `None` if the store is reachable, `Some(reason)` if it is not. Always
    /// `None` for the file backend, which needs no store.
    pub unavailable: Option<String>,
    /// Providers whose key is set, from any source.
    pub providers: Vec<String>,
    /// MCP servers with a stored OAuth grant.
    pub mcp_servers: Vec<String>,
    /// Providers signed in with a browser, as `(name, description)`. The
    /// description carries the account and plan, which is the fact a person is
    /// actually checking for.
    pub oauth_providers: Vec<(String, String)>,
    /// Providers whose key is present in *both* the config file and the OS
    /// store. A duplicate is not an error, but it is worth saying: the file
    /// copy wins, so rotating the keychain entry would appear to do nothing.
    pub duplicated: Vec<String>,
    /// The config file path, for the report.
    pub config_path: String,
}

/// Inspect the current credential situation.
pub(crate) fn status(config: &Config, path: &std::path::Path) -> Status {
    let resolved = crate::credentials::store_for(config.security.credential_store);
    status_with(config, path, resolved)
}

/// Core of [`status`] with the backend already resolved.
///
/// The resolution is the caller's because "this machine has no credential
/// store" cannot be produced in a test by *not installing* one: the real probe
/// would install the platform store and read the developer's actual login
/// keychain. Passing the outcome in is what makes the unavailable case testable.
pub(crate) fn status_with(
    config: &Config,
    path: &std::path::Path,
    resolved: crate::credentials::Resolved,
) -> Status {
    let kind = config.security.credential_store;
    let supported = leviath_sys::keychain::is_supported();

    let providers: Vec<String> = config
        .provider_secrets()
        .into_iter()
        .map(|(account, _)| account)
        .collect();

    // Read the file directly rather than through `Config::load`: the loader
    // already folded the keychain in, so it cannot tell the two sources apart.
    let on_disk = providers_in_file(path);
    // The store itself is kept, not just what it held: the OAuth grants below
    // live in the same backend and have to be read through it.
    let (unavailable, in_store, backend) = match resolved {
        Ok(Some(store)) => {
            let accounts: Vec<String> = crate::credentials::PROVIDER_KEYS
                .iter()
                .map(|p| leviath_core::provider_account(p))
                .collect();
            let found = store.read_all(&accounts).into_keys().collect();
            (None, found, Some(store))
        }
        Ok(None) => (None, Vec::new(), None),
        Err(e) => (Some(e), Vec::new(), None),
    };

    let duplicated = on_disk
        .iter()
        .filter(|a| in_store.contains(a))
        .cloned()
        .collect();

    // MCP grants live in their own store, keyed by server name. A load failure
    // is reported as "none" rather than propagated: `lev auth status` is the
    // command a user runs *because* something is wrong, so it has to answer.
    let mcp_servers = mcp_server_names(leviath_mcp::AuthStore::default_path().as_deref(), None);

    // Same policy as the MCP grants above: a load failure reads as "none"
    // rather than propagating, because this is the command someone runs when
    // something is already wrong.
    let oauth_providers = oauth_provider_summaries(
        leviath_providers::codex::ProviderAuthStore::default_path().as_deref(),
        backend.as_deref(),
    );

    Status {
        kind,
        supported,
        unavailable,
        providers,
        mcp_servers,
        oauth_providers,
        duplicated,
        config_path: path.display().to_string(),
    }
}

/// Every provider signed in with a browser, with the account behind it.
///
/// `store` is the resolved credential backend, so a keychain-held grant is
/// found rather than reported missing.
fn oauth_provider_summaries(
    path: Option<&std::path::Path>,
    store: Option<&dyn CredentialStore>,
) -> Vec<(String, String)> {
    let Some(all) =
        path.and_then(|p| leviath_providers::codex::ProviderAuthStore::load_with(p, store).ok())
    else {
        return Vec::new();
    };
    all.entries()
        .into_iter()
        .map(|(name, grant)| {
            let claims = grant.claims();
            let who = grant
                .email
                .or(claims.email)
                .unwrap_or_else(|| "signed in".to_string());
            let detail = match grant.plan_type.or(claims.plan_type) {
                Some(plan) => format!("{who} ({plan} plan)"),
                None => who,
            };
            (name, detail)
        })
        .collect()
}

/// The provider accounts that have a key written in the config *file*.
///
/// Parsed straight out of the TOML because `Config::load` merges the
/// environment and the credential store in, which is exactly the distinction
/// this needs to make.
fn providers_in_file(path: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = text.parse::<toml::Table>() else {
        return Vec::new();
    };
    crate::credentials::PROVIDER_KEYS
        .iter()
        .filter(|p| file_has_key(&value, p))
        .map(|p| leviath_core::provider_account(p))
        .collect()
}

/// Whether the parsed config file carries a key for `provider`.
///
/// `openrouter_api_key` sits at the top level while the other three live under
/// `[providers]` - a historical split the config struct still reflects.
fn file_has_key(value: &toml::Table, provider: &str) -> bool {
    let field = format!("{provider}_api_key");
    if provider == "openrouter" {
        return value.get(&field).and_then(|v| v.as_str()).is_some();
    }
    value
        .get("providers")
        .and_then(|p| p.get(&field))
        .and_then(|v| v.as_str())
        .is_some()
}

/// Render a [`Status`] for the terminal.
pub(crate) fn render_status(s: &Status) -> String {
    let mut out = String::new();
    let backend = match s.kind {
        CredentialStoreKind::File => "file (Leviath's own 0600 files)",
        CredentialStoreKind::Keychain => "keychain (OS credential store)",
    };
    out.push_str(&format!("Credential store: {backend}\n"));
    out.push_str(&format!("Config file:      {}\n", s.config_path));

    if !s.supported {
        out.push_str(
            "\nThis build has no OS credential store support (the `keychain` feature is off).\n",
        );
    }
    if let Some(reason) = &s.unavailable {
        out.push_str(&format!("\n! {reason}\n"));
    }

    out.push('\n');
    if s.providers.is_empty() {
        out.push_str("No provider API keys are configured. Run `lev setup` to add one.\n");
    } else {
        out.push_str("Provider keys configured:\n");
        for p in &s.providers {
            out.push_str(&format!("  - {p}\n"));
        }
    }

    if !s.oauth_providers.is_empty() {
        out.push_str("\nProviders signed in with a browser:\n");
        for (name, detail) in &s.oauth_providers {
            out.push_str(&format!("  - {name}: {detail}\n"));
        }
    }

    if !s.mcp_servers.is_empty() {
        out.push_str("\nMCP servers logged in:\n");
        for m in &s.mcp_servers {
            out.push_str(&format!("  - {m}\n"));
        }
    }

    if !s.duplicated.is_empty() {
        out.push_str(
            "\n! These are stored in BOTH the config file and the OS keychain. The file copy\n  \
             wins, so changing the keychain entry will appear to have no effect. Run\n  \
             `lev auth migrate` to remove the file copies.\n",
        );
        for p in &s.duplicated {
            out.push_str(&format!("  - {p}\n"));
        }
    }

    if s.kind == CredentialStoreKind::File && s.supported {
        out.push_str(
            "\nTo move these into the OS keychain, set `[security] credential_store = \"keychain\"`\n\
             in the config file and run `lev auth migrate`.\n",
        );
    }
    out
}

/// Move secrets between the config file and the OS credential store.
fn migrate(path: &std::path::Path, to_file: bool, dry_run: bool) -> anyhow::Result<()> {
    let config = Config::load_from_path_public(path)?;
    let plan = plan_migration(&config, to_file);

    if plan.moving.is_empty() {
        println!("{}", plan.summary);
        return Ok(());
    }

    println!("{}", plan.summary);
    for moved in &plan.moving {
        println!("  - {moved}");
    }
    if dry_run {
        println!("\nDry run: nothing was changed.");
        return Ok(());
    }

    apply_migration(&config, path, to_file)?;
    println!("\nDone. {}", plan.done);
    Ok(())
}

/// What a migration would do, computed without changing anything.
#[derive(Debug, PartialEq)]
pub(crate) struct MigrationPlan {
    pub moving: Vec<String>,
    pub summary: String,
    pub done: String,
}

pub(crate) fn plan_migration(config: &Config, to_file: bool) -> MigrationPlan {
    let moving: Vec<String> = config
        .provider_secrets()
        .into_iter()
        .map(|(account, _)| account)
        .collect();

    if moving.is_empty() {
        return MigrationPlan {
            moving,
            summary: "No provider API keys are configured; there is nothing to move.".to_string(),
            done: String::new(),
        };
    }

    let (summary, done) = if to_file {
        (
            "Moving these secrets out of the OS keychain and into the config file:",
            "The config file now holds these keys (mode 0600). Set `[security] \
             credential_store = \"file\"` if you have not already.",
        )
    } else {
        (
            "Moving these secrets into the OS keychain:",
            "The config file no longer contains these keys. Set `[security] \
             credential_store = \"keychain\"` if you have not already.",
        )
    };
    MigrationPlan {
        moving,
        summary: summary.to_string(),
        done: done.to_string(),
    }
}

/// Perform the move.
///
/// The order matters in both directions: write the destination first, verify it
/// took, and only then remove the source. A migration that cleared the config
/// file before the keychain write succeeded would destroy the user's API keys.
fn apply_migration(config: &Config, path: &std::path::Path, to_file: bool) -> anyhow::Result<()> {
    let resolved = crate::credentials::store_for(CredentialStoreKind::Keychain);
    apply_migration_with(
        config,
        path,
        to_file,
        resolved,
        leviath_mcp::AuthStore::default_path().as_deref(),
        leviath_providers::codex::ProviderAuthStore::default_path().as_deref(),
    )
}

/// Core of [`apply_migration`] with the keychain already resolved - see
/// [`status_with`] for why the resolution is the caller's.
fn apply_migration_with(
    config: &Config,
    path: &std::path::Path,
    to_file: bool,
    resolved: crate::credentials::Resolved,
    mcp_path: Option<&std::path::Path>,
    grant_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let secrets = config.provider_secrets();

    if to_file {
        // The keys are already in `config` (the loader folded them in), so
        // saving with the file backend writes them out. Clear the keychain only
        // after that write has succeeded.
        let mut file_config = config.clone();
        file_config.security.credential_store = CredentialStoreKind::File;
        file_config.save_to_path_public(path)?;

        if let Ok(Some(store)) = resolved {
            for (account, _) in &secrets {
                // A failure to clean up is not a failure to migrate: the keys
                // are safely in the file, and a leftover keychain entry is
                // reported by `lev auth status` as a duplicate.
                if let Err(e) = store.delete(account) {
                    tracing::warn!("could not remove {account} from the keychain: {e}");
                }
            }
            // The MCP grants move the same direction: out of the keychain and
            // back into their own file.
            let names = mcp_server_names(mcp_path, Some(store.as_ref()));
            migrate_mcp_grants(mcp_path, Some(store.as_ref()), None)?;
            for name in names {
                if let Err(e) = store.delete(&leviath_core::mcp_account(&name)) {
                    tracing::warn!("could not remove the grant for '{name}': {e}");
                }
            }
            // And the provider sign-ins, which move the same direction.
            let signed_in = provider_grant_names(grant_path, Some(store.as_ref()));
            migrate_provider_grants(grant_path, Some(store.as_ref()), None)?;
            for name in signed_in {
                let account = leviath_providers::codex::grant_account(&name);
                if let Err(e) = store.delete(&account) {
                    tracing::warn!("could not remove the sign-in for '{name}': {e}");
                }
            }
        }
        return Ok(());
    }

    let store = resolved
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .ok_or_else(|| anyhow::anyhow!("no OS credential store is available"))?;

    for (account, secret) in &secrets {
        store
            .set(account, secret)
            .map_err(|e| anyhow::anyhow!("failed to store {account}: {e}"))?;
        // Read it back before trusting it. A store that accepts a write and
        // returns nothing would otherwise lose the key when the file copy is
        // removed below.
        match store.get(account) {
            Ok(Some(v)) if &v == secret => {}
            _ => anyhow::bail!(
                "{account} did not read back correctly from the credential store; \
                 the config file has been left unchanged"
            ),
        }
    }

    // Only now is it safe to drop the file copies.
    let mut stripped = config.clone();
    stripped.security.credential_store = CredentialStoreKind::Keychain;
    stripped.save_to_path_public(path)?;

    migrate_mcp_grants(mcp_path, None, Some(store.as_ref()))?;
    migrate_provider_grants(grant_path, None, Some(store.as_ref()))
}

/// Rewrite the provider grant store at `path`, moving its grants from `source`
/// to `destination`.
///
/// The twin of [`migrate_mcp_grants`], and needed for the same reason: a
/// migration that moved the API keys and left a refresh token in a plaintext
/// file would report that the secrets had moved while one of them had not.
fn migrate_provider_grants(
    path: Option<&std::path::Path>,
    source: Option<&dyn CredentialStore>,
    destination: Option<&dyn CredentialStore>,
) -> anyhow::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if !path.exists() {
        return Ok(());
    }
    let store = leviath_providers::codex::ProviderAuthStore::load_with(path, source)?;
    store.save_with(path, destination)
}

/// The providers with a stored sign-in, read through `store`.
fn provider_grant_names(
    path: Option<&std::path::Path>,
    store: Option<&dyn CredentialStore>,
) -> Vec<String> {
    path.and_then(|p| leviath_providers::codex::ProviderAuthStore::load_with(p, store).ok())
        .map(|s| s.names())
        .unwrap_or_default()
}

/// Rewrite the MCP auth store at `path`, moving its grants from `source` to
/// `destination`.
///
/// `None` on either side means the file itself. The grants are read through
/// whichever backend holds them today and written to the other, so this is the
/// same operation in both directions.
///
/// A missing path or a store that was never created is not an error: a user who
/// has never run `lev mcp login` has nothing to move.
fn migrate_mcp_grants(
    path: Option<&std::path::Path>,
    source: Option<&dyn CredentialStore>,
    destination: Option<&dyn CredentialStore>,
) -> anyhow::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if !path.exists() {
        return Ok(());
    }
    let store = leviath_mcp::AuthStore::load_with(path, source)?;
    store.save_with(path, destination)
}

/// The MCP servers with a stored grant, read through `store`.
fn mcp_server_names(
    path: Option<&std::path::Path>,
    store: Option<&dyn CredentialStore>,
) -> Vec<String> {
    path.and_then(|p| leviath_mcp::AuthStore::load_with(p, store).ok())
        .map(|s| {
            let mut names: Vec<String> = s
                .server_names()
                .into_iter()
                .map(str::to_string)
                .chain(s.keychain_server_names().iter().cloned())
                .collect();
            names.sort();
            names.dedup();
            names
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::credentials::test_store;

    fn with_mock_store() -> std::sync::MutexGuard<'static, ()> {
        test_store::with_mock()
    }

    /// The keychain backend, resolved against whatever store is installed.
    fn keychain() -> crate::credentials::Resolved {
        crate::credentials::store_for(CredentialStoreKind::Keychain)
    }

    /// A store whose three operations answer however a test needs.
    ///
    /// One configurable stub rather than a bespoke struct per test: a struct
    /// with a `delete` no test ever calls is an uncovered method, and the point
    /// here is the *combination* of answers, not the type.
    struct Stub {
        get: fn(&str) -> Result<Option<String>, String>,
        set: fn(&str, &str) -> Result<(), String>,
        delete: fn(&str) -> Result<bool, String>,
    }

    impl CredentialStore for Stub {
        fn get(&self, account: &str) -> Result<Option<String>, String> {
            (self.get)(account)
        }
        fn set(&self, account: &str, secret: &str) -> Result<(), String> {
            (self.set)(account, secret)
        }
        fn delete(&self, account: &str) -> Result<bool, String> {
            (self.delete)(account)
        }
    }

    fn absent(_: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn accepts_write(_: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn refuses_write(_: &str, _: &str) -> Result<(), String> {
        Err("read-only keychain".to_string())
    }
    fn refuses_delete(_: &str) -> Result<bool, String> {
        Err("cannot delete".to_string())
    }

    /// Make `path` unwritable, and undo it.
    ///
    /// `set_readonly` rather than a `0400` chmod: it clears the write bits on
    /// Unix *and* sets the read-only attribute on Windows, so the "the rewrite
    /// failed" tests run on every platform. Gated to Unix they left the `?` arms
    /// they cover unexercised on Windows, which the gate then failed on.
    fn set_readonly(path: &std::path::Path, readonly: bool) {
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_readonly(readonly);
        std::fs::set_permissions(path, perms).unwrap();
    }

    /// The keychain backend on a machine that has none.
    fn no_keychain() -> crate::credentials::Resolved {
        Err(
            "`[security] credential_store = \"keychain\"` is set, but OS \
             credential store unavailable: no default store"
                .to_string(),
        )
    }

    fn config_with_keys(kind: CredentialStoreKind) -> Config {
        let mut c = Config::default();
        c.security.credential_store = kind;
        c.providers.anthropic_api_key = Some("sk-ant-secret".into());
        c.openrouter_api_key = Some("sk-or-secret".into());
        c
    }

    /// The end-to-end move: keys start in the file, end in the keychain, and
    /// the file no longer contains them.
    #[test]
    fn migrating_to_the_keychain_moves_the_secrets_out_of_the_file() {
        let _guard = with_mock_store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let config = config_with_keys(CredentialStoreKind::File);
        config.save_to_path_public(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(before.contains("sk-ant-secret"), "the file starts with it");

        apply_migration_with(&config, &path, false, keychain(), None, None).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            !after.contains("sk-ant-secret") && !after.contains("sk-or-secret"),
            "no secret may remain in the file: {after}"
        );

        let store = crate::credentials::store_for(CredentialStoreKind::Keychain)
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .get(&leviath_core::provider_account("anthropic"))
                .unwrap()
                .as_deref(),
            Some("sk-ant-secret")
        );
        assert_eq!(
            store
                .get(&leviath_core::provider_account("openrouter"))
                .unwrap()
                .as_deref(),
            Some("sk-or-secret")
        );
    }

    /// And back again, so the keychain is not a one-way door.
    #[test]
    fn migrating_to_the_file_restores_the_secrets_and_clears_the_keychain() {
        let _guard = with_mock_store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let config = config_with_keys(CredentialStoreKind::Keychain);
        apply_migration_with(&config, &path, false, keychain(), None, None).unwrap();
        apply_migration_with(&config, &path, true, keychain(), None, None).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("sk-ant-secret"), "back in the file: {after}");

        let store = crate::credentials::store_for(CredentialStoreKind::Keychain)
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .get(&leviath_core::provider_account("anthropic"))
                .unwrap(),
            None,
            "and gone from the keychain"
        );
    }

    /// The safety property that matters most: if the credential store cannot be
    /// written, the config file must be left alone. Losing the user's API keys
    /// to a half-finished migration is the worst outcome available here.
    #[test]
    fn a_failing_store_leaves_the_config_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = config_with_keys(CredentialStoreKind::File);
        config.save_to_path_public(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        assert!(
            apply_migration_with(&config, &path, false, no_keychain(), None, None).is_err(),
            "no store means no migration"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "the file must be byte-identical after a failed migration"
        );
    }

    /// A store that silently drops writes must be caught by the read-back,
    /// before the file copies are removed. Without it, `set` succeeding would be
    /// taken as proof and the only copy of the key would be deleted.
    #[test]
    fn a_store_that_does_not_persist_aborts_before_the_file_is_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = config_with_keys(CredentialStoreKind::File);
        config.save_to_path_public(&path).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        // Accepts the write, then reports nothing back.
        let amnesiac = Stub {
            get: absent,
            set: accepts_write,
            delete: refuses_delete,
        };
        let err = apply_migration_with(
            &config,
            &path,
            false,
            Ok(Some(Box::new(amnesiac))),
            None,
            None,
        )
        .expect_err("a store that does not persist must not be trusted");
        assert!(err.to_string().contains("did not read back"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "and the file is untouched"
        );
    }

    /// A store that refuses the write at all is caught the same way.
    #[test]
    fn a_store_that_refuses_the_write_aborts_the_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = config_with_keys(CredentialStoreKind::File);

        let refuses = Stub {
            get: absent,
            set: refuses_write,
            delete: refuses_delete,
        };
        let err = apply_migration_with(
            &config,
            &path,
            false,
            Ok(Some(Box::new(refuses))),
            None,
            None,
        )
        .expect_err("a refused write is not a migration");
        assert!(err.to_string().contains("failed to store"), "{err}");
    }

    /// Migrating *to* the file is not blocked by a keychain that cannot be
    /// cleaned up: the keys are already safely written, and a leftover entry is
    /// reported by `lev auth status` as a duplicate rather than lost data.
    #[test]
    fn cleanup_failures_do_not_fail_a_migration_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = config_with_keys(CredentialStoreKind::Keychain);

        let undeletable = Stub {
            get: absent,
            set: accepts_write,
            delete: refuses_delete,
        };
        // A real MCP store too, so the grant cleanup runs and its failure is
        // shown to be non-fatal as well.
        let mcp = dir.path().join("mcp-auth.json");
        write_mcp_store(&mcp, "github");

        apply_migration_with(
            &config,
            &path,
            true,
            Ok(Some(Box::new(undeletable))),
            Some(&mcp),
            None,
        )
        .expect("the keys are in the file; cleanup is best effort");
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("sk-ant-secret"), "{after}");
    }

    /// The ordinary to-file path: both the provider keys and the MCP grants come
    /// back, and the keychain entries are cleaned up without incident.
    #[test]
    fn migrating_to_the_file_also_brings_back_the_mcp_grants() {
        use leviath_core::CredentialStore as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mcp = dir.path().join("mcp-auth.json");
        let config = config_with_keys(CredentialStoreKind::Keychain);

        // Put a grant in the store, and leave the file holding only the index.
        let store = leviath_core::MemoryStore::new();
        write_mcp_store(&mcp, "github");
        migrate_mcp_grants(Some(&mcp), None, Some(&store)).unwrap();
        assert!(!std::fs::read_to_string(&mcp).unwrap().contains("rt-SECRET"));
        store
            .set(
                &leviath_core::provider_account("anthropic"),
                "sk-ant-secret",
            )
            .unwrap();

        apply_migration_with(
            &config,
            &path,
            true,
            Ok(Some(Box::new(store))),
            Some(&mcp),
            None,
        )
        .unwrap();

        assert!(
            std::fs::read_to_string(&mcp).unwrap().contains("rt-SECRET"),
            "the grant is back in its own file"
        );
    }

    /// A corrupt MCP store must fail the migration rather than be reported as a
    /// completed move that silently dropped every login.
    #[test]
    fn a_corrupt_mcp_store_fails_a_migration_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mcp = dir.path().join("mcp-auth.json");
        std::fs::write(&mcp, "not json").unwrap();

        let config = config_with_keys(CredentialStoreKind::Keychain);
        let store = leviath_core::MemoryStore::new();
        let err = apply_migration_with(
            &config,
            &path,
            true,
            Ok(Some(Box::new(store))),
            Some(&mcp),
            None,
        )
        .expect_err("a corrupt MCP store is not a successful migration");
        assert!(!err.to_string().is_empty());
    }

    /// A corrupt MCP store fails the migration into the keychain too, not just
    /// the one back out of it.
    #[test]
    fn a_corrupt_mcp_store_fails_the_migration_into_the_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mcp = dir.path().join("mcp-auth.json");
        std::fs::write(&mcp, "not json").unwrap();

        let config = config_with_keys(CredentialStoreKind::File);
        config.save_to_path_public(&path).unwrap();
        let store = leviath_core::MemoryStore::new();
        assert!(
            apply_migration_with(
                &config,
                &path,
                false,
                Ok(Some(Box::new(store))),
                Some(&mcp),
                None,
            )
            .is_err()
        );
    }

    /// And a migration to the file still works when there is no keychain at all
    /// to clean up.
    #[test]
    fn migrating_to_the_file_works_without_a_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = config_with_keys(CredentialStoreKind::Keychain);
        apply_migration_with(&config, &path, true, no_keychain(), None, None).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("sk-ant-secret")
        );
    }

    /// The rewrite that completes a move into the keychain has to be able to
    /// fail: the secrets are already in the store, but the config still names
    /// them, and reporting success would be a lie.
    #[test]
    fn a_failed_final_rewrite_fails_the_migration() {
        let _guard = with_mock_store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = config_with_keys(CredentialStoreKind::File);
        config.save_to_path_public(&path).unwrap();
        set_readonly(&path, true);

        let err = apply_migration_with(&config, &path, false, keychain(), None, None)
            .expect_err("an unwritable config cannot complete the move");
        assert!(!err.to_string().is_empty());

        set_readonly(&path, false);
    }

    /// `Ok(None)` - the file backend where a keychain was expected - is a
    /// refusal, not a silent no-op that would strip the file.
    #[test]
    fn migrating_to_a_backend_that_is_not_a_store_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = config_with_keys(CredentialStoreKind::File);
        let err = apply_migration_with(&config, &path, false, Ok(None), None, None)
            .expect_err("there is nowhere to migrate to");
        assert!(err.to_string().contains("no OS credential store"), "{err}");
    }

    #[test]
    fn the_plan_lists_every_configured_key_and_says_nothing_when_there_are_none() {
        let plan = plan_migration(&config_with_keys(CredentialStoreKind::File), false);
        assert_eq!(plan.moving.len(), 2);
        assert!(plan.summary.contains("into the OS keychain"));

        let back = plan_migration(&config_with_keys(CredentialStoreKind::Keychain), true);
        assert!(back.summary.contains("out of the OS keychain"));
        assert!(back.done.contains("credential_store = \"file\""));

        let empty = plan_migration(&Config::default(), false);
        assert!(empty.moving.is_empty());
        assert!(empty.summary.contains("nothing to move"));
    }

    /// The duplicate warning: a key in both places is not an error, but the file
    /// copy wins, so rotating the keychain entry would silently do nothing.
    #[test]
    fn status_reports_a_secret_stored_in_both_places() {
        let _guard = with_mock_store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // Written with the file backend, so the key lands in the TOML...
        let config = config_with_keys(CredentialStoreKind::File);
        config.save_to_path_public(&path).unwrap();
        // ...and also placed in the keychain.
        let store = crate::credentials::store_for(CredentialStoreKind::Keychain)
            .unwrap()
            .unwrap();
        store
            .set(
                &leviath_core::provider_account("anthropic"),
                "sk-ant-secret",
            )
            .unwrap();

        let mut keychain_config = config.clone();
        keychain_config.security.credential_store = CredentialStoreKind::Keychain;
        let s = status_with(&keychain_config, &path, keychain());

        assert_eq!(s.duplicated, vec!["provider/anthropic".to_string()]);
        let rendered = render_status(&s);
        assert!(rendered.contains("BOTH"), "{rendered}");
        assert!(rendered.contains("lev auth migrate"), "{rendered}");
    }

    #[test]
    fn status_on_a_plain_file_install_says_so_and_offers_the_keychain() {
        let _guard = with_mock_store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = config_with_keys(CredentialStoreKind::File);
        config.save_to_path_public(&path).unwrap();

        let s = status_with(&config, &path, Ok(None));
        assert_eq!(s.kind, CredentialStoreKind::File);
        assert!(s.unavailable.is_none(), "the file backend needs no store");
        assert!(s.duplicated.is_empty());
        assert_eq!(s.providers.len(), 2);

        let rendered = render_status(&s);
        assert!(rendered.contains("file (Leviath's own 0600 files)"));
        assert!(rendered.contains("credential_store = \"keychain\""));
    }

    /// An unavailable keychain has to be reported rather than looking like an
    /// empty one.
    #[test]
    fn status_reports_an_unreachable_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let s = status_with(
            &config_with_keys(CredentialStoreKind::Keychain),
            &path,
            no_keychain(),
        );
        assert!(s.unavailable.is_some());
        let rendered = render_status(&s);
        assert!(
            rendered.contains("credential store unavailable"),
            "{rendered}"
        );
    }

    #[test]
    fn status_with_no_keys_points_at_setup() {
        let _guard = with_mock_store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let s = status_with(&Config::default(), &path, Ok(None));
        assert!(s.providers.is_empty());
        let rendered = render_status(&s);
        assert!(rendered.contains("lev setup"), "{rendered}");
    }

    /// A build compiled without keychain support must say so rather than
    /// offering a migration that cannot work.
    #[test]
    fn a_build_without_keychain_support_says_so() {
        let s = Status {
            kind: CredentialStoreKind::File,
            supported: false,
            unavailable: None,
            providers: vec!["provider/anthropic".into()],
            mcp_servers: Vec::new(),
            oauth_providers: Vec::new(),
            duplicated: Vec::new(),
            config_path: "/x/config.toml".into(),
        };
        let rendered = render_status(&s);
        assert!(
            rendered.contains("no OS credential store support"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("credential_store = \"keychain\""),
            "and must not suggest a backend it cannot use: {rendered}"
        );
    }

    /// The file scan has to tell the top-level `openrouter_api_key` apart from
    /// the three under `[providers]`, and must not fall over on an unreadable or
    /// malformed file.
    #[test]
    fn the_file_scan_finds_keys_in_both_shapes_and_tolerates_a_bad_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        assert!(
            providers_in_file(&path).is_empty(),
            "a missing file is empty"
        );

        std::fs::write(&path, "this is not toml = = =").unwrap();
        assert!(providers_in_file(&path).is_empty(), "so is a broken one");

        std::fs::write(
            &path,
            "openrouter_api_key = \"a\"\n[providers]\nanthropic_api_key = \"b\"\n",
        )
        .unwrap();
        let found = providers_in_file(&path);
        assert!(found.contains(&"provider/openrouter".to_string()));
        assert!(found.contains(&"provider/anthropic".to_string()));
        assert_eq!(found.len(), 2, "and nothing else: {found:?}");
    }

    /// `migrate` must propagate a failed migration rather than reporting
    /// success. Here the config file itself is read-only, so the rewrite that
    /// completes the move cannot happen.
    #[test]
    fn migrate_propagates_a_failed_move() {
        let _guard = with_mock_store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        config_with_keys(CredentialStoreKind::File)
            .save_to_path_public(&path)
            .unwrap();
        // Readable, so the load succeeds; unwritable, so the rewrite does not.
        set_readonly(&path, true);

        let err = run_auth(&path, AuthArgs::migrate_for_test(true, false))
            .expect_err("an unwritable config cannot be migrated");
        assert!(!err.to_string().is_empty());

        set_readonly(&path, false);
    }

    /// Writing a grant into a temporary MCP auth store, so the migration
    /// helpers have something to move.
    fn write_mcp_store(path: &std::path::Path, server: &str) {
        let mut store = leviath_mcp::AuthStore::default();
        store.set(
            server,
            leviath_mcp::ServerAuth {
                resource: "https://example.test/mcp".to_string(),
                issuer: "https://example.test".to_string(),
                authorization_endpoint: "https://example.test/authorize".to_string(),
                token_endpoint: "https://example.test/token".to_string(),
                client_id: "cid".to_string(),
                access_token: "at-SECRET".to_string(),
                refresh_token: Some("rt-SECRET".to_string()),
                expires_at: 9_999_999_999,
                scope: String::new(),
            },
        );
        store.save(path).unwrap();
    }

    /// MCP OAuth grants move with the provider keys: tokens out of the file,
    /// only the server name left behind as an index.
    #[test]
    fn mcp_grants_move_into_the_credential_store_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = dir.path().join("mcp-auth.json");
        write_mcp_store(&mcp, "github");
        assert!(std::fs::read_to_string(&mcp).unwrap().contains("rt-SECRET"));

        let store = leviath_core::MemoryStore::new();
        migrate_mcp_grants(Some(&mcp), None, Some(&store)).unwrap();

        let on_disk = std::fs::read_to_string(&mcp).unwrap();
        assert!(!on_disk.contains("rt-SECRET"), "{on_disk}");
        assert!(!on_disk.contains("at-SECRET"), "{on_disk}");
        assert!(on_disk.contains("github"), "the index remains: {on_disk}");
        assert_eq!(mcp_server_names(Some(&mcp), Some(&store)), ["github"]);

        // ...and back again.
        migrate_mcp_grants(Some(&mcp), Some(&store), None).unwrap();
        let restored = std::fs::read_to_string(&mcp).unwrap();
        assert!(restored.contains("rt-SECRET"), "{restored}");
    }

    /// Nothing to move is not an error - a user who has never run
    /// `lev mcp login` has no store, and no home is not a failure either.
    #[test]
    fn migrating_mcp_grants_is_a_no_op_when_there_is_nothing_to_move() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("mcp-auth.json");

        migrate_mcp_grants(None, None, None).expect("no path, nothing to do");
        migrate_mcp_grants(Some(&missing), None, None).expect("no file, nothing to do");
        assert!(mcp_server_names(None, None).is_empty());
        assert!(mcp_server_names(Some(&missing), None).is_empty());
    }

    /// A corrupt MCP store fails the migration rather than silently discarding
    /// every stored grant.
    #[test]
    fn a_corrupt_mcp_store_fails_the_migration() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = dir.path().join("mcp-auth.json");
        std::fs::write(&mcp, "not json").unwrap();

        assert!(migrate_mcp_grants(Some(&mcp), None, None).is_err());
        // The reporting path is more forgiving: `lev auth status` is the command
        // a user runs *because* something is wrong, so it answers with "none"
        // rather than refusing to run.
        assert!(mcp_server_names(Some(&mcp), None).is_empty());
    }

    /// The status report lists logged-in MCP servers.
    #[test]
    fn status_lists_mcp_servers() {
        let s = Status {
            kind: CredentialStoreKind::Keychain,
            supported: true,
            unavailable: None,
            providers: vec!["provider/anthropic".into()],
            mcp_servers: vec!["github".into(), "linear".into()],
            oauth_providers: Vec::new(),
            duplicated: Vec::new(),
            config_path: "/x/config.toml".into(),
        };
        let rendered = render_status(&s);
        assert!(rendered.contains("MCP servers logged in"), "{rendered}");
        assert!(rendered.contains("- github"), "{rendered}");
        assert!(rendered.contains("- linear"), "{rendered}");
    }

    /// A config file that cannot be parsed must fail the command rather than
    /// being treated as an empty install - both entry points read it.
    #[test]
    fn a_broken_config_file_fails_both_subcommands() {
        let _guard = with_mock_store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is not = = toml").unwrap();

        assert!(
            run_auth(&path, AuthArgs::status_for_test()).is_err(),
            "status must not report a broken config as an empty one"
        );
        assert!(
            run_auth(&path, AuthArgs::migrate_for_test(false, false)).is_err(),
            "and migrate must not act on one"
        );
    }

    /// A migration whose write fails has to surface through `migrate`, not just
    /// through `apply_migration_with`.
    #[test]
    fn migrate_reports_a_failing_store() {
        let _guard = test_store::lock();
        // No store installed: `store_for` probes, and on a machine with a real
        // keychain that would reach it - so drive the seam directly instead.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = config_with_keys(CredentialStoreKind::File);
        config.save_to_path_public(&path).unwrap();

        let refuses = Stub {
            get: absent,
            set: refuses_write,
            delete: refuses_delete,
        };
        assert!(
            apply_migration_with(
                &config,
                &path,
                false,
                Ok(Some(Box::new(refuses))),
                None,
                None
            )
            .is_err()
        );
    }

    /// The `to_file` direction writes the config first; an unwritable path has
    /// to fail rather than silently clearing the keychain.
    #[test]
    fn migrating_to_an_unwritable_path_fails_before_touching_the_keychain() {
        let dir = tempfile::tempdir().unwrap();
        // A file where a parent directory would have to be.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let path = blocker.join("config.toml");

        let config = config_with_keys(CredentialStoreKind::Keychain);
        assert!(
            apply_migration_with(&config, &path, true, no_keychain(), None, None).is_err(),
            "an unwritable destination is not a migration"
        );
    }

    /// Drive `execute` - the real entry point - for each subcommand, against a
    /// config path of our choosing.
    ///
    /// Plain `#[test]`s driving their own runtime rather than `#[tokio::test]`:
    /// the mock-store guard has to be held across the whole call, and holding a
    /// `std` guard across an `.await` is a deadlock the scheduler is free to
    /// arrange.
    fn run_auth(path: &std::path::Path, args: AuthArgs) -> anyhow::Result<()> {
        run_auth_with(
            path,
            args,
            AuthEnv {
                // Never the real one: a test must not launch a browser.
                opener: std::sync::Arc::new(|_| false),
                grant_path: leviath_providers::codex::ProviderAuthStore::default_path(),
                client: reqwest::Client::new(),
                issuer: leviath_providers::codex::ISSUER.to_string(),
                ports: vec![0],
            },
        )
    }

    /// [`run_auth`] with the outside world supplied by the caller.
    fn run_auth_with(path: &std::path::Path, args: AuthArgs, env: AuthEnv) -> anyhow::Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        temp_env::with_var("LEVIATH_CONFIG_PATH", Some(path.as_os_str()), || {
            rt.block_on(execute(args, env))
        })
    }

    #[test]
    fn execute_status_reads_the_configured_path() {
        let _guard = with_mock_store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        config_with_keys(CredentialStoreKind::File)
            .save_to_path_public(&path)
            .unwrap();

        run_auth(&path, AuthArgs::status_for_test()).expect("status succeeds");
    }

    /// `--dry-run` reports the plan and changes nothing.
    #[test]
    fn execute_migrate_dry_run_changes_nothing() {
        let _guard = with_mock_store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        config_with_keys(CredentialStoreKind::File)
            .save_to_path_public(&path)
            .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        run_auth(&path, AuthArgs::migrate_for_test(false, true)).expect("dry run succeeds");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a dry run must not touch the file"
        );
    }

    /// And the real thing, through the command rather than the helper.
    #[test]
    fn execute_migrate_moves_the_keys() {
        let _guard = with_mock_store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        config_with_keys(CredentialStoreKind::File)
            .save_to_path_public(&path)
            .unwrap();

        run_auth(&path, AuthArgs::migrate_for_test(false, false)).expect("migrate succeeds");
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("sk-ant-secret"), "{after}");
    }

    /// With nothing configured there is nothing to move, and that is reported
    /// rather than treated as an error.
    #[test]
    fn execute_migrate_with_no_keys_is_a_no_op() {
        let _guard = with_mock_store();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();

        run_auth(&path, AuthArgs::migrate_for_test(false, false)).expect("nothing to do succeeds");
    }

    // ── provider sign-ins ───────────────────────────────────────────────────

    /// A grant file holding one signed-in provider.
    fn write_grant_store(path: &std::path::Path) {
        let mut store = leviath_providers::codex::ProviderAuthStore::default();
        store.set(
            "codex",
            leviath_providers::ProviderGrant {
                access_token: "at-SECRET".to_string(),
                refresh_token: "rt-SECRET".to_string(),
                email: Some("someone@example.com".to_string()),
                plan_type: Some("plus".to_string()),
                ..Default::default()
            },
        );
        store.save(path).unwrap();
    }

    /// A migration that moved the API keys and left a refresh token in a
    /// plaintext file would report that the secrets had moved while one of them
    /// had not.
    #[test]
    fn a_provider_sign_in_moves_into_the_keychain_with_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let grants = dir.path().join("provider-auth.json");
        write_grant_store(&grants);

        let config = config_with_keys(CredentialStoreKind::File);
        config.save_to_path_public(&path).unwrap();
        assert!(
            std::fs::read_to_string(&grants)
                .unwrap()
                .contains("rt-SECRET"),
            "the file starts with the token"
        );

        let store = leviath_core::MemoryStore::new();
        apply_migration_with(
            &config,
            &path,
            false,
            Ok(Some(Box::new(store))),
            None,
            Some(&grants),
        )
        .unwrap();

        let after = std::fs::read_to_string(&grants).unwrap();
        assert!(
            !after.contains("rt-SECRET"),
            "the token stayed behind: {after}"
        );
        assert!(after.contains("codex"), "the name index went too: {after}");
    }

    /// And back out again, so the two directions are the same operation.
    #[test]
    fn a_provider_sign_in_comes_back_out_of_the_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let grants = dir.path().join("provider-auth.json");
        let store = leviath_core::MemoryStore::new();

        let mut initial = leviath_providers::codex::ProviderAuthStore::default();
        initial.set(
            "codex",
            leviath_providers::ProviderGrant {
                access_token: "at-SECRET".to_string(),
                refresh_token: "rt-SECRET".to_string(),
                ..Default::default()
            },
        );
        initial.save_with(&grants, Some(&store)).unwrap();

        let config = config_with_keys(CredentialStoreKind::Keychain);
        config.save_to_path_public(&path).unwrap();

        apply_migration_with(
            &config,
            &path,
            true,
            Ok(Some(Box::new(store))),
            None,
            Some(&grants),
        )
        .unwrap();

        assert!(
            std::fs::read_to_string(&grants)
                .unwrap()
                .contains("rt-SECRET"),
            "the token did not come back to the file"
        );
    }

    /// A grant file that will not parse fails the migration in either
    /// direction rather than reporting a move that did not happen.
    #[test]
    fn a_corrupt_grant_file_fails_the_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let grants = dir.path().join("provider-auth.json");
        std::fs::write(&grants, "{ not json").unwrap();
        let config = config_with_keys(CredentialStoreKind::Keychain);
        config.save_to_path_public(&path).unwrap();

        for to_file in [true, false] {
            let store = leviath_core::MemoryStore::new();
            assert!(
                apply_migration_with(
                    &config,
                    &path,
                    to_file,
                    Ok(Some(Box::new(store))),
                    None,
                    Some(&grants),
                )
                .is_err(),
                "to_file={to_file}"
            );
        }
    }

    /// A keychain that will not give the sign-in up is a cleanup failure, not a
    /// migration failure: the grant is safely in the file either way.
    #[test]
    fn a_sign_in_that_cannot_be_deleted_still_migrates_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let grants = dir.path().join("provider-auth.json");
        let config = config_with_keys(CredentialStoreKind::Keychain);
        config.save_to_path_public(&path).unwrap();

        // Seeded through the very store the migration will read it back from,
        // so the cleanup has a sign-in to try to delete. A different store
        // would leave nothing there and the loop would never run.
        let store = Undeletable::default();
        let mut initial = leviath_providers::codex::ProviderAuthStore::default();
        initial.set(
            "codex",
            leviath_providers::ProviderGrant {
                access_token: "at".to_string(),
                refresh_token: "rt-SECRET".to_string(),
                ..Default::default()
            },
        );
        initial.save_with(&grants, Some(&store)).unwrap();

        apply_migration_with(
            &config,
            &path,
            true,
            Ok(Some(Box::new(store))),
            None,
            Some(&grants),
        )
        .expect("cleanup is best effort");
    }

    /// An ordinary in-memory store that refuses to delete.
    ///
    /// It has to read back what it was given: the migration verifies every key
    /// it wrote before dropping the file copy, so a store that answered with
    /// something else would fail for the wrong reason.
    #[derive(Default)]
    struct Undeletable(leviath_core::MemoryStore);

    impl CredentialStore for Undeletable {
        fn get(&self, account: &str) -> Result<Option<String>, String> {
            self.0.get(account)
        }
        fn set(&self, account: &str, secret: &str) -> Result<(), String> {
            self.0.set(account, secret)
        }
        fn delete(&self, _: &str) -> Result<bool, String> {
            Err("locked".to_string())
        }
    }

    /// A store that accepts writes and refuses deletes still migrates into the
    /// keychain: there is nothing to delete in that direction.
    #[test]
    fn a_store_that_refuses_deletes_still_migrates_into_the_keychain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let grants = dir.path().join("provider-auth.json");
        write_grant_store(&grants);
        let config = config_with_keys(CredentialStoreKind::File);
        config.save_to_path_public(&path).unwrap();

        apply_migration_with(
            &config,
            &path,
            false,
            Ok(Some(Box::new(Undeletable::default()))),
            None,
            Some(&grants),
        )
        .expect("writing is all this direction needs");
    }

    /// Nobody signed in is not a failed migration.
    #[test]
    fn a_migration_with_no_sign_in_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        assert!(migrate_provider_grants(None, None, None).is_ok());
        assert!(migrate_provider_grants(Some(&dir.path().join("absent.json")), None, None).is_ok());
        assert!(provider_grant_names(None, None).is_empty());
    }

    /// `lev auth status` answers the question a person asks right after
    /// `lev auth login`: which account, and on what plan.
    #[test]
    fn the_report_names_the_signed_in_account_and_plan() {
        let s = Status {
            kind: CredentialStoreKind::File,
            supported: true,
            unavailable: None,
            providers: Vec::new(),
            mcp_servers: Vec::new(),
            oauth_providers: vec![(
                "codex".to_string(),
                "someone@example.com (plus plan)".to_string(),
            )],
            duplicated: Vec::new(),
            config_path: "/x/config.toml".into(),
        };
        let rendered = render_status(&s);
        assert!(rendered.contains("signed in with a browser"), "{rendered}");
        assert!(
            rendered.contains("someone@example.com (plus plan)"),
            "{rendered}"
        );
    }

    /// The summary reads the grant rather than inventing one.
    #[test]
    fn the_summary_comes_from_the_stored_grant() {
        let dir = tempfile::tempdir().unwrap();
        let grants = dir.path().join("provider-auth.json");
        write_grant_store(&grants);
        assert_eq!(provider_grant_names(Some(&grants), None), vec!["codex"]);
    }

    impl AuthArgs {
        /// A `login` invocation, for driving the command end to end.
        fn login_for_test(provider: &str) -> Self {
            Self {
                command: AuthCommand::Login {
                    provider: provider.to_string(),
                },
            }
        }

        /// A `logout` invocation.
        fn logout_for_test(provider: &str) -> Self {
            Self {
                command: AuthCommand::Logout {
                    provider: provider.to_string(),
                },
            }
        }
    }

    /// A stub browser that plays the redirect, so the whole command runs
    /// without launching anything or reaching the real issuer.
    fn stub_browser() -> leviath_mcp::BrowserOpener {
        std::sync::Arc::new(move |url: &str| {
            let parsed = url::Url::parse(url).expect("a URL");
            let pairs: std::collections::HashMap<_, _> =
                parsed.query_pairs().into_owned().collect();
            let redirect = pairs.get("redirect_uri").expect("a redirect").clone();
            let state = pairs.get("state").expect("a state").clone();
            let port: u16 = redirect
                .rsplit(':')
                .next()
                .and_then(|rest| rest.split('/').next())
                .and_then(|p| p.parse().ok())
                .expect("a port");
            tokio::spawn(async move { play_the_redirect(port, &state).await });
            true
        })
    }

    /// Connect to the loopback callback and issue the redirect a browser would.
    ///
    /// Lifted out of the closure so the "nobody is listening" arm can be driven
    /// directly rather than inside a spawned task nothing observes.
    async fn play_the_redirect(port: u16, state: &str) -> bool {
        use tokio::io::AsyncReadExt as _;
        use tokio::io::AsyncWriteExt as _;
        let Ok(mut socket) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
            return false;
        };
        let request = format!("GET /auth/callback?code=c&state={state} HTTP/1.1\r\n\r\n");
        let _ = socket.write_all(request.as_bytes()).await;
        let mut sink = Vec::new();
        let _ = socket.read_to_end(&mut sink).await;
        true
    }

    /// Port one never listens, so the redirect simply does not land.
    #[tokio::test]
    async fn a_redirect_nobody_is_listening_for_goes_nowhere() {
        assert!(!play_the_redirect(1, "st8").await);
    }

    /// The whole command, from the argument to the stored grant.
    ///
    /// The mock issuer is spawned on the same runtime the command runs on: a
    /// server spawned on a runtime that is then dropped stops answering, which
    /// looks exactly like an unreachable issuer.
    #[tokio::test]
    async fn execute_login_signs_in_and_stores_the_grant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();
        let issuer = leviath_testkit::spawn_mock_server(
            200,
            "OK",
            br#"{"access_token":"at-1","refresh_token":"rt-1"}"#.to_vec(),
        )
        .await;

        temp_env::async_with_vars(
            [
                ("LEVIATH_CONFIG_PATH", Some(path.as_os_str())),
                ("LEVIATH_HOME", Some(dir.path().as_os_str())),
            ],
            async {
                execute(
                    AuthArgs::login_for_test("codex"),
                    AuthEnv {
                        opener: stub_browser(),
                        grant_path: leviath_providers::codex::ProviderAuthStore::default_path(),
                        client: reqwest::Client::new(),
                        issuer,
                        // Never the registered ports: a test must not fight the
                        // developer's own Codex CLI for them.
                        ports: vec![0],
                    },
                )
                .await
                .expect("sign-in succeeds");

                let store = leviath_providers::codex::ProviderAuthStore::default_path()
                    .and_then(|p| leviath_providers::codex::ProviderAuthStore::load(&p).ok())
                    .expect("a store");
                assert_eq!(store.get("codex").expect("a grant").refresh_token, "rt-1");
            },
        )
        .await;
    }

    /// And signing out again, which is the other half of the pair.
    #[test]
    fn execute_logout_forgets_the_grant() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();

        temp_env::with_var("LEVIATH_HOME", Some(dir.path()), || {
            let grants =
                leviath_providers::codex::ProviderAuthStore::default_path().expect("a home is set");
            write_grant_store(&grants);

            run_auth(&path, AuthArgs::logout_for_test("codex")).expect("logout succeeds");
            let store =
                leviath_providers::codex::ProviderAuthStore::load(&grants).expect("a store");
            assert!(store.get("codex").is_none());

            // And again, which reports there was nothing to do.
            run_auth(&path, AuthArgs::logout_for_test("codex")).expect("a second logout is fine");
        });
    }

    /// A provider that signs in with a key is refused by both halves.
    #[test]
    fn execute_refuses_a_key_based_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();

        for args in [
            AuthArgs::login_for_test("anthropic"),
            AuthArgs::logout_for_test("anthropic"),
        ] {
            let err = run_auth(&path, args).unwrap_err().to_string();
            assert!(err.contains("does not sign in with a browser"), "{err}");
        }
    }

    /// Signing in to a provider that is already enabled skips the reminder,
    /// which is the other arm of that branch.
    #[tokio::test]
    async fn signing_in_to_an_enabled_provider_says_nothing_extra() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        config.providers.codex_enabled = true;
        config.save_to_path_public(&path).unwrap();
        let issuer = leviath_testkit::spawn_mock_server(
            200,
            "OK",
            br#"{"access_token":"at-1","refresh_token":"rt-1","id_token":"a.b.c"}"#.to_vec(),
        )
        .await;

        temp_env::async_with_vars(
            [
                ("LEVIATH_CONFIG_PATH", Some(path.as_os_str())),
                ("LEVIATH_HOME", Some(dir.path().as_os_str())),
            ],
            async {
                execute(
                    AuthArgs::login_for_test("codex"),
                    AuthEnv {
                        opener: stub_browser(),
                        grant_path: leviath_providers::codex::ProviderAuthStore::default_path(),
                        client: reqwest::Client::new(),
                        issuer,
                        ports: vec![0],
                    },
                )
                .await
                .expect("sign-in succeeds");
            },
        )
        .await;
    }

    /// A machine with nowhere to put the grant says so rather than panicking
    /// or writing somewhere surprising.
    #[tokio::test]
    async fn a_sign_in_with_nowhere_to_store_it_is_refused() {
        let env = AuthEnv {
            opener: std::sync::Arc::new(|_| false),
            grant_path: None,
            client: reqwest::Client::new(),
            issuer: "http://127.0.0.1:1".to_string(),
            ports: vec![0],
        };
        let err = login_with(&Config::default(), "codex", env, Ok(None))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no home directory"), "{err}");

        let err = logout_with("codex", None, Ok(None))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no home directory"), "{err}");
    }

    /// A keychain that cannot be reached is reported rather than silently
    /// falling back to the file, which would put a refresh token where the
    /// user asked for it not to be.
    #[tokio::test]
    async fn an_unavailable_keychain_stops_both_halves() {
        let env = AuthEnv {
            opener: std::sync::Arc::new(|_| false),
            grant_path: Some(std::path::PathBuf::from("/does/not/matter")),
            client: reqwest::Client::new(),
            issuer: "http://127.0.0.1:1".to_string(),
            ports: vec![0],
        };
        let err = login_with(
            &Config::default(),
            "codex",
            env,
            Err("no credential store on this machine".to_string()),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("no credential store"), "{err}");

        let err = logout_with(
            "codex",
            Some(std::path::PathBuf::from("/does/not/matter")),
            Err("no credential store on this machine".to_string()),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no credential store"), "{err}");
    }

    /// A sign-in the issuer refuses is reported rather than stored.
    #[tokio::test]
    async fn a_refused_sign_in_stores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let issuer = leviath_testkit::spawn_mock_server(400, "Bad Request", b"nope".to_vec()).await;
        let env = AuthEnv {
            opener: stub_browser(),
            grant_path: Some(dir.path().join("provider-auth.json")),
            client: reqwest::Client::new(),
            issuer,
            ports: vec![0],
        };
        let err = login_with(&Config::default(), "codex", env, Ok(None))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("400"), "{err}");
        assert!(!dir.path().join("provider-auth.json").exists());
    }

    /// The account and plan are named when the id token carries them, which is
    /// the line a person reads to confirm they signed in as the right one.
    #[tokio::test]
    async fn a_sign_in_names_the_plan_when_the_token_carries_one() {
        let dir = tempfile::tempdir().unwrap();
        let claims = serde_json::json!({
            "email": "someone@example.com",
            "https://api.openai.com/auth": { "chatgpt_plan_type": "plus" },
        })
        .to_string();
        let id_token = format!("aGVhZGVy.{}.c2ln", base64url(claims.as_bytes()));
        let body = serde_json::json!({
            "access_token": "at-1",
            "refresh_token": "rt-1",
            "id_token": id_token,
        });
        let issuer =
            leviath_testkit::spawn_mock_server(200, "OK", body.to_string().into_bytes()).await;

        let grants = dir.path().join("provider-auth.json");
        let env = AuthEnv {
            opener: stub_browser(),
            grant_path: Some(grants.clone()),
            client: reqwest::Client::new(),
            issuer,
            ports: vec![0],
        };
        login_with(&Config::default(), "codex", env, Ok(None))
            .await
            .expect("sign-in succeeds");
        let store = leviath_providers::codex::ProviderAuthStore::load(&grants).expect("a store");
        assert_eq!(
            store.get("codex").expect("a grant").plan_type.as_deref(),
            Some("plus")
        );
    }

    /// Base64url, no padding. `base64` is not a CLI dependency and adding one
    /// for a test fixture is a poor trade.
    fn base64url(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let mut buf = [0u8; 3];
            buf[..chunk.len()].copy_from_slice(chunk);
            let n = u32::from(buf[0]) << 16 | u32::from(buf[1]) << 8 | u32::from(buf[2]);
            for i in 0..chunk.len() + 1 {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            }
        }
        out
    }

    /// The real environment resolves this machine's paths and the real issuer.
    #[test]
    fn the_real_environment_points_at_this_machine() {
        let env = AuthEnv::real();
        assert_eq!(env.issuer, leviath_providers::codex::ISSUER);
        assert_eq!(env.ports, leviath_providers::codex::CALLBACK_PORTS.to_vec());
        assert!(env.grant_path.is_some(), "a home resolves in a test run");
    }

    /// A config the loader cannot read stops both halves before anything else.
    #[test]
    fn a_broken_config_stops_the_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not = = toml").unwrap();
        for args in [
            AuthArgs::login_for_test("codex"),
            AuthArgs::logout_for_test("codex"),
        ] {
            assert!(run_auth(&path, args).is_err());
        }
    }

    /// A grant file that cannot be read stops the sign-out rather than
    /// reporting that nothing was there.
    #[test]
    fn a_corrupt_grant_file_fails_the_sign_out() {
        let dir = tempfile::tempdir().unwrap();
        let grants = dir.path().join("provider-auth.json");
        std::fs::write(&grants, "{ not json").unwrap();
        assert!(logout_with("codex", Some(grants), Ok(None)).is_err());
    }

    /// The report reads the grant rather than inventing one, and falls back
    /// through the id token when the stored fields are absent.
    #[test]
    fn the_summaries_read_the_stored_grant() {
        let dir = tempfile::tempdir().unwrap();
        temp_env::with_var("LEVIATH_HOME", Some(dir.path()), || {
            let grants =
                leviath_providers::codex::ProviderAuthStore::default_path().expect("a home is set");

            // Nothing signed in yet, and nowhere to look at all.
            assert!(oauth_provider_summaries(Some(&grants), None).is_empty());
            assert!(oauth_provider_summaries(None, None).is_empty());

            // The stored fields win when they are there.
            write_grant_store(&grants);
            assert_eq!(
                oauth_provider_summaries(Some(&grants), None),
                vec![(
                    "codex".to_string(),
                    "someone@example.com (plus plan)".to_string()
                )]
            );

            // With none of them, the id token answers instead.
            let claims = serde_json::json!({
                "email": "from-token@example.com",
                "https://api.openai.com/auth": { "chatgpt_plan_type": "pro" },
            })
            .to_string();
            let mut store = leviath_providers::codex::ProviderAuthStore::default();
            store.set(
                "codex",
                leviath_providers::ProviderGrant {
                    access_token: "at".to_string(),
                    refresh_token: "rt".to_string(),
                    id_token: format!("aGVhZGVy.{}.c2ln", base64url(claims.as_bytes())),
                    ..Default::default()
                },
            );
            store.save(&grants).unwrap();
            assert_eq!(
                oauth_provider_summaries(Some(&grants), None),
                vec![(
                    "codex".to_string(),
                    "from-token@example.com (pro plan)".to_string()
                )]
            );

            // And with no account at all, the name alone.
            let mut store = leviath_providers::codex::ProviderAuthStore::default();
            store.set(
                "codex",
                leviath_providers::ProviderGrant {
                    access_token: "at".to_string(),
                    refresh_token: "rt".to_string(),
                    ..Default::default()
                },
            );
            store.save(&grants).unwrap();
            assert_eq!(
                oauth_provider_summaries(Some(&grants), None),
                vec![("codex".to_string(), "signed in".to_string())]
            );

            // A file that will not parse reports nothing rather than failing:
            // this is the command someone runs because something is wrong.
            std::fs::write(&grants, "{ not json").unwrap();
            assert!(oauth_provider_summaries(Some(&grants), None).is_empty());
        });
    }

    /// A provider that signs in with a key is told where to go instead.
    #[test]
    fn a_key_based_provider_is_refused_with_the_alternative() {
        let refusal = not_an_oauth_provider("anthropic").to_string();
        // Named needles rather than printing the refusal itself. The message
        // comes out of an `oauth`-named function, and CodeQL's
        // cleartext-logging rule reads any value from one of those reaching a
        // format argument as a leaked secret.
        for needle in ["anthropic", "codex", "lev setup"] {
            assert!(refusal.contains(needle), "refusal never mentions {needle}");
        }
    }
}
