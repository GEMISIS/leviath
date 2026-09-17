//! A Grok subscription's account routes: what the plan has used, which
//! reasoning efforts each model takes, and the account's retention setting.
//!
//! They live on the Grok CLI's own host (`cli-chat-proxy.grok.com`), not on
//! the API. Measured 2026-09-16 with a live sign-in: each of these answers a
//! plain `GET` with the bearer and no client headers, and none of them is
//! inference, so Leviath identifies as itself here too.
//!
//! - `GET /v1/billing?format=credits` carries the current weekly period
//!   (`currentPeriod.start`/`end`), `onDemandCap` and `onDemandUsed`, and
//!   `prepaidBalance`, each as `{"val": n}`.
//! - `GET /v1/billing` carries the calendar month: `monthlyLimit`, `used`,
//!   `billingPeriodEnd`.
//! - `GET /v1/models-v2` carries each model's `reasoning_efforts` list.
//! - `GET /v1/user` carries `codingDataRetentionOptOut`.
//!
//! xAI documents none of this, so every reader is lenient: a field it cannot
//! read is left out, never guessed.

use std::collections::HashMap;

use serde_json::Value;

use crate::quota::{QuotaReport, UsageWindow};

/// The `val` of a `{"val": n}` amount at `key` under `node`.
fn val(node: &Value, key: &str) -> Option<f64> {
    node.get(key)?.get("val")?.as_f64()
}

/// Unix seconds for an RFC 3339 timestamp with a fractional part and an
/// offset, the form these routes write (`2026-09-23T22:25:42.311757+00:00`).
/// Only the `+00:00` and `Z` offsets are read: anything else is `None`,
/// since a reset time a little wrong is worse than none.
fn unix_seconds(text: &str) -> Option<u64> {
    let (stamp, offset_ok) = match text.strip_suffix("+00:00") {
        Some(stamp) => (stamp, true),
        None => (text.strip_suffix('Z').unwrap_or(text), text.ends_with('Z')),
    };
    if !offset_ok {
        return None;
    }
    let whole = stamp.split('.').next().unwrap_or(stamp);
    crate::learned::unix_seconds_from_rfc3339(&format!("{whole}Z")).map(|s| s as u64)
}

/// The quota a Grok subscription reports, from the two billing reads. Either
/// may be absent; with neither there is nothing to report.
pub(crate) fn quota(credits: Option<&Value>, monthly: Option<&Value>) -> Option<QuotaReport> {
    let mut report = QuotaReport::default();
    if let Some(config) = credits.and_then(|b| b.get("config")) {
        report.windows.push(UsageWindow {
            label: "this week (on demand)".to_string(),
            used_percent: None,
            used: val(config, "onDemandUsed"),
            limit: val(config, "onDemandCap"),
            unit: Some("credits".to_string()),
            resets_at: config
                .pointer("/currentPeriod/end")
                .and_then(Value::as_str)
                .and_then(unix_seconds),
        });
        report.balance = val(config, "prepaidBalance").map(|b| format!("{b} prepaid credits"));
    }
    if let Some(config) = monthly.and_then(|b| b.get("config")) {
        report.windows.push(UsageWindow {
            label: "this month".to_string(),
            used_percent: None,
            used: val(config, "used"),
            limit: val(config, "monthlyLimit").filter(|l| *l > 0.0),
            unit: Some("credits".to_string()),
            resets_at: config
                .get("billingPeriodEnd")
                .and_then(Value::as_str)
                .and_then(unix_seconds),
        });
    }
    report.limit_reached = report.windows.iter().any(
        |w| matches!((w.used, w.limit), (Some(used), Some(limit)) if limit > 0.0 && used >= limit),
    );
    (!report.windows.is_empty()).then_some(report)
}

/// Each model's reasoning efforts, from `models-v2`, by id.
pub(crate) fn efforts(body: &Value) -> HashMap<String, Vec<String>> {
    body.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_string();
            let efforts = match entry
                .get("supports_reasoning_effort")
                .and_then(Value::as_bool)
            {
                Some(false) => Vec::new(),
                _ => entry
                    .get("reasoning_efforts")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.get("value").or_else(|| e.get("id"))?.as_str())
                    .map(str::to_string)
                    .collect(),
            };
            Some((id, efforts))
        })
        .collect()
}

