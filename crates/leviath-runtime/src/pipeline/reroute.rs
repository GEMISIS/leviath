//! Where a stage's call goes once its provider has failed it for good.
//!
//! The retry policy has already had its turn by the time a failure reaches
//! here: a transient one was retried on the same provider with backoff until
//! its attempts or its time ran out. What is left is whether the call moves
//! to another route, and to which.
//!
//! Three failures move it, each for its own reason:
//!
//! - **The provider cannot serve anyone now** (out of credits, a rejected
//!   key, unreachable) or a gateway's upstream for this model is down
//!   ([`UnavailableReason::ModelUnavailable`]). The next route in the
//!   stage's list, whatever it is.
//! - **Availability trouble that outlived its retries**: a 5xx, a 429 or an
//!   "overloaded". The provider is there and keeps failing this call, so the
//!   next route in the list is tried rather than ending the stage.
//! - **The provider says it does not have the model** (a real 404 for it).
//!   Only for a bare model name, and only to another provider in the stage's
//!   list that serves the same model: the author left the route open, so a
//!   provider that cannot serve the model is the wrong route, not the wrong
//!   model. A pinned `provider/model` keeps its answer.
//! - **The route says it cannot do what was asked**: a bad request whose
//!   words are a modality, a parameter or a value the model does not support
//!   ([`ProviderError::is_capability_refusal`]). The next route in the list,
//!   whatever it is: the request is sound, and this route has no way to give
//!   it.
//!
//! Every move uses up the route it moves to, so a call moves at most once per
//! route in the stage's list. Any other bad request, a refused prompt, a token
//! limit or a malformed answer moves nothing: the same request fails the same
//! way everywhere.

use bevy_ecs::prelude::Component;
use leviath_providers::{FailureKind, ProviderError, UnavailableReason};

use crate::spec::names::ModelRef;
use crate::spec::run_spec::RunSpec;

/// Why a failed call moves to another route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reroute {
    /// The provider, or the upstream behind it for this model, cannot serve
    /// the call now.
    Unusable(UnavailableReason),
    /// Availability trouble the retry policy could not ride out.
    Exhausted,
    /// The provider does not have the model.
    NotServed,
    /// The route cannot do what the request asked of it.
    Incapable,
}

impl Reroute {
    /// Why `err` moves the call, or `None` when it ends the stage.
    pub(crate) fn of(err: &ProviderError) -> Option<Self> {
        if let Some(reason) = err.unavailable_reason() {
            return Some(Self::Unusable(reason));
        }
        if err.failure_kind() == Some(FailureKind::NotFound) {
            return Some(Self::NotServed);
        }
        if err.is_capability_refusal() {
            return Some(Self::Incapable);
        }
        err.is_transient().then_some(Self::Exhausted)
    }

    /// The reason a failover record and the stage log give for the move.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Unusable(reason) => reason.label(),
            Self::Exhausted => "retries-exhausted",
            Self::NotServed => "model-not-found",
            Self::Incapable => "route-incapable",
        }
    }

    /// Which of `fallbacks` the call moves to from `provider`/`model`, by
    /// index. `open` is whether that route came from a bare model name (see
    /// [`is_open_route`]).
    pub(crate) fn next(
        self,
        fallbacks: &[ModelRef],
        open: bool,
        provider: &str,
        model: &str,
    ) -> Option<usize> {
        match self {
            Self::Unusable(_) | Self::Exhausted | Self::Incapable => {
                (!fallbacks.is_empty()).then_some(0)
            }
            Self::NotServed if open => fallbacks.iter().position(|f| {
                f.provider_or_empty() != provider
                    && super::model_key(f.model.as_str()) == super::model_key(model)
            }),
            Self::NotServed => None,
        }
    }
}

