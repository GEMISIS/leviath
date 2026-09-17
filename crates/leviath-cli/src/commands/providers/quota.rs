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
    usage_within(config, registry, QUOTA_TIMEOUT).await
}

/// [`usage`] with the bound named, for a caller with a page open rather than a
/// person at a terminal.
///
/// The accounts are asked side by side, so a signed-in subscription that will
/// not answer costs one `timeout` for the whole read rather than one each. That
/// is `join_all` and not a `JoinSet` on purpose: it answers in the order it was
/// given, which is catalog order, so nothing has to be tagged and re-sorted;
/// it needs no `'static`, so the borrowed registry stays borrowed and every
/// caller's signature is unchanged; and it adds no "this account's read
/// panicked" arm, which for an HTTP call would be a branch no test could reach.
pub(crate) async fn usage_within(
    config: &Config,
    registry: &leviath_runtime::ProviderRegistry,
    timeout: Duration,
) -> Vec<Usage> {
    let asking = catalog::providers()
        .into_iter()
        .filter(|row| catalog::signin_enabled(config, row.id))
        .filter_map(|row| registry.get(row.id).map(|provider| (row.id, provider)))
        .map(|(provider_id, provider)| async move {
            (
                provider_id,
                tokio::time::timeout(timeout, provider.quota()).await,
            )
        });
    futures_util::future::join_all(asking)
        .await
        .into_iter()
        .filter_map(|(provider, read)| {
            let report = match read {
                Ok(Some(Ok(report))) => Ok(report),
                Ok(Some(Err(e))) => Err(e.to_string()),
                // Nothing to say: a key-billed provider, or a subscription
                // whose plan reports no windows. Left out rather than reported
                // as an empty one.
                Ok(None) => return None,
                Err(_) => Err(format!(
                    "the account did not answer within {}s",
                    timeout.as_secs()
                )),
            };
            Some(Usage { provider, report })
        })
        .collect()
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
        "quota": usage.iter().map(|u| {
            let mut value = entry(u);
            value["provider"] = serde_json::json!(u.provider);
            value
        }).collect::<Vec<_>>()
    })
}

/// One subscription's usage as JSON: `{"report": ...}` or `{"error": ...}`.
pub(crate) fn entry(usage: &Usage) -> serde_json::Value {
    match &usage.report {
        Ok(report) => serde_json::json!({ "report": report }),
        Err(error) => serde_json::json!({ "error": error }),
    }
}

/// Unix seconds now.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What `lev providers quota` prints for `usage`.
pub(crate) fn report_text(usage: &[Usage], json_out: bool, now: u64) -> String {
    match (json_out, usage.is_empty()) {
        (true, _) => format!(
            "{}\n",
            serde_json::to_string_pretty(&json(usage)).expect("a JSON value serialises")
        ),
        (false, true) => "No subscription is signed in. `lev auth login codex` or `lev auth \
                          login grok` signs one in; `lev setup` turns it on.\n"
            .to_string(),
        (false, false) => render(usage, now),
    }
}

/// The usage block `lev auth status` adds, or nothing when no subscription
/// is signed in.
pub(crate) fn section(usage: &[Usage], now: u64) -> String {
    match usage.is_empty() {
        true => String::new(),
        false => format!("\nSubscription usage:\n{}", render(usage, now)),
    }
}

/// The registry a quota read asks: every provider the config builds, or
/// none when a client cannot be built, which reads as nothing signed in.
pub(crate) fn registry_for(config: &Config) -> leviath_runtime::ProviderRegistry {
    crate::commands::run::session::build_provider_registry_from_config(config).unwrap_or_default()
}