/// Whether the account has opted out of coding data retention, from
/// `/v1/user`. `None` when the field is absent.
pub(crate) fn retention_opt_out(body: &Value) -> Option<bool> {
    body.get("codingDataRetentionOptOut")?.as_bool()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The bodies measured on a live SuperGrok account, trimmed.
    fn credits() -> Value {
        json!({ "config": {
            "currentPeriod": { "type": "USAGE_PERIOD_TYPE_WEEKLY",
                "start": "2026-09-16T22:25:42.311757+00:00", "end": "2026-09-23T22:25:42.311757+00:00" },
            "onDemandCap": { "val": 50 }, "onDemandUsed": { "val": 12 },
            "prepaidBalance": { "val": 0 }
        }})
    }

    fn monthly() -> Value {
        json!({ "config": {
            "monthlyLimit": { "val": 0 }, "used": { "val": 3 },
            "billingPeriodEnd": "2026-10-01T00:00:00+00:00"
        }})
    }

    #[test]
    fn both_billing_reads_become_windows_with_their_resets() {
        let report = quota(Some(&credits()), Some(&monthly())).expect("a report");
        assert_eq!(report.windows.len(), 2);
        let week = &report.windows[0];
        assert_eq!((week.used, week.limit), (Some(12.0), Some(50.0)));
        assert_eq!(week.resets_at, Some(1_790_202_342));
        let month = &report.windows[1];
        assert_eq!(month.limit, None, "a zero monthly limit is no limit");
        assert_eq!(month.resets_at, Some(1_790_812_800));
        assert_eq!(report.balance.as_deref(), Some("0 prepaid credits"));
        assert!(!report.limit_reached);
    }

    #[test]
    fn a_spent_cap_is_a_reached_limit() {
        let spent =
            json!({ "config": { "onDemandCap": { "val": 5 }, "onDemandUsed": { "val": 5 } } });
        let report = quota(Some(&spent), None).unwrap();
        assert!(report.limit_reached);
        assert_eq!(report.windows[0].resets_at, None);
        assert_eq!(report.balance, None);
    }

    #[test]
    fn nothing_readable_is_no_report() {
        assert_eq!(quota(None, None), None);
        assert_eq!(quota(Some(&json!({})), Some(&json!("no"))), None);
    }

    #[test]
    fn only_utc_timestamps_are_read() {
        assert_eq!(unix_seconds("2026-10-01T00:00:00Z"), Some(1_790_812_800));
        assert_eq!(
            unix_seconds("2026-10-01T00:00:00.5+00:00"),
            Some(1_790_812_800)
        );
        assert_eq!(unix_seconds("2026-10-01T00:00:00+02:00"), None);
        assert_eq!(unix_seconds("not a time+00:00"), None);
    }

    #[test]
    fn efforts_are_read_per_model_and_a_model_without_them_has_none() {
        let body = json!({ "data": [
            { "id": "grok-4.6", "supports_reasoning_effort": true,
              "reasoning_efforts": [ { "id": "xhigh", "value": "xhigh" }, { "id": "low" } ] },
            { "id": "grok-build", "supports_reasoning_effort": false,
              "reasoning_efforts": [ { "value": "high" } ] },
            { "id": "grok-4.3" },
            { "reasoning_efforts": [] }
        ]});
        let efforts = efforts(&body);
        assert_eq!(efforts["grok-4.6"], ["xhigh", "low"]);
        assert!(efforts["grok-build"].is_empty());
        assert!(efforts["grok-4.3"].is_empty());
        assert_eq!(efforts.len(), 3);
    }

    #[test]
    fn the_retention_flag_is_read_when_present() {
        assert_eq!(
            retention_opt_out(&json!({ "codingDataRetentionOptOut": true })),
            Some(true)
        );
        assert_eq!(retention_opt_out(&json!({})), None);
    }
}
