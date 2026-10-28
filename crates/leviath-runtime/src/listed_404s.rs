//! Listed models that keep answering 404, given up on for the life of the
//! process.
//!
//! A 404 for a model the provider's own listing names is read as an outage
//! and retried (see [`crate::inference_bridge::listed_outage`]). That is
//! right for a bad minute and wrong for a model the provider lists and will
//! never serve again: OpenAI listed `sora-2` for a month after shutting it
//! down, and every call to it spent its retries before moving on, on every
//! stage that named it. Retirement dates catch the case a listing dates; this
//! catches the one it does not. After [`GIVE_UP_AFTER`] such answers in a row
//! with none answered in between, the model is not retried, not offered an
//! open route, and refused by name, until the daemon restarts.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use leviath_providers::{ProviderError, UnavailableReason};

/// How many 404s in a row a listed model answers before it is given up on:
/// two calls' worth of the default retries
/// ([`crate::inference_bridge::DEFAULT_RETRY_ATTEMPTS`]), so one bad minute
/// that a later call rides out never trips it.
pub(crate) const GIVE_UP_AFTER: u32 = 2 * crate::inference_bridge::DEFAULT_RETRY_ATTEMPTS;

/// The current run of 404s, per provider and model key.
static STREAKS: LazyLock<Mutex<HashMap<(String, String), u32>>> = LazyLock::new(Default::default);

fn key(provider: &str, model: &str) -> (String, String) {
    (
        provider.to_string(),
        crate::pipeline::model_key(model).to_string(),
    )
}

/// Count one more 404 from `provider` for the listed `model`; the error to
/// end the call with once that makes the model given up on, else `None`.
///
/// Not an outage any more, so not retried; still a reason to move to the next
/// route, as any model out of reach on its route is.
pub(crate) fn missed(provider: &str, model: &str) -> Option<ProviderError> {
    let mut streaks = leviath_core::sync::lock(&STREAKS);
    let streak = streaks.entry(key(provider, model)).or_default();
    *streak = streak.saturating_add(1);
    (*streak >= GIVE_UP_AFTER).then(|| ProviderError::Unavailable {
        reason: UnavailableReason::ModelUnavailable,
        detail: given_up_detail(provider, model),
    })
}

/// `provider` answered a call to `model`: whatever run of 404s it had is over.
pub(crate) fn answered(provider: &str, model: &str) {
    leviath_core::sync::lock(&STREAKS).remove(&key(provider, model));
}

/// Whether `model` on `provider` has been given up on in this process.
pub(crate) fn given_up(provider: &str, model: &str) -> bool {
    leviath_core::sync::lock(&STREAKS)
        .get(&key(provider, model))
        .is_some_and(|streak| *streak >= GIVE_UP_AFTER)
}

/// Why a stage naming `model` on `provider` is refused, when it was given up
/// on.
pub(crate) fn refusal(provider: &str, model: &str) -> Option<String> {
    given_up(provider, model).then(|| given_up_detail(provider, model))
}

fn given_up_detail(provider: &str, model: &str) -> String {
    format!(
        "{provider}/{} answered 404 {GIVE_UP_AFTER} times in a row although \
         '{provider}' lists it, so it is treated as unavailable and not routed to \
         again until the daemon restarts",
        crate::pipeline::model_key(model)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique per test: the streaks are process-wide.
    const PROVIDER: &str = "listed-404s-test";

    #[test]
    fn a_run_of_404s_gives_the_model_up_and_an_answer_ends_the_run() {
        let model = "gone-1";
        for _ in 1..GIVE_UP_AFTER {
            assert!(missed(PROVIDER, model).is_none());
        }
        assert!(!given_up(PROVIDER, model));
        assert_eq!(refusal(PROVIDER, model), None);
        // An answer in between starts the count again.
        answered(PROVIDER, model);
        for _ in 1..GIVE_UP_AFTER {
            assert!(missed(PROVIDER, model).is_none());
        }
        let err = missed(PROVIDER, model).expect("given up");
        assert!(!err.is_transient(), "not retried any more");
        assert_eq!(
            err.unavailable_reason(),
            Some(UnavailableReason::ModelUnavailable),
            "still moves to the next route"
        );
        assert!(err.to_string().contains("until the daemon restarts"));
        assert!(given_up(PROVIDER, model));
        // Counted by model key, so a namespaced id is the same model.
        assert!(given_up(PROVIDER, "vendor/gone-1"));
        assert!(!given_up("another-provider", model));
        let why = refusal(PROVIDER, model).expect("given up");
        assert!(
            why.contains("listed-404s-test/gone-1 answered 404"),
            "{why}"
        );
    }
}
