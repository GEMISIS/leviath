//! How much of a subscription is left.
//!
//! A subscription has no per-call price, so what answers "how much is left" is
//! a usage window: a percentage of a rolling limit (Codex), or an amount spent
//! against a cap in a billing period (Grok). One shape covers both so every
//! place that shows quota (`lev auth status`, `lev providers quota`, `lev
//! doctor`, `GET /api/providers`) draws it the same way.

use serde::Serialize;

/// One usage window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageWindow {
    /// What the window is, for a person: `5h`, `week`, `this week`,
    /// `this month`.
    pub label: String,
    /// How much of it is spent, 0 to 100, when the provider reports a share.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    /// How much is spent, in [`Self::unit`], when the provider reports an
    /// amount.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    /// The cap on [`Self::used`], when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    /// What [`Self::used`] and [`Self::limit`] count, as the provider names it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// When the window resets, as Unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
}

impl UsageWindow {
    /// One line describing the window, given the time `now`.
    pub fn describe(&self, now: u64) -> String {
        let spent = match (self.used_percent, self.used, self.limit) {
            (Some(percent), _, _) => format!("{percent:.0}% used"),
            (None, Some(used), Some(limit)) => {
                format!(
                    "{} of {} used",
                    amount(used, &self.unit),
                    amount(limit, &self.unit)
                )
            }
            (None, Some(used), None) => format!("{} used", amount(used, &self.unit)),
            (None, None, _) => "no usage reported".to_string(),
        };
        match self.resets_at.map(|at| at.saturating_sub(now)) {
            Some(secs) => format!("{}: {spent}, resets in {}", self.label, duration(secs)),
            None => format!("{}: {spent}", self.label),
        }
    }
}

/// An amount with its unit: `$1.25` for dollars, `12 credits` otherwise.
fn amount(value: f64, unit: &Option<String>) -> String {
    match unit.as_deref() {
        Some("usd") => format!("${value:.2}"),
        Some(other) => format!("{} {other}", trimmed(value)),
        None => trimmed(value),
    }
}

/// A number without a trailing `.0` when it is whole.
fn trimmed(value: f64) -> String {
    match value.fract() == 0.0 {
        true => format!("{value:.0}"),
        false => format!("{value:.2}"),
    }
}

/// Seconds as the largest sensible unit: `3d`, `5h`, `12m`, `40s`.
fn duration(secs: u64) -> String {
    match secs {
        s if s >= 86_400 => format!("{}d", s / 86_400),
        s if s >= 3_600 => format!("{}h", s / 3_600),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// What a subscription has left.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct QuotaReport {
    /// The plan the account is on, when the provider says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Every window the provider reports, shortest first.
    pub windows: Vec<UsageWindow>,
    /// A prepaid or top-up balance, as the provider writes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
    /// Whether the account is over a limit right now.
    pub limit_reached: bool,
}

impl QuotaReport {
    /// Seconds until the soonest window resets, given `now`: what a 429 with
    /// no `Retry-After` should wait.
    pub fn resets_in(&self, now: u64) -> Option<u64> {
        self.windows
            .iter()
            .filter_map(|w| w.resets_at.map(|at| at.saturating_sub(now)))
            .min()
    }

    /// One line per window, then the balance and whether a limit is reached.
    pub fn lines(&self, now: u64) -> Vec<String> {
        let mut lines: Vec<String> = self.windows.iter().map(|w| w.describe(now)).collect();
        if let Some(balance) = &self.balance {
            lines.push(format!("balance: {balance}"));
        }
        if self.limit_reached {
            lines.push("a usage limit is reached; requests are refused until it resets".into());
        }
        if lines.is_empty() {
            lines.push("no usage reported".to_string());
        }
        lines
    }
}

impl From<&crate::codex::Quota> for QuotaReport {
    fn from(quota: &crate::codex::Quota) -> Self {
        Self {
            plan: quota.plan_type.clone(),
            windows: [quota.primary, quota.secondary]
                .into_iter()
                .flatten()
                .map(|w| UsageWindow {
                    label: w.label(),
                    used_percent: Some(w.used_percent),
                    used: None,
                    limit: None,
                    unit: None,
                    resets_at: w.reset_at,
                })
                .collect(),
            balance: quota.credit_balance.clone(),
            limit_reached: quota.limit_reached,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(used_percent: Option<f64>, used: Option<f64>, limit: Option<f64>) -> UsageWindow {
        UsageWindow {
            label: "this week".into(),
            used_percent,
            used,
            limit,
            unit: None,
            resets_at: Some(1_000 + 3 * 86_400),
        }
    }

    #[test]
    fn a_window_reads_as_a_share_an_amount_or_nothing() {
        assert_eq!(
            window(Some(42.4), None, None).describe(1_000),
            "this week: 42% used, resets in 3d"
        );
        let mut dollars = window(None, Some(1.5), Some(20.0));
        dollars.unit = Some("usd".into());
        dollars.resets_at = Some(1_000 + 5 * 3_600);
        assert_eq!(
            dollars.describe(1_000),
            "this week: $1.50 of $20.00 used, resets in 5h"
        );
        let mut credits = window(None, Some(12.0), None);
        credits.unit = Some("credits".into());
        credits.resets_at = Some(1_000 + 120);
        assert_eq!(
            credits.describe(1_000),
            "this week: 12 credits used, resets in 2m"
        );
        let mut bare = window(None, Some(2.25), Some(3.0));
        bare.resets_at = Some(1_030);
        assert_eq!(
            bare.describe(1_000),
            "this week: 2.25 of 3 used, resets in 30s"
        );
        let mut empty = window(None, None, None);
        empty.resets_at = None;
        assert_eq!(empty.describe(0), "this week: no usage reported");
    }

    #[test]
    fn a_report_names_its_soonest_reset_balance_and_limit() {
        let report = QuotaReport {
            plan: Some("plus".into()),
            windows: vec![window(Some(10.0), None, None), {
                let mut w = window(Some(90.0), None, None);
                w.resets_at = Some(1_060);
                w
            }],
            balance: Some("4.00".into()),
            limit_reached: true,
        };
        assert_eq!(report.resets_in(1_000), Some(60));
        let lines = report.lines(1_000);
        assert_eq!(lines.len(), 4);
        assert!(lines[2].contains("balance: 4.00"));
        assert!(lines[3].contains("limit is reached"));
        assert_eq!(QuotaReport::default().lines(0), ["no usage reported"]);
        assert_eq!(QuotaReport::default().resets_in(0), None);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["windows"][0]["used_percent"], 10.0);
        assert!(json["windows"][0].get("used").is_none());
    }

    #[test]
    fn a_codex_quota_becomes_percentage_windows() {
        let quota = crate::codex::Quota {
            plan_type: Some("plus".into()),
            limit_reached: false,
            primary: Some(crate::codex::QuotaWindow {
                window_secs: 18_000,
                used_percent: 12.0,
                reset_at: Some(5),
            }),
            secondary: None,
            credit_balance: None,
        };
        let report = QuotaReport::from(&quota);
        assert_eq!(report.plan.as_deref(), Some("plus"));
        assert_eq!(report.windows.len(), 1);
        assert_eq!(report.windows[0].label, "5h");
        assert_eq!(report.windows[0].used_percent, Some(12.0));
    }
}
