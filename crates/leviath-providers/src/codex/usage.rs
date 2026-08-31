//! The subscription's quota windows.
//!
//! A subscription has no per-call price, so the number that answers "how much
//! is left" is not a dollar figure, it is a percentage of a rolling window.
//! Measured on a Plus account: a five-hour primary window and a seven-day
//! secondary one, both reported as `used_percent` with a reset timestamp.
//!
//! The highest-value use is a 429 with no `Retry-After`: the window's reset is
//! the real answer, and without it the retry loop guesses with exponential
//! backoff against a limit that resets on a wall clock.

use serde::Deserialize;

/// One rolling limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuotaWindow {
    /// How long the window is.
    pub window_secs: u64,
    /// How much of it is spent, 0 to 100.
    pub used_percent: f64,
    /// When it resets, as a Unix timestamp.
    pub reset_at: Option<u64>,
}

impl QuotaWindow {
    /// A human label for the window's length.
    pub fn label(&self) -> String {
        match self.window_secs {
            0 => "window".to_string(),
            s if s >= 604_800 => "week".to_string(),
            s if s >= 86_400 => format!("{}d", s / 86_400),
            s => format!("{}h", (s / 3600).max(1)),
        }
    }

    /// Seconds until this window resets, given `now`.
    pub fn resets_in(&self, now: u64) -> Option<u64> {
        self.reset_at.map(|at| at.saturating_sub(now))
    }
}

/// What the subscription has left.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Quota {
    /// The plan tier the server reports, which can differ from the id token's
    /// if the subscription changed since sign-in.
    pub plan_type: Option<String>,
    /// Whether the account is currently over a limit.
    pub limit_reached: bool,
    /// The shorter, faster-moving window.
    pub primary: Option<QuotaWindow>,
    /// The longer one.
    pub secondary: Option<QuotaWindow>,
    /// Top-up credit balance, when the account has one.
    pub credit_balance: Option<String>,
}

impl Quota {
    /// Seconds until the soonest window resets.
    ///
    /// What a 429 with no `Retry-After` should wait. The nearer of the two,
    /// because that is the one that will let a request through first.
    pub fn resets_in(&self, now: u64) -> Option<u64> {
        [self.primary, self.secondary]
            .into_iter()
            .flatten()
            .filter_map(|w| w.resets_in(now))
            .min()
    }

    /// A one-line summary for a rate-limit message.
    pub fn summary(&self) -> String {
        let parts: Vec<String> = [("", self.primary), ("", self.secondary)]
            .into_iter()
            .filter_map(|(_, w)| w)
            .map(|w| format!("{} {:.0}% used", w.label(), w.used_percent))
            .collect();
        match parts.is_empty() {
            true => "no quota information".to_string(),
            false => parts.join(", "),
        }
    }
}

/// The wire shape of `GET /backend-api/wham/usage`.
#[derive(Deserialize)]
struct Wire {
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    rate_limit: Option<WireRateLimit>,
    #[serde(default)]
    credits: Option<WireCredits>,
}

#[derive(Deserialize)]
struct WireRateLimit {
    #[serde(default)]
    limit_reached: bool,
    #[serde(default)]
    primary_window: Option<WireWindow>,
    #[serde(default)]
    secondary_window: Option<WireWindow>,
}

#[derive(Deserialize)]
struct WireWindow {
    #[serde(default)]
    limit_window_seconds: u64,
    #[serde(default)]
    used_percent: f64,
    #[serde(default)]
    reset_at: Option<u64>,
}

#[derive(Deserialize)]
struct WireCredits {
    #[serde(default)]
    balance: Option<serde_json::Value>,
}