/// Run `lev providers quota`.
pub(super) async fn show(json_out: bool, config_path: &std::path::Path) -> anyhow::Result<()> {
    let config = Config::load_from_path_public(config_path)?;
    let usage = usage(&config, &registry_for(&config)).await;
    print!("{}", report_text(&usage, json_out, now()));
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

    #[tokio::test]
    async fn each_enabled_subscription_answers_or_says_why_not() {
        use crate::test_fixtures::{QuotaAnswer, Subscription, quota_report, subscriptions};
        let mut config = Config::default();
        config.providers.codex_enabled = true;
        config.providers.grok_enabled = true;
        let registry = subscriptions(vec![
            Subscription {
                name: "codex",
                answer: QuotaAnswer::Report(quota_report(false)),
            },
            Subscription {
                name: "grok",
                answer: QuotaAnswer::Fails("HTTP 401".into()),
            },
        ]);
        let read = usage(&config, &registry).await;
        assert_eq!(read.len(), 2);
        assert!(read[0].report.is_ok());
        assert_eq!(read[1].report.as_ref().unwrap_err(), "HTTP 401");
        assert!(report_text(&read, false, 0).contains("codex (plus plan)"));
        assert!(report_text(&read, true, 0).contains("\"quota\""));
        assert!(report_text(&[], false, 0).contains("No subscription is signed in"));
        assert!(section(&read, 0).starts_with("\nSubscription usage:\ncodex"));
        assert_eq!(section(&[], 0), "");
        let _ = registry_for(&Config::default());
    }

    #[tokio::test(start_paused = true)]
    async fn a_subscription_with_nothing_to_say_is_left_out_and_a_silent_one_times_out() {
        use crate::test_fixtures::{QuotaAnswer, Subscription, subscriptions};
        let mut config = Config::default();
        config.providers.codex_enabled = true;
        config.providers.grok_enabled = true;
        let registry = subscriptions(vec![
            Subscription {
                name: "codex",
                answer: QuotaAnswer::Nothing,
            },
            Subscription {
                name: "grok",
                answer: QuotaAnswer::Hangs,
            },
        ]);
        let read = usage(&config, &registry).await;
        assert_eq!(read.len(), 1);
        assert!(
            read[0]
                .report
                .as_ref()
                .unwrap_err()
                .contains("did not answer")
        );
    }

    /// The whole read is bounded by one timeout, not by one per subscription.
    ///
    /// This is the regression test for the shape the loop used to have: an
    /// `await` per provider inside a `for`, which cost ten seconds for the
    /// first silent account and ten more for the second, on a console's
    /// providers page.
    #[tokio::test(start_paused = true)]
    async fn two_silent_accounts_are_given_up_on_together_rather_than_one_after_the_other() {
        use crate::test_fixtures::{QuotaAnswer, Subscription, subscriptions};
        let mut config = Config::default();
        config.providers.codex_enabled = true;
        config.providers.grok_enabled = true;
        let registry = subscriptions(vec![
            Subscription {
                name: "codex",
                answer: QuotaAnswer::Hangs,
            },
            Subscription {
                name: "grok",
                answer: QuotaAnswer::Hangs,
            },
        ]);
        let started = tokio::time::Instant::now();
        let read = usage(&config, &registry).await;
        assert_eq!(read.len(), 2);
        assert!(
            started.elapsed() < QUOTA_TIMEOUT * 2,
            "asked one after the other"
        );
        for entry in &read {
            assert_eq!(
                entry.report.as_ref().unwrap_err(),
                "the account did not answer within 10s"
            );
        }
    }

    /// A caller that names its own bound gets it, in the wait and in the words.
    #[tokio::test(start_paused = true)]
    async fn a_named_bound_is_the_one_waited_and_the_one_reported() {
        use crate::test_fixtures::{QuotaAnswer, Subscription, subscriptions};
        let mut config = Config::default();
        config.providers.grok_enabled = true;
        let registry = subscriptions(vec![Subscription {
            name: "grok",
            answer: QuotaAnswer::Hangs,
        }]);
        let read = usage_within(&config, &registry, Duration::from_secs(1)).await;
        assert_eq!(
            read[0].report.as_ref().unwrap_err(),
            "the account did not answer within 1s"
        );
    }

    #[tokio::test]
    async fn a_config_that_does_not_parse_is_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not = [toml").unwrap();
        assert!(show(false, &path).await.is_err());
    }

    #[tokio::test]
    async fn the_fake_subscription_keeps_its_trait_obligations() {
        use leviath_providers::Provider;
        let fake = crate::test_fixtures::Subscription {
            name: "codex",
            answer: crate::test_fixtures::QuotaAnswer::Nothing,
        };
        let request = leviath_providers::InferenceRequest {
            system: vec![],
            messages: vec![],
            model: "m".into(),
            max_tokens: 1,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        assert!(fake.infer(&request).await.is_err());
        assert_eq!(fake.count_tokens("", "").await, 1);
        assert_eq!(fake.max_context_tokens(""), 1);
        assert_eq!(fake.name(), "codex");
        let _ = fake.capabilities("");
    }
}
