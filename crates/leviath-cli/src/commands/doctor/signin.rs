//! The doctor's checks on browser sign-in providers: whether each one that is
//! switched on is signed in, and whether its subscription is over a limit.

use super::{Check, Config};

/// Whether each enabled browser sign-in provider can actually answer.
///
/// Nothing for a provider that is not enabled, so the report says nothing
/// about one nobody asked for. The failure this exists to name is "enabled but
/// never signed in": without it the first inference fails with a bare HTTP 401
/// and nothing pointing at the one command that fixes it.
pub(super) fn signin_checks(config: &Config) -> Vec<Check> {
    crate::commands::setup::catalog::providers()
        .into_iter()
        .filter(|p| crate::commands::setup::catalog::signin_enabled(config, p.id))
        .map(|p| signin_check(p.id))
        .collect()
}

/// A warning for each signed-in subscription that is over a usage limit,
/// since every run on it is refused until the limit resets. Nothing when a
/// subscription is within its limits or its usage could not be read: the
/// sign-in check above already speaks for the account.
pub(super) async fn quota_checks(
    config: &Config,
    registry: &leviath_runtime::ProviderRegistry,
) -> Vec<Check> {
    let now = crate::commands::providers::quota::now();
    crate::commands::providers::quota::usage(config, registry)
        .await
        .into_iter()
        .filter_map(|usage| {
            let report = usage.report.ok()?;
            report.limit_reached.then(|| {
                Check::warn(
                    usage.provider,
                    format!(
                        "over a usage limit; runs on it are refused until it resets ({})",
                        report.lines(now).join("; ")
                    ),
                )
            })
        })
        .collect()
}

/// The check for one enabled sign-in provider.
pub(super) fn signin_check(id: &'static str) -> Check {
    let grant = leviath_providers::oauth::ProviderAuthStore::default_path()
        .and_then(|path| leviath_providers::oauth::ProviderAuthStore::load(&path).ok())
        .and_then(|store| store.get(id).cloned());
    let account = leviath_providers::oauth::profile(id).map_or(id, |p| p.brand);
    match grant {
        None => Check::warn(
            id,
            format!("enabled but not signed in; run `lev auth login {id}`"),
        ),
        Some(grant) => {
            let claims = grant.claims();
            let who = grant
                .email
                .or(claims.email)
                .unwrap_or_else(|| "signed in".to_string());
            match grant.plan_type.or(claims.plan_type) {
                Some(plan) => Check::ok(id, format!("{who} ({account} {plan} plan)")),
                None => Check::ok(id, who),
            }
        }
    }
}