/// Read a usage payload.
///
/// A body that does not parse is `None` rather than an error: quota is
/// advisory everywhere it is used, and a shape change upstream must not take
/// inference down with it.
pub fn parse(body: &str) -> Option<Quota> {
    let wire: Wire = serde_json::from_str(body).ok()?;
    let rate_limit = wire.rate_limit;
    Some(Quota {
        plan_type: wire.plan_type,
        limit_reached: rate_limit.as_ref().is_some_and(|r| r.limit_reached),
        primary: rate_limit
            .as_ref()
            .and_then(|r| r.primary_window.as_ref())
            .map(|w| QuotaWindow {
                window_secs: w.limit_window_seconds,
                used_percent: w.used_percent,
                reset_at: w.reset_at,
            }),
        secondary: rate_limit
            .as_ref()
            .and_then(|r| r.secondary_window.as_ref())
            .map(|w| QuotaWindow {
                window_secs: w.limit_window_seconds,
                used_percent: w.used_percent,
                reset_at: w.reset_at,
            }),
        credit_balance: wire.credits.and_then(|c| c.balance).map(|b| match b {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body a live Plus account returned, trimmed to the fields read here.
    const MEASURED: &str = r#"{
        "plan_type": "plus",
        "rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": {
                "used_percent": 12.5,
                "limit_window_seconds": 18000,
                "reset_after_seconds": 18000,
                "reset_at": 1788137372
            },
            "secondary_window": {
                "used_percent": 3,
                "limit_window_seconds": 604800,
                "reset_at": 1788724172
            }
        },
        "credits": { "has_credits": false, "balance": "0" }
    }"#;

    #[test]
    fn the_measured_payload_parses_into_both_windows() {
        let quota = parse(MEASURED).expect("parses");
        assert_eq!(quota.plan_type.as_deref(), Some("plus"));
        assert!(!quota.limit_reached);
        let primary = quota.primary.expect("primary");
        assert_eq!(primary.window_secs, 18_000);
        assert_eq!(primary.used_percent, 12.5);
        assert_eq!(primary.reset_at, Some(1_788_137_372));
        assert_eq!(quota.secondary.expect("secondary").window_secs, 604_800);
        assert_eq!(quota.credit_balance.as_deref(), Some("0"));
    }

    #[test]
    fn the_five_hour_and_weekly_windows_are_labelled_as_such() {
        let quota = parse(MEASURED).expect("parses");
        assert_eq!(quota.primary.unwrap().label(), "5h");
        assert_eq!(quota.secondary.unwrap().label(), "week");
    }

    #[test]
    fn window_labels_cover_the_shapes_the_route_can_send() {
        let window = |secs| QuotaWindow {
            window_secs: secs,
            used_percent: 0.0,
            reset_at: None,
        };
        assert_eq!(window(0).label(), "window");
        assert_eq!(
            window(60).label(),
            "1h",
            "a sub-hour window still reads as 1h"
        );
        assert_eq!(window(3600).label(), "1h");
        assert_eq!(window(18_000).label(), "5h");
        assert_eq!(window(86_400).label(), "1d");
        assert_eq!(window(172_800).label(), "2d");
        assert_eq!(window(604_800).label(), "week");
    }

    #[test]
    fn the_soonest_reset_is_what_a_rate_limit_should_wait_for() {
        let quota = parse(MEASURED).expect("parses");
        // Primary resets first, so it is the answer.
        assert_eq!(quota.resets_in(1_788_137_000), Some(372));
    }

    #[test]
    fn a_reset_already_past_waits_zero_rather_than_wrapping() {
        let quota = parse(MEASURED).expect("parses");
        assert_eq!(quota.resets_in(u64::MAX), Some(0));
    }

    #[test]
    fn a_payload_with_no_windows_has_no_reset_to_offer() {
        let quota = parse(r#"{"plan_type":"plus"}"#).expect("parses");
        assert_eq!(quota.resets_in(0), None);
        assert_eq!(quota.summary(), "no quota information");
        assert!(quota.primary.is_none());
        assert!(!quota.limit_reached);
    }

    #[test]
    fn a_window_with_no_reset_timestamp_contributes_none() {
        let quota = parse(
            r#"{"rate_limit":{"primary_window":{"used_percent":9,"limit_window_seconds":3600}}}"#,
        )
        .expect("parses");
        assert_eq!(quota.resets_in(0), None);
        assert_eq!(quota.summary(), "1h 9% used");
    }

    #[test]
    fn the_summary_names_both_windows() {
        assert_eq!(
            parse(MEASURED).expect("parses").summary(),
            "5h 12% used, week 3% used"
        );
    }

    #[test]
    fn a_reached_limit_is_reported() {
        let quota = parse(r#"{"rate_limit":{"limit_reached":true}}"#).expect("parses");
        assert!(quota.limit_reached);
    }

    #[test]
    fn a_numeric_credit_balance_is_still_readable() {
        // The route has been seen sending this as a string; a number must not
        // become `None` and read as "no credits".
        let quota = parse(r#"{"credits":{"balance":42}}"#).expect("parses");
        assert_eq!(quota.credit_balance.as_deref(), Some("42"));
    }

    #[test]
    fn a_null_balance_is_absent_rather_than_the_string_null() {
        let quota = parse(r#"{"credits":{"balance":null}}"#).expect("parses");
        assert_eq!(quota.credit_balance, None);
    }

    #[test]
    fn a_body_that_is_not_this_shape_is_none_rather_than_fatal() {
        // Quota is advisory everywhere it is read; an upstream shape change
        // must not take inference down with it.
        assert!(parse("not json").is_none());
        // Not `[]`: serde deserializes a struct from a sequence, and every
        // field here has a default, so an empty one is accepted.
        assert!(parse("42").is_none());
        assert!(parse("\"a string\"").is_none());
    }

    #[test]
    fn an_empty_object_parses_to_nothing_known() {
        let quota = parse("{}").expect("parses");
        assert_eq!(quota, Quota::default());
    }
}
