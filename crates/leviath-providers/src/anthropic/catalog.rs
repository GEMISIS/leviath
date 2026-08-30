//! What Anthropic's `GET /v1/models` says about one model.
//!
//! Measured against the live endpoint (2026-08-28) and its reference page:
//! each entry carries `id`, `display_name`, `created_at` (RFC 3339),
//! `max_input_tokens`, `max_tokens` and a `capabilities` object, inside a
//! paginated envelope of `data`, `has_more`, `first_id` and `last_id`. The
//! default page is twenty entries and the cap is a thousand, so a reader that
//! takes one page at the default silently truncates once the catalogue grows.
//!
//! The endpoint does report size, and this is where it is read.

use crate::learned::{LearnedModel, unix_seconds_from_rfc3339};

/// How many entries one page asks for: the documented maximum.
pub(super) const PAGE_LIMIT: usize = 1000;

/// One listing entry as a [`LearnedModel`], keyed by its id.
///
/// The limits are `None` when absent, null or zero: the reference example
/// shows both as `0`, and a zero output cap would collapse every request. The
/// temperature flag is `None` because the endpoint has no such field - its
/// `capabilities` object covers batching, citations, thinking and effort - so
/// the compiled table remains the only source for which models refuse one.
/// Tools are recorded as taken by every entry: every Anthropic chat model
/// takes them, and the listing carries chat models only.
pub(super) fn parse_entry(entry: &serde_json::Value) -> Option<(String, LearnedModel)> {
    let id = entry.get("id").and_then(|v| v.as_str())?.to_string();
    let limit = |key: &str| {
        entry
            .get(key)
            .and_then(|v| v.as_u64())
            .filter(|n| *n > 0)
            .map(|n| n as usize)
    };
    Some((
        id,
        LearnedModel {
            display_name: entry
                .get("display_name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            max_context_tokens: limit("max_input_tokens"),
            max_output_tokens: limit("max_tokens"),
            supports_temperature: None,
            supports_tools: Some(true),
            // The direct API always reads `cache_control` markers; the flag
            // exists for a gateway choosing per upstream.
            explicit_cache_control: None,
            // No rates on the listing; `published_rates` stays the source.
            pricing: None,
            released: entry
                .get("created_at")
                .and_then(|v| v.as_str())
                .and_then(unix_seconds_from_rfc3339),
            retires: None,
        },
    ))
}

/// The `after_id` cursor for the next page, if the envelope says there is one.
pub(super) fn next_page(body: &serde_json::Value) -> Option<String> {
    if body.get("has_more").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    body.get("last_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_full_entry_fills_what_the_listing_carries() {
        let entry = json!({
            "type": "model",
            "id": "claude-opus-5",
            "display_name": "Claude Opus 5",
            "created_at": "2026-07-24T00:00:00Z",
            "max_input_tokens": 1_000_000,
            "max_tokens": 128_000,
            "capabilities": { "thinking": { "supported": true } }
        });
        let (id, learned) = parse_entry(&entry).unwrap();
        assert_eq!(id, "claude-opus-5");
        assert_eq!(learned.display_name.as_deref(), Some("Claude Opus 5"));
        assert_eq!(learned.max_context_tokens, Some(1_000_000));
        assert_eq!(learned.max_output_tokens, Some(128_000));
        assert_eq!(learned.supports_temperature, None);
        assert_eq!(learned.supports_tools, Some(true));
        assert_eq!(learned.explicit_cache_control, None);
        assert_eq!(learned.pricing, None);
        assert_eq!(learned.released, Some(1_784_851_200));
        assert_eq!(learned.retires, None);
    }

    #[test]
    fn zero_null_and_absent_limits_are_all_unknown() {
        for entry in [
            json!({ "id": "m", "max_input_tokens": 0, "max_tokens": 0 }),
            json!({ "id": "m", "max_input_tokens": null, "max_tokens": null }),
            json!({ "id": "m" }),
        ] {
            let (_, learned) = parse_entry(&entry).unwrap();
            assert_eq!(learned.max_context_tokens, None);
            assert_eq!(learned.max_output_tokens, None);
            assert_eq!(learned.released, None);
            assert_eq!(learned.display_name, None);
        }
    }

    #[test]
    fn an_unreadable_date_is_no_date() {
        let (_, learned) = parse_entry(&json!({ "id": "m", "created_at": "yesterday" })).unwrap();
        assert_eq!(learned.released, None);
    }

    #[test]
    fn an_entry_without_an_id_is_dropped() {
        assert_eq!(parse_entry(&json!({ "display_name": "x" })), None);
    }

    #[test]
    fn the_next_page_is_named_only_when_there_is_one() {
        assert_eq!(
            next_page(&json!({ "has_more": true, "last_id": "claude-x" })).as_deref(),
            Some("claude-x")
        );
        assert_eq!(
            next_page(&json!({ "has_more": false, "last_id": "claude-x" })),
            None
        );
        assert_eq!(next_page(&json!({ "last_id": "claude-x" })), None);
        assert_eq!(next_page(&json!({ "has_more": true })), None);
        assert_eq!(
            next_page(&json!({ "has_more": true, "last_id": null })),
            None
        );
    }
}
