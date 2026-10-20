//! The shapes a provider-neutral effort takes on the wire, shared by the
//! providers whose models speak the same dialect.
//!
//! Each provider's [`crate::Provider::effort_extra`] decides whether a model
//! takes an effort at all; the helpers here only spell it.

use leviath_core::Effort;
use serde_json::{Value, json};

/// `reasoning.effort`, as the Responses routes and OpenRouter take it, with
/// `effort` moved to a level in `accepted` (see [`Effort::clamp`]).
pub fn reasoning_effort(effort: Effort, accepted: Option<&[String]>) -> Value {
    effort.clamp(accepted).map_or(
        Value::Null,
        |level| json!({ "reasoning": { "effort": level } }),
    )
}

/// `output_config.effort`, as Claude takes it.
pub fn claude_effort(model: &str, effort: Effort) -> Value {
    match claude_takes_effort(model) {
        true => json!({ "output_config": { "effort": effort.as_str() } }),
        false => Value::Null,
    }
}

/// Whether a Claude model takes `output_config.effort`.
///
/// Opus 4.5 and every model from 4.6 on do; Sonnet 4.5, Haiku 4.5 and older
/// refuse it. Read from the id, wherever it sits in a longer one
/// (`us.anthropic.claude-opus-4-6-v1:0` on Bedrock): the family, then the
/// major version, then a minor version of one or two digits when there is
/// one (a dated snapshot's eight digits are not a minor version). An id this
/// cannot read takes none.
pub fn claude_takes_effort(model: &str) -> bool {
    let Some((_, rest)) = model.split_once("claude-") else {
        return false;
    };
    let mut parts = rest.split(['-', '.', ':', '[']);
    let family = parts.next().unwrap_or_default();
    let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor: u32 = parts
        .next()
        .filter(|p| p.len() <= 2)
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    match family {
        "fable" | "mythos" => true,
        "opus" | "sonnet" | "haiku" => {
            (major, minor) >= (4, 6) || (family == "opus" && (major, minor) == (4, 5))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_models_from_four_six_on_take_an_effort() {
        for model in [
            "claude-opus-5-5",
            "claude-sonnet-5",
            "claude-haiku-5-5",
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "claude-opus-4-6",
            "claude-sonnet-4-6[1m]",
            "claude-opus-4-5-20251101",
            "us.anthropic.claude-opus-4-6-v1:0",
            "anthropic/claude-sonnet-5",
        ] {
            assert!(claude_takes_effort(model));
        }
    }

    #[test]
    fn older_claude_models_and_other_ids_take_none() {
        for model in [
            "claude-sonnet-4-5",
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-20250514",
            "claude-opus-4-1",
            "claude-3-7-sonnet-latest",
            "claude-opus",
            "gpt-5",
            "claude-",
        ] {
            assert!(!claude_takes_effort(model));
        }
    }

    #[test]
    fn the_spellings() {
        assert_eq!(
            claude_effort("claude-opus-5", Effort::Low),
            json!({ "output_config": { "effort": "low" } })
        );
        assert!(claude_effort("claude-sonnet-4-5", Effort::Low).is_null());
        assert_eq!(
            reasoning_effort(Effort::High, None),
            json!({ "reasoning": { "effort": "high" } })
        );
        assert!(reasoning_effort(Effort::High, Some(&[])).is_null());
    }
}
