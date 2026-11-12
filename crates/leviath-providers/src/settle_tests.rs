use super::*;
use crate::provider::{InferenceRequest, InferenceResponse, ModelCapabilities, Result};
use std::sync::Mutex;

/// A provider whose rates, record of a call and billing terms a test sets.
#[derive(Default)]
struct Shop {
    /// What it charges, by model.
    rates: Vec<(&'static str, ModelPricing)>,
    /// What each request for a call's record answers, in order. Empty means
    /// it keeps no record at all.
    record: Mutex<Vec<Result<TokenUsage>>>,
    /// Whether it keeps records.
    keeps_records: bool,
    /// Whether its terms say an error answer is free.
    free_errors: bool,
    /// How many times its record was asked for.
    asked: Mutex<usize>,
    /// How long each request for a record takes to answer.
    slow: Duration,
}

#[async_trait::async_trait]
impl Provider for Shop {
    async fn infer(&self, _request: &InferenceRequest) -> Result<InferenceResponse> {
        Err(ProviderError::unsent("not called"))
    }
    async fn count_tokens(&self, text: &str, _model: &str) -> usize {
        text.len()
    }
    fn max_context_tokens(&self, _model: &str) -> usize {
        0
    }
    fn name(&self) -> &str {
        "shop"
    }
    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        self.rates
            .iter()
            .find(|(id, _)| *id == model)
            .map(|(_, rates)| *rates)
    }
    async fn call_usage(&self, _call_id: &str, _model: &str) -> Option<Result<TokenUsage>> {
        if !self.keeps_records {
            return None;
        }
        *self.asked.lock().unwrap() += 1;
        // No timer at all for a record that answers at once: a sleep of
        // nothing still yields on some clocks, and the lookup's own bound
        // would then decide which way a test goes.
        if !self.slow.is_zero() {
            tokio::time::sleep(self.slow).await;
        }
        let mut record = self.record.lock().unwrap();
        Some(match record.is_empty() {
            true => Err(ProviderError::ApiError("HTTP 404 not yet".into())),
            false => record.remove(0),
        })
    }
    fn bills_no_error_answer(&self) -> bool {
        self.free_errors
    }
}

fn priced() -> Shop {
    Shop {
        rates: vec![("m", ModelPricing::flat(1.0, 2.0))],
        ..Shop::default()
    }
}

const SHORT: Duration = Duration::from_millis(40);
const TICK: Duration = Duration::from_millis(10);

async fn settle(shop: &Shop, ended: Ended<'_>, seen: &Traced) -> Option<Unanswered> {
    unanswered(shop, "m", ended, seen, SHORT, TICK).await
}

fn seen_id(id: &str) -> Traced {
    Traced {
        call_id: Some(id.to_string()),
        ..Traced::default()
    }
}

#[tokio::test]
async fn a_call_that_was_never_sent_is_no_call() {
    let error = ProviderError::unsent("refused here");
    assert_eq!(
        settle(&priced(), Ended::Failed(&error), &Traced::default()).await,
        None
    );
}

#[tokio::test]
async fn free_rates_make_any_ending_a_known_nothing() {
    let shop = Shop {
        rates: vec![("m", ModelPricing::flat(0.0, 0.0))],
        ..Shop::default()
    };
    let settled = settle(&shop, Ended::CutOff, &Traced::default())
        .await
        .unwrap();
    assert_eq!(settled.cost, CallCost::Computed(0.0));
}

#[tokio::test]
async fn usage_the_provider_finished_with_is_priced_as_a_replys_is() {
    let seen = Traced {
        usage: Some(TokenUsage::new(1_000_000, 0, 0, 1_000_000)),
        whole: true,
        call_id: Some("r1".into()),
        ..Traced::default()
    };
    let error = ProviderError::InvalidResponse("answered without audio".into());
    let settled = settle(&priced(), Ended::Failed(&error), &seen)
        .await
        .unwrap();
    assert_eq!(settled.cost, CallCost::Computed(3.0));
    assert_eq!(settled.usage.completion_tokens, 1_000_000);
    assert_eq!(settled.call_id.as_deref(), Some("r1"));
}

#[tokio::test]
async fn a_record_that_arrives_is_the_calls_cost() {
    let shop = Shop {
        keeps_records: true,
        record: Mutex::new(vec![
            Err(ProviderError::ApiError("HTTP 404".into())),
            Ok(TokenUsage::new(14, 0, 0, 9).with_reported_cost(Some(0.000_005))),
        ]),
        ..priced()
    };
    let settled = settle(&shop, Ended::CutOff, &seen_id("gen-1"))
        .await
        .unwrap();
    assert_eq!(settled.cost, CallCost::Reported(0.000_005));
    assert_eq!(settled.usage.prompt_tokens, 14);
    assert_eq!(*shop.asked.lock().unwrap(), 2, "asked again after the 404");
}

