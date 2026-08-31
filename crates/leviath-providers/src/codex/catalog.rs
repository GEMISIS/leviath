//! What this route serves, and which plans can reach it.
//!
//! There is no `/models` endpoint here, so the catalog is compiled in the way
//! the Claude Code transport's is. Two consequences worth stating rather than
//! discovering: the context windows are this build's belief and cannot be
//! checked against the server, and the reachable set depends on the account's
//! ChatGPT plan.
//!
//! Percentage region budgets resolve once at spawn against
//! [`ModelCapabilities::max_context_tokens`], so a wrong row here silently
//! mis-sizes every region for a whole run - which is exactly what happened:
//! every model carried 400,000, a figure no vendor publishes for any of them,
//! and a `gpt-5.5` run used 38% of the window it had for months.
//!
//! The windows are OpenAI's published `max_input_tokens`, cross-checked
//! against OpenRouter's `context_length` (which is the whole window, and
//! larger by exactly the output allowance). Whether the Codex *route* caps
//! lower than the API is not knowable from here - it publishes no catalogue -
//! so these assume it does not. If it turns out to, the symptom is a refused
//! request rather than a silent one, and `[model_capabilities]` is the lever.
//!
//! `cargo xtask prices` re-checks these against the published figures every
//! week and reports a row that has drifted. It never rewrites them.

use crate::capabilities::{LimitsSource, Match, ModelCapabilities, Row};

/// The models this route serves, as `(id, display name)`.
///
/// Kept honest by `cargo xtask prices`, which every week compares this against
/// OpenAI's published catalogue and reports anything that looks renamed,
/// withdrawn or newly served. It cannot fix the list - what Codex serves is
/// published nowhere - so a name it raises is a prompt to ask the route.
///
/// Two it raised have been asked, on a Plus account on 2026-08-31:
/// `gpt-5.6-cyber` and `gpt-5.6-sol-pro` both answer `400 ... not supported
/// when using Codex with a ChatGPT account`, in the same run where `gpt-5.5`
/// and `gpt-5.6-sol` answered 200. They are recorded in that task's
/// `MEASURED_ABSENT` so it stops asking.
pub(crate) const CATALOG: &[(&str, &str)] = &[
    ("gpt-5.6-sol", "GPT-5.6 Sol"),
    ("gpt-5.6-terra", "GPT-5.6 Terra"),
    ("gpt-5.6-luna", "GPT-5.6 Luna"),
    ("gpt-5.5", "GPT-5.5"),
    ("gpt-5.3-codex-spark", "GPT-5.3 Codex Spark"),
];

/// Models a plan tier cannot reach, by the exact string the tier reports.
///
/// An allowlist would be the obvious shape and is the wrong one: a model added
/// to the route tomorrow would be refused for everybody until this table caught
/// up. A denylist fails open, which for "can this account use this model" is
/// the right direction. The server is the real gate and says so clearly.
const PLAN_EXCLUSIONS: &[(&str, &[&str])] = &[
    // Measured: `gpt-5.3-codex-spark` on a Plus account answers 400 with
    // "The 'gpt-5.3-codex-spark' model is not supported when using Codex with
    // a ChatGPT account."
    ("free", &["gpt-5.3-codex-spark"]),
    ("plus", &["gpt-5.3-codex-spark"]),
];

/// What this build knows about the models, most specific first.
///
/// Every row sets `temperature: false`. Not a per-model quirk the way it is on
/// the OpenAI provider: this route answers `400 Unsupported parameter:
/// temperature` for every model it serves.
pub(crate) const MODELS: &[Row] = &[
    Row {
        matches: &[Match::Prefix("gpt-5.6")],
        temperature: false,
        tools: true,
        context: 922_000,
        output: 128_000,
    },
    Row {
        matches: &[Match::Prefix("gpt-5.5")],
        temperature: false,
        tools: true,
        context: 1_050_000,
        output: 128_000,
    },
    Row {
        // Text-only research preview, and the smallest window of the set.
        matches: &[Match::Contains("codex-spark")],
        temperature: false,
        tools: true,
        context: 128_000,
        output: 32_000,
    },
    Row {
        matches: &[Match::Prefix("gpt-5")],
        temperature: false,
        tools: true,
        context: 272_000,
        output: 128_000,
    },
];

/// Capabilities for `model`, before any operator override.
pub(crate) fn capabilities(model: &str) -> ModelCapabilities {
    crate::capabilities::lookup(
        MODELS,
        model,
        // The conservative fallback. Guessing high would size every region
        // against a window that is not there, and the run would fail at the
        // far end of a long stage rather than at its start.
        ModelCapabilities {
            supports_temperature: false,
            supports_streaming: true,
            supports_tools: true,
            max_context_tokens: 128_000,
            max_output_tokens: 32_000,
            limits_source: LimitsSource::Builtin,
            supports_system_prompt: true,
        },
    )
}