/// Whether the route `provider`/`model` stage `idx` is on came from a bare
/// model name, which leaves the provider to the machine.
///
/// A run-level `--model` decides it for every stage: bare is open, and a
/// pinned one has no fallbacks to move to anyway. Otherwise the stage's own
/// list does: open when it names this model bare and does not pin this exact
/// pair.
pub(crate) fn is_open_route(spec: &RunSpec, idx: usize, provider: &str, model: &str) -> bool {
    if let Some(requested) = &spec.requested_model {
        return requested.provider.is_none();
    }
    let Some(stage) = spec.graph.stages.get(idx) else {
        return false;
    };
    let models = &stage.model.models;
    let pinned = models
        .iter()
        .any(|m| m.provider_or_empty() == provider && m.model.as_str() == model);
    let key = super::model_key(model);
    !pinned
        && models
            .iter()
            .any(|m| m.provider.is_none() && super::model_key(m.model.as_str()) == key)
}

/// The routes a stage's call has already been moved off, so the error that
/// ends it, when every route failed, names each one.
///
/// Held on the agent only while a call is moving: an answer clears it, and so
/// does the error that ends the stage. Never written to the run file.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RoutesTried {
    /// The stage the call belongs to.
    stage: String,
    /// Each route left, as `provider/model (why)`, in order.
    routes: Vec<String>,
}

impl RoutesTried {
    /// Note that the call on `stage` left `provider/model` because of `why`.
    pub(crate) fn left(&mut self, stage: &str, provider: &str, model: &str, why: &str) {
        if self.stage != stage {
            self.stage = stage.to_string();
            self.routes.clear();
        }
        self.routes.push(format!("{provider}/{model} ({why})"));
    }