#[tokio::test]
async fn a_record_that_never_arrives_leaves_the_cost_unknown() {
    let shop = Shop {
        keeps_records: true,
        ..priced()
    };
    let seen = Traced {
        usage: Some(TokenUsage::new(7, 0, 0, 0)),
        ..seen_id("gen-2")
    };
    let settled = settle(&shop, Ended::CutOff, &seen).await.unwrap();
    assert_eq!(
        settled.cost,
        CallCost::Unknown(CostUnknown::LookupUnanswered)
    );
    assert_eq!(
        settled.usage.prompt_tokens, 7,
        "the part that arrived is kept"
    );
    assert!(*shop.asked.lock().unwrap() >= 2, "asked more than once");
}

#[tokio::test]
async fn a_provider_with_no_record_leaves_a_begun_call_unknown() {
    let error = ProviderError::ApiError("HTTP 500 mid job".into());
    let settled = settle(&priced(), Ended::Failed(&error), &seen_id("task-1"))
        .await
        .unwrap();
    assert_eq!(settled.cost, CallCost::Unknown(CostUnknown::NoLookup));
}

#[tokio::test]
async fn a_refusal_that_says_nothing_ran_costs_nothing() {
    for error in [
        ProviderError::RateLimitExceeded {
            retry_after_secs: None,
        },
        ProviderError::ApiError("HTTP 404 no such model".into()),
        ProviderError::Unavailable {
            reason: crate::provider::UnavailableReason::CreditsExhausted,
            detail: "HTTP 402 out of credits".into(),
        },
    ] {
        let settled = settle(&priced(), Ended::Failed(&error), &Traced::default())
            .await
            .unwrap();
        assert_eq!(settled.cost, CallCost::Computed(0.0), "{error}");
        assert_eq!(settled.usage.total_tokens, 0);
    }
}

#[tokio::test]
async fn any_other_error_answer_is_free_only_where_the_provider_says_so() {
    let error = ProviderError::ApiError("HTTP 500 upstream fell over".into());
    let plain = settle(&priced(), Ended::Failed(&error), &Traced::default())
        .await
        .unwrap();
    assert_eq!(plain.cost, CallCost::Unknown(CostUnknown::NoLookup));

    let generous = Shop {
        free_errors: true,
        ..priced()
    };
    let settled = settle(&generous, Ended::Failed(&error), &Traced::default())
        .await
        .unwrap();
    assert_eq!(settled.cost, CallCost::Computed(0.0));

    // Not a status the provider sent: its terms about error answers say
    // nothing of it.
    let dropped = ProviderError::labelled(
        crate::FailureKind::ConnectionDropped,
        "reading the stream",
        "reset",
    )
    .after_send();
    let settled = settle(&generous, Ended::Failed(&dropped), &Traced::default())
        .await
        .unwrap();
    assert_eq!(settled.cost, CallCost::Unknown(CostUnknown::NoLookup));
}

#[tokio::test]
async fn a_call_cut_off_with_nothing_to_go_on_is_unknown() {
    let settled = settle(&priced(), Ended::CutOff, &Traced::default())
        .await
        .unwrap();
    assert_eq!(settled.cost, CallCost::Unknown(CostUnknown::NoLookup));
    assert_eq!(settled.call_id, None);
}

#[test]
fn a_reply_is_priced_by_the_model_that_answered_when_it_has_a_price() {
    let shop = Shop {
        rates: vec![
            ("alias", ModelPricing::flat(1.0, 1.0)),
            ("dated", ModelPricing::flat(3.0, 3.0)),
        ],
        ..Shop::default()
    };
    let mut usage = TokenUsage::new(1, 0, 0, 1);
    assert_eq!(
        pricing_for(&shop, "alias", &usage).map(|r| r.input_per_mtok),
        Some(1.0),
        "no model named: the one asked for"
    );
    usage.terms.model = Some("dated".into());
    assert_eq!(
        pricing_for(&shop, "alias", &usage).map(|r| r.input_per_mtok),
        Some(3.0)
    );
    usage.terms.model = Some("a-local-servers-own-name".into());
    assert_eq!(
        pricing_for(&shop, "alias", &usage).map(|r| r.input_per_mtok),
        Some(1.0),
        "a name with no price falls back to the one asked for"
    );
    usage.terms.model = Some("alias".into());
    assert_eq!(
        pricing_for(&shop, "alias", &usage).map(|r| r.input_per_mtok),
        Some(1.0)
    );
}

#[test]
fn a_call_that_sent_its_request_twice_has_no_whole_figure() {
    let usage = TokenUsage::new(1_000_000, 0, 0, 0);
    let (cost, pricing) = answered(&priced(), "m", &usage, &Traced::default());
    assert_eq!(cost, CallCost::Computed(1.0));
    assert!(pricing.is_some());

    let resent = Traced {
        resent: true,
        ..Traced::default()
    };
    let (cost, _) = answered(&priced(), "m", &usage, &resent);
    assert_eq!(cost, CallCost::Unknown(CostUnknown::NoLookup));

    // On rates that charge nothing, twice nothing is nothing.
    let free = Shop {
        rates: vec![("m", ModelPricing::flat(0.0, 0.0))],
        ..Shop::default()
    };
    let (cost, _) = answered(&free, "m", &usage, &resent);
    assert_eq!(cost, CallCost::Computed(0.0));
}

