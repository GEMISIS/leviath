//! How Leviath writes a span of seconds, in the two shapes its surfaces need.
//!
//! One module rather than one per surface. `lev ps` and the dashboard each grew
//! their own formatter, which is how the same run came to read `4m` in one and
//! `4m12s` in the other with nothing saying the two were the same number.
//!
//! There are two shapes because there are two jobs, not because anyone
//! disagreed:
//!
//! - [`compact`] answers "roughly how long", in one unit, for a table column
//!   that has to stay narrow next to a dozen others.
//! - [`precise`] answers "exactly how long", in two units, for a single figure
//!   a reader is looking straight at.
//!
//! Both clamp at zero: a machine whose clock stepped backwards should read `0s`
//! rather than a negative span or a wrapped one.

/// A span in the largest unit that keeps the number small: `12s`, `4m`, `3h`,
/// `2d`.
///
/// For table cells. Deliberately lossy - `4m` covers anything from four minutes
/// to just under five - because a column that has to fit beside a run id, a
/// status and a stage cannot afford the second unit, and at a glance the
/// magnitude is the whole message.
pub fn compact(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// A span in two units: `45s`, `2m5s`, `1h20m`.
///
/// For a figure the reader is looking straight at - a run's duration in the
/// dashboard header, a stage's on its tab - where the seconds are the point and
/// there is room for them.
pub fn precise(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3_600, (secs % 3_600) / 60)
    }
}

/// The seconds between two unix timestamps, clamped at zero.
///
/// The clamp is not paranoia about arithmetic: these are wall-clock stamps from
/// a machine whose clock can be corrected, and a run whose `started_at` lands
/// after `now` should read as brand new rather than as `u64::MAX` seconds old.
pub fn between(from: i64, to: i64) -> u64 {
    (to - from).max(0) as u64
}

/// The JSON key carrying a run's age in seconds, on every API that serves runs.
pub const AGE_SECS_KEY: &str = "age_secs";

/// The JSON key carrying a run's working time in seconds, on every API that
/// serves runs.
pub const WORKING_SECS_KEY: &str = "working_secs";

/// Add the two computed spans to a serialized run object.
///
/// The raw stamps stay: a caller that wants to do its own arithmetic still can.
/// These are here so it does not have to, because the working span is the one
/// with a rule behind it - banked seconds plus the span in progress - and every
/// client that reimplemented that rule would be a place for it to drift.
///
/// A non-object is left alone rather than wrapped: there is no run without an
/// object to annotate, and inventing one would hide the caller's mistake.
pub fn annotate_spans(value: &mut serde_json::Value, age_secs: u64, working_secs: u64) {
    if let serde_json::Value::Object(map) = value {
        map.insert(AGE_SECS_KEY.to_string(), age_secs.into());
        map.insert(WORKING_SECS_KEY.to_string(), working_secs.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_uses_one_unit_and_names_each_boundary() {
        assert_eq!(compact(0), "0s");
        assert_eq!(compact(59), "59s");
        assert_eq!(compact(60), "1m");
        assert_eq!(compact(3_599), "59m");
        assert_eq!(compact(3_600), "1h");
        assert_eq!(compact(86_399), "23h");
        assert_eq!(compact(86_400), "1d");
        assert_eq!(compact(172_800), "2d");
    }

    #[test]
    fn precise_uses_two_units_and_names_each_boundary() {
        assert_eq!(precise(0), "0s");
        assert_eq!(precise(45), "45s");
        assert_eq!(precise(59), "59s");
        assert_eq!(precise(60), "1m0s");
        assert_eq!(precise(125), "2m5s");
        assert_eq!(precise(3_599), "59m59s");
        assert_eq!(precise(3_600), "1h0m");
        assert_eq!(precise(4_800), "1h20m");
    }

    #[test]
    fn annotate_spans_adds_both_keys_and_keeps_the_raw_stamps() {
        let mut v = serde_json::json!({ "run_id": "r", "started_at": 100 });
        annotate_spans(&mut v, 3_840, 720);
        assert_eq!(v[AGE_SECS_KEY], 3_840);
        assert_eq!(v[WORKING_SECS_KEY], 720);
        assert_eq!(v["started_at"], 100, "the raw stamps are not replaced");
    }

    #[test]
    fn annotate_spans_leaves_a_non_object_alone() {
        let mut v = serde_json::Value::Null;
        annotate_spans(&mut v, 1, 2);
        assert_eq!(v, serde_json::Value::Null);
    }

    #[test]
    fn between_clamps_a_clock_that_moved_backwards() {
        assert_eq!(between(100, 145), 45);
        assert_eq!(between(100, 100), 0);
        assert_eq!(between(200, 100), 0);
    }
}