    /// The error that ends the call on `stage`, where `last` (as
    /// `provider/model (why)`) failed it with `error`: naming every route it
    /// was moved off before, when there were any.
    pub(crate) fn ending(&self, stage: &str, last: &str, error: &str) -> String {
        match self.stage == stage && !self.routes.is_empty() {
            true => format!(
                "{error} [every route failed: {}, {last}]",
                self.routes.join(", ")
            ),
            false => error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(list: &[&str]) -> Vec<ModelRef> {
        list.iter().map(|r| ModelRef::parse(r).unwrap()).collect()
    }

    #[test]
    fn each_failure_moves_or_ends_the_call_by_its_kind() {
        let unusable = ProviderError::Unavailable {
            reason: UnavailableReason::CreditsExhausted,
            detail: "HTTP 402".into(),
        };
        assert_eq!(
            Reroute::of(&unusable),
            Some(Reroute::Unusable(UnavailableReason::CreditsExhausted))
        );
        let not_found = ProviderError::ApiError("[not-found] HTTP 404 Not Found: no".into());
        assert_eq!(Reroute::of(&not_found), Some(Reroute::NotServed));
        let server = ProviderError::ApiError("[server-error] HTTP 503 Service Unavailable".into());
        assert_eq!(Reroute::of(&server), Some(Reroute::Exhausted));
        let limited = ProviderError::RateLimitExceeded {
            retry_after_secs: Some(3),
        };
        assert_eq!(Reroute::of(&limited), Some(Reroute::Exhausted));
        let incapable = ProviderError::ApiError(
            "[bad-request] HTTP 400 Bad Request: this model does not support audio output".into(),
        );
        assert_eq!(Reroute::of(&incapable), Some(Reroute::Incapable));
        // What fails the same way everywhere ends the stage.
        for stays in [
            ProviderError::ApiError("[bad-request] HTTP 400 Bad Request: no".into()),
            ProviderError::InvalidResponse("bad json".into()),
            ProviderError::TokenLimitExceeded {
                used: 10,
                reply_budget: 1,
                max: 5,
            },
            ProviderError::Other("refused by the content policy".into()),
        ] {
            assert_eq!(Reroute::of(&stays), None, "{stays}");
        }
        assert_eq!(
            Reroute::Unusable(UnavailableReason::ModelUnavailable).label(),
            "model-unavailable"
        );
        assert_eq!(Reroute::Exhausted.label(), "retries-exhausted");
        assert_eq!(Reroute::NotServed.label(), "model-not-found");
        assert_eq!(Reroute::Incapable.label(), "route-incapable");
    }

    #[test]
    fn a_missing_model_moves_only_an_open_route_and_only_to_the_same_model() {
        let list = refs(&["openai/gpt-x", "meta/muse-1", "openrouter/meta/muse-1"]);
        assert_eq!(
            Reroute::NotServed.next(&list, true, "openrouter", "meta/muse-1"),
            Some(1),
            "the next provider serving the same model, past a different model"
        );
        assert_eq!(
            Reroute::NotServed.next(&list, false, "openrouter", "meta/muse-1"),
            None,
            "a pinned route keeps its answer"
        );
        assert_eq!(
            Reroute::NotServed.next(&refs(&["openai/gpt-x"]), true, "meta", "muse-1"),
            None
        );
        assert_eq!(Reroute::Exhausted.next(&list, false, "x", "y"), Some(0));
        assert_eq!(Reroute::Exhausted.next(&[], true, "x", "y"), None);
        // A route that cannot do it moves on down the list, pinned or open.
        assert_eq!(Reroute::Incapable.next(&list, false, "x", "y"), Some(0));
        assert_eq!(Reroute::Incapable.next(&[], true, "x", "y"), None);
    }

    /// A spec whose one stage `s` lists `models`.
    fn spec_listing(models: &[&str]) -> RunSpec {
        let stage = crate::spec::graph::StageDef {
            model: crate::spec::graph::ModelChoice {
                models: refs(models),
                ..Default::default()
            },
            ..crate::test_graph::stage("s")
        };
        let layout = crate::test_graph::layout(vec![], 0);
        RunSpec {
            requested_model: None,
            ..crate::test_graph::spec_named("bp", crate::test_graph::graph(vec![stage], layout))
        }
    }

    #[test]
    fn a_route_is_open_when_the_stage_names_its_model_bare() {
        let spec = spec_listing(&["muse-1", "openai/gpt-x"]);
        assert!(is_open_route(&spec, 0, "openrouter", "meta/muse-1"));
        assert!(is_open_route(&spec, 0, "meta", "muse-1"));
        assert!(!is_open_route(&spec, 0, "openai", "gpt-x"), "pinned");
        assert!(!is_open_route(&spec, 7, "meta", "muse-1"), "no such stage");
        // A bare name that is also pinned to this very pair is the pin.
        let both = spec_listing(&["meta/muse-1", "muse-1"]);
        assert!(!is_open_route(&both, 0, "meta", "muse-1"));
        assert!(is_open_route(&both, 0, "openrouter", "meta/muse-1"));
        // A run-level `--model` decides for every stage.
        let mut bare = spec_listing(&["openai/gpt-x"]);
        bare.requested_model = Some(ModelRef::parse("gpt-x").unwrap());
        assert!(is_open_route(&bare, 0, "openai", "gpt-x"));
        bare.requested_model = Some(ModelRef::parse("openai/gpt-x").unwrap());
        assert!(!is_open_route(&bare, 0, "openai", "gpt-x"));
    }

    #[test]
    fn the_ending_names_every_route_tried_on_its_own_stage() {
        let mut tried = RoutesTried::default();
        assert_eq!(tried.ending("s", "z/m (x)", "boom"), "boom");
        tried.left("s", "openrouter", "meta/muse-1", "model-unavailable");
        tried.left("s", "meta", "muse-1", "model-not-found");
        assert_eq!(
            tried.ending("s", "openai/muse-1 (server-error)", "boom"),
            "boom [every route failed: openrouter/meta/muse-1 (model-unavailable), \
             meta/muse-1 (model-not-found), openai/muse-1 (server-error)]"
        );
        assert_eq!(tried.ending("other", "z/m (x)", "boom"), "boom");
        tried.left("other", "a", "b", "why");
        assert_eq!(
            tried.ending("other", "c/d (e)", "err"),
            "err [every route failed: a/b (why), c/d (e)]"
        );
    }
}