/// Whether `plan` can reach `model`.
///
/// An unknown plan answers `true`: refusing a model because the tier could not
/// be read would turn a missing id token into a dead provider, and the server
/// refuses clearly enough on its own.
pub(crate) fn plan_allows(plan: Option<&str>, model: &str) -> bool {
    let Some(plan) = plan else {
        return true;
    };
    let plan = plan.to_ascii_lowercase();
    !PLAN_EXCLUSIONS
        .iter()
        .filter(|(tier, _)| *tier == plan)
        .any(|(_, denied)| denied.iter().any(|d| model.contains(d)))
}

/// The models `plan` can reach, as ids.
pub(crate) fn served(plan: Option<&str>) -> Vec<String> {
    CATALOG
        .iter()
        .filter(|(id, _)| plan_allows(plan, id))
        .map(|(id, _)| (*id).to_string())
        .collect()
}

/// The remedy for a model the account's plan does not include.
pub(crate) fn gated_remedy(plan: Option<&str>, model: &str) -> String {
    let available = served(plan).join(", ");
    match plan {
        Some(plan) => format!(
            "your ChatGPT {plan} plan does not include '{model}'. Available on this plan: \
             {available}. Change the stage's model, or upgrade the plan."
        ),
        None => format!(
            "this ChatGPT account cannot use '{model}'. Models this build knows about: \
             {available}."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_model_on_this_route_takes_a_temperature() {
        // Measured on every id in the catalog, not inferred from one of them.
        for (id, _) in CATALOG {
            assert!(
                !capabilities(id).supports_temperature,
                "{id} claims to take a temperature"
            );
        }
    }

    #[test]
    fn every_catalog_id_has_a_row_that_is_not_the_fallback() {
        // A fallback-shaped answer for a shipped id means the table missed it,
        // and a 128k window where the model has 400k mis-sizes every region.
        for (id, _) in CATALOG {
            let caps = capabilities(id);
            assert!(caps.supports_tools, "{id}");
            assert!(caps.max_context_tokens >= 128_000, "{id}");
        }
        // The published `max_input_tokens`, not a round number: these were
        // 400,000 for both, which no vendor publishes for either, and the
        // weekly window check is what now holds them to the source.
        assert_eq!(capabilities("gpt-5.6-sol").max_context_tokens, 922_000);
        assert_eq!(capabilities("gpt-5.5").max_context_tokens, 1_050_000);
    }

    #[test]
    fn the_research_preview_gets_its_own_smaller_row() {
        let caps = capabilities("gpt-5.3-codex-spark");
        assert_eq!(caps.max_context_tokens, 128_000);
        assert_eq!(caps.max_output_tokens, 32_000);
    }

    #[test]
    fn an_unknown_model_falls_back_conservatively() {
        // Guessing high here would size regions against a window that is not
        // there, and the run would fail at the far end of a long stage.
        let caps = capabilities("something-new");
        assert_eq!(caps.max_context_tokens, 128_000);
        assert!(caps.supports_tools);
        assert!(!caps.supports_temperature);
    }

    #[test]
    fn a_plus_plan_reaches_everything_but_the_pro_preview() {
        // Exactly what the probe measured against a live Plus account.
        let served = served(Some("plus"));
        assert!(served.contains(&"gpt-5.6-sol".to_string()));
        assert!(served.contains(&"gpt-5.6-terra".to_string()));
        assert!(served.contains(&"gpt-5.6-luna".to_string()));
        assert!(served.contains(&"gpt-5.5".to_string()));
        assert!(!served.contains(&"gpt-5.3-codex-spark".to_string()));
    }

    #[test]
    fn a_pro_plan_reaches_the_preview_too() {
        assert!(plan_allows(Some("pro"), "gpt-5.3-codex-spark"));
        assert_eq!(served(Some("pro")).len(), CATALOG.len());
    }

    #[test]
    fn an_unknown_plan_refuses_nothing() {
        // Failing open: the server is the real gate, and a missing id token
        // must not read as "this account can use nothing".
        assert!(plan_allows(None, "gpt-5.3-codex-spark"));
        assert_eq!(served(None).len(), CATALOG.len());
        assert!(plan_allows(Some("some-new-tier"), "gpt-5.3-codex-spark"));
    }

    #[test]
    fn the_plan_comparison_ignores_case() {
        assert!(!plan_allows(Some("PLUS"), "gpt-5.3-codex-spark"));
    }

    #[test]
    fn the_gated_remedy_names_the_plan_and_what_is_left() {
        let message = gated_remedy(Some("plus"), "gpt-5.3-codex-spark");
        assert!(message.contains("plus plan"), "{message}");
        assert!(message.contains("gpt-5.6-sol"), "{message}");
        assert!(!message.contains("Available on this plan: gpt-5.3-codex-spark"));
    }

    #[test]
    fn the_gated_remedy_still_helps_without_a_known_plan() {
        let message = gated_remedy(None, "some-model");
        assert!(message.contains("some-model"), "{message}");
        assert!(message.contains("gpt-5.6-sol"), "{message}");
    }
}