/// The whole lookup ends at its bound: a request still out when the bound
/// passes is dropped there, however long it would have taken to answer.
#[tokio::test(start_paused = true)]
async fn a_lookup_ends_at_its_bound_with_a_request_still_out() {
    let shop = Shop {
        keeps_records: true,
        slow: Duration::from_secs(30),
        ..priced()
    };
    let began = tokio::time::Instant::now();
    let settled = unanswered(
        &shop,
        "m",
        Ended::CutOff,
        &seen_id("gen-1"),
        LOOKUP_PATIENCE,
        LOOKUP_INTERVAL,
    )
    .await
    .unwrap();
    assert_eq!(
        settled.cost,
        CallCost::Unknown(CostUnknown::LookupUnanswered)
    );
    assert_eq!(began.elapsed(), LOOKUP_PATIENCE);
    assert_eq!(
        *shop.asked.lock().unwrap(),
        1,
        "one request, cut at the bound"
    );
}

/// A status named inside a reply that had begun is no refusal: the provider
/// accepted the request, and what it charged before the error is not known.
#[tokio::test]
async fn a_status_inside_a_reply_that_began_is_not_a_refusal() {
    let shop = Shop {
        free_errors: true,
        ..priced()
    };
    let error = ProviderError::ApiError("HTTP 429: rate limited mid-stream".into());
    let opened = Traced {
        began: true,
        ..Traced::default()
    };
    let streamed = Traced {
        usage: Some(TokenUsage::new(5, 0, 0, 2)),
        ..Traced::default()
    };
    for seen in [opened, streamed] {
        let settled = settle(&shop, Ended::Failed(&error), &seen).await.unwrap();
        assert_eq!(settled.cost, CallCost::Unknown(CostUnknown::NoLookup));
    }
    // The same status with nothing begun is the refusal it looks like.
    let settled = settle(&shop, Ended::Failed(&error), &Traced::default())
        .await
        .unwrap();
    assert_eq!(settled.cost, CallCost::Computed(0.0));
}

/// A request that went out twice may have been billed twice, so nothing the
/// call ended with makes its cost known: not a refusal, not the usage the
/// second send finished with, not a record of the second send.
#[tokio::test]
async fn a_request_sent_twice_is_never_a_known_figure() {
    let resent = Traced {
        resent: true,
        ..Traced::default()
    };
    let refused = ProviderError::ApiError("HTTP 429: slow down".into());
    let settled = settle(&priced(), Ended::Failed(&refused), &resent)
        .await
        .unwrap();
    assert_eq!(settled.cost, CallCost::Unknown(CostUnknown::NoLookup));

    let finished = Traced {
        usage: Some(TokenUsage::new(1_000_000, 0, 0, 0)),
        whole: true,
        ..resent.clone()
    };
    let settled = settle(&priced(), Ended::CutOff, &finished).await.unwrap();
    assert_eq!(settled.cost, CallCost::Unknown(CostUnknown::NoLookup));
    assert_eq!(settled.usage.prompt_tokens, 1_000_000, "its usage is kept");

    let shop = Shop {
        keeps_records: true,
        record: Mutex::new(vec![Ok(TokenUsage::new(1, 0, 0, 1))]),
        ..priced()
    };
    let named = Traced {
        call_id: Some("gen-2".into()),
        ..resent.clone()
    };
    let settled = settle(&shop, Ended::CutOff, &named).await.unwrap();
    assert_eq!(settled.cost, CallCost::Unknown(CostUnknown::NoLookup));
    assert_eq!(
        *shop.asked.lock().unwrap(),
        0,
        "no record covers both sends"
    );

    // On rates that charge nothing, twice nothing is nothing.
    let free = Shop {
        rates: vec![("m", ModelPricing::flat(0.0, 0.0))],
        ..Shop::default()
    };
    let settled = settle(&free, Ended::Failed(&refused), &resent)
        .await
        .unwrap();
    assert_eq!(settled.cost, CallCost::Computed(0.0));
}

/// A stream the provider opened has begun, so a status it names part way
/// through settles as unknown: tokens had been streamed, and nothing says
/// what they were charged.
#[tokio::test]
async fn a_stream_that_opened_then_named_a_status_is_unknown() {
    use crate::provider::StreamChunk;
    let trace = crate::CallTrace::new();
    let chunks: Vec<Result<StreamChunk>> = vec![
        Ok(StreamChunk {
            reasoning_delta: None,
            delta: "Hel".to_string(),
            tool_calls: Vec::new(),
            tokens: None,
            finish_reason: None,
            reasoning: None,
            parts: Vec::new(),
        }),
        Err(ProviderError::ApiError(
            "HTTP 429: upstream rate limit".into(),
        )),
    ];
    let failed = trace
        .scope(crate::collect_stream(Box::pin(tokio_stream::iter(chunks))))
        .await
        .unwrap_err();
    assert!(
        failed.refused_unrun(),
        "the status alone reads as a refusal"
    );
    let settled = settle(&priced(), Ended::Failed(&failed), &trace.seen())
        .await
        .unwrap();
    assert_eq!(settled.cost, CallCost::Unknown(CostUnknown::NoLookup));
}
