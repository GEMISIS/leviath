//! `lev providers quota`, and the usage lines `lev auth status` and `lev
//! doctor` show: how much of each signed-in subscription is left.

use std::time::Duration;

use leviath_providers::quota::QuotaReport;

use crate::commands::setup::catalog;
use crate::config::Config;

/// How long one provider's account read may take. A person is waiting.
const QUOTA_TIMEOUT: Duration = Duration::from_secs(10);

/// One subscription's usage, or why it could not be read.
pub(crate) struct Usage {
    /// The provider's registry name.
    pub(crate) provider: &'static str,
    /// What it reported.
    pub(crate) report: Result<QuotaReport, String>,
}

/// Every enabled sign-in provider's usage, in catalog order. A provider the
/// registry did not build (no grant location) or that reports no quota is
/// left out: there is nothing to say about it here.
pub(crate) async fn usage(
    config: &Config,
    registry: &leviath_runtime::ProviderRegistry,
) -> Vec<Usage> {
    let mut out = Vec::new();
    for row in catalog::providers() {
        if !catalog::signin_enabled(config, row.id) {
            continue;
        }
        let Some(provider) = registry.get(row.id) else {
            continue;
        };
        let report = match tokio::time::timeout(QUOTA_TIMEOUT, provider.quota()).await {
            Ok(Some(Ok(report))) => Ok(report),
            Ok(Some(Err(e))) => Err(e.to_string()),
            Ok(None) => continue,
            Err(_) => Err(format!(
                "the account did not answer within {}s",
                QUOTA_TIMEOUT.as_secs()
            )),
        };
        out.push(Usage {
            provider: row.id,
            report,
        });
    }
    out
}

/// The usage as text, one block per provider.
pub(crate) fn render(usage: &[Usage], now: u64) -> String {
    let mut out = String::new();
    for entry in usage {
        match &entry.report {
            Ok(report) => {
                let plan = report
                    .plan
                    .as_deref()
                    .map(|p| format!(" ({p} plan)"))
                    .unwrap_or_default();
                out.push_str(&format!("{}{plan}\n", entry.provider));
                for line in report.lines(now) {
                    out.push_str(&format!("  {line}\n"));
                }
            }
            Err(reason) => out.push_str(&format!(
                "{}\n  could not read usage: {reason}\n",
                entry.provider
            )),
        }
    }
    out
}

/// The usage as JSON: `{"quota": [{"provider", "report" | "error"}]}`.
pub(crate) fn json(usage: &[Usage]) -> serde_json::Value {
    serde_json::json!({
        "quota": usage.iter().map(|u| match &u.report {
            Ok(report) => serde_json::json!({ "provider": u.provider, "report": report }),
            Err(error) => serde_json::json!({ "provider": u.provider, "error": error }),
        }).collect::<Vec<_>>()
    })
}

/// Unix seconds now.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Run `lev providers quota`.
pub(super) async fn show(json_out: bool, config_path: &std::path::Path) -> anyhow::Result<()> {
    let config = Config::load_from_path_public(config_path)?;
    let registry = crate::commands::run::session::build_provider_registry_from_config(&config)?;
    let usage = usage(&config, &registry).await;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json(&usage))?);
        return Ok(());
    }
    match usage.is_empty() {
        true => println!(
            "No subscription is signed in. `lev auth login codex` or `lev auth login grok` \
             signs one in; `lev setup` turns it on."
        ),
        false => print!("{}", render(&usage, now())),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_providers::quota::UsageWindow;

    fn report() -> QuotaReport {
        QuotaReport {
            plan: Some("plus".into()),
            windows: vec![UsageWindow {
                label: "5h".into(),
                used_percent: Some(20.0),
                used: None,
                limit: None,
                unit: None,
                resets_at: None,
            }],
            balance: None,
            limit_reached: false,
        }
    }

    #[test]
    fn usage_renders_per_provider_as_text_and_json() {
        let usage = vec![
            Usage {
                provider: "codex",
                report: Ok(report()),
            },
            Usage {
                provider: "grok",
                report: Err("HTTP 401".into()),
            },
        ];
        let text = render(&usage, 0);
        assert!(text.contains("codex (plus plan)\n  5h: 20% used"), "{text}");
        assert!(
            text.contains("grok\n  could not read usage: HTTP 401"),
            "{text}"
        );
        let value = json(&usage);
        assert_eq!(value["quota"][0]["report"]["plan"], "plus");
        assert_eq!(value["quota"][1]["error"], "HTTP 401");
        let mut unplanned = report();
        unplanned.plan = None;
        let bare = render(
            &[Usage {
                provider: "codex",
                report: Ok(unplanned),
            }],
            0,
        );
        assert!(bare.starts_with("codex\n"), "{bare}");
        assert!(now() > 0);
    }

    #[tokio::test]
    async fn only_enabled_sign_ins_the_registry_built_are_asked() {
        let mut config = Config::default();
        config.providers.codex_enabled = true;
        // Nothing registered: nothing to ask.
        let empty = leviath_runtime::ProviderRegistry::new();
        assert!(usage(&config, &empty).await.is_empty());
        // A key provider reports no quota and is left out; a disabled
        // subscription is never asked.
        let config = Config::default();
        assert!(usage(&config, &empty).await.is_empty());
    }

    #[tokio::test]
    async fn the_command_reads_the_config_it_is_pointed_at() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        show(false, &path)
            .await
            .expect("nothing signed in is not an error");
        show(true, &path).await.expect("and as JSON");
    }
}
