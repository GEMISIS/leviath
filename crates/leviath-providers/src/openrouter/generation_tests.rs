//! What OpenRouter says a generation was charged, after the call is over:
//! the id it names the generation by, and the record kept under that id.

use super::*;
use crate::call_trace::CallTrace;
use crate::openrouter::OpenRouterProvider;
use crate::provider::{InferenceRequest, Message, Provider};
use leviath_core::cost::{CallCost, CostUnknown};
use tokio_stream::StreamExt;

fn provider(url: String) -> OpenRouterProvider {
    OpenRouterProvider::new(
        crate::provider::build_http_client(None).expect("a test client builds"),
        "key".to_string(),
    )
    .with_base_url(Some(url))
}

fn request() -> InferenceRequest {
    InferenceRequest {
        system: vec![],
        messages: vec![Message {
            role: "user".to_string(),
            content: "hi".into(),
            cache_breakpoint: false,
            reasoning: None,
        }],
        model: "openai/gpt-5.5".to_string(),
        max_tokens: 16,
        temperature: 1.0,
        tools: vec![],
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    }
}

const ANSWER: &[u8] = br#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"cost":0.25}}"#;

/// The generation's id comes in a header beside the status, so a reply names
/// it to whoever is tracing the call, and a reply without the header names
/// nothing.
#[tokio::test]
async fn a_reply_names_its_generation() {
    let url = leviath_testkit::spawn_mock_server_with_headers(
        200,
        "OK",
        "Content-Type: application/json\r\nX-Generation-Id: gen-abc-1\r\n",
        ANSWER,
    )
    .await;
    let trace = CallTrace::new();
    let reply = trace.scope(provider(url).infer(&request())).await;
    assert!(reply.is_ok(), "{reply:?}");
    assert_eq!(trace.seen().call_id.as_deref(), Some("gen-abc-1"));

    let url = leviath_testkit::spawn_mock_server(200, "OK", ANSWER).await;
    let trace = CallTrace::new();
    let reply = trace.scope(provider(url).infer(&request())).await;
    assert!(reply.is_ok(), "{reply:?}");
    assert_eq!(trace.seen().call_id, None);
}

/// A stream names its generation before its first token, which is what lets
/// a stream that is cut be looked up afterwards.
#[tokio::test]
async fn a_stream_names_its_generation_before_any_token() {
    let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    let url = leviath_testkit::spawn_mock_server_with_headers(
        200,
        "OK",
        "Content-Type: text/event-stream\r\nX-Generation-Id: gen-stream-2\r\n",
        sse.to_vec(),
    )
    .await;
    let trace = CallTrace::new();
    let provider = provider(url);
    let request = request();
    trace
        .scope(async {
            let mut stream = provider
                .infer_stream(&request)
                .await
                .expect("the stream opens");
            assert!(stream.next().await.is_some(), "a chunk arrives");
        })
        .await;
    assert_eq!(trace.seen().call_id.as_deref(), Some("gen-stream-2"));
}

/// The record's `total_cost` is what the account was charged, and its
/// counts are the upstream's own, the cached ones taken out of the prompt.
#[test]
fn a_generation_record_reads_as_the_calls_usage_and_charge() {
    let record = serde_json::json!({ "data": {
        "total_cost": 0.00052,
        "model": "openai/gpt-4.1-nano",
        "native_tokens_prompt": 192,
        "native_tokens_cached": 64,
        "native_tokens_completion": 1252,
        "cancelled": true,
    }});
    let usage = usage_of(&record).expect("a record with a charge reads");
    assert_eq!(usage.prompt_tokens, 128);
    assert_eq!(usage.cached_tokens, 64);
    assert_eq!(usage.completion_tokens, 1252);
    assert_eq!(usage.terms.model.as_deref(), Some("openai/gpt-4.1-nano"));
    assert_eq!(usage.cost(None), CallCost::Reported(0.00052));

    // The record's fields with no `data` around them read the same.
    let bare = serde_json::json!({ "total_cost": 0.5 });
    let usage = usage_of(&bare).expect("a bare record reads");
    assert_eq!(usage.cost(None), CallCost::Reported(0.5));
    assert_eq!(usage.terms.model, None);

    // A record with no charge on it says nothing of what the call cost.
    let none = usage_of(&serde_json::json!({ "data": {} }));
    assert!(
        format!("{none:?}").contains("names no total_cost"),
        "{none:?}"
    );
}

/// On the account's own upstream key the charge is OpenRouter's fee and the
/// upstream's price together, and a fee with no upstream price beside it is
/// not the whole cost.
#[test]
fn a_generation_on_the_accounts_own_key_adds_the_upstream_price() {
    let both = serde_json::json!({ "data": {
        "total_cost": 0.01, "is_byok": true, "upstream_inference_cost": 0.2,
    }});
    let cost = usage_of(&both).expect("reads").cost(None);
    assert_eq!(cost.class(), leviath_core::cost::CostClass::Reported);
    let usd = cost.usd().expect("a reported cost is a figure");
    assert!((usd - 0.21).abs() < 1e-12, "{usd}");

    let fee_only = serde_json::json!({ "data": { "total_cost": 0.01, "is_byok": true }});
    assert_eq!(
        usage_of(&fee_only).expect("reads").cost(None),
        CallCost::Unknown(CostUnknown::UnmodelledTerms)
    );
}

/// The lookup is one `GET /generation?id=`: the record when there is one, an
/// error while there is none yet, and no record to ask for under an id that
/// is not a generation's.
#[tokio::test]
async fn a_generation_is_looked_up_by_its_id() {
    let record =
        br#"{"data":{"total_cost":0.125,"native_tokens_prompt":10,"native_tokens_completion":4}}"#;
    let (url, asked) = leviath_testkit::spawn_mock_recorder(200, "OK", record.to_vec()).await;
    let usage = provider(url)
        .call_usage("gen-abc-1", "openai/gpt-5.5")
        .await
        .expect("OpenRouter keeps a record")
        .expect("and it reads");
    assert_eq!(usage.cost(None), CallCost::Reported(0.125));
    assert_eq!(usage.prompt_tokens, 10);
    let asked = leviath_core::sync::lock(&asked).clone();
    assert!(
        asked[0].starts_with("GET /generation?id=gen-abc-1 "),
        "{asked:?}"
    );

    // Not written yet: the caller may ask again.
    let url = leviath_testkit::spawn_mock_server(404, "Not Found", b"{}".to_vec()).await;
    let early = provider(url).call_usage("gen-abc-1", "m").await;
    assert!(early.expect("OpenRouter keeps a record").is_err());

    // Never asked: a job's id, and ids that would not survive a URL.
    for id in ["", "job-1", "gen-1 2", "gen-/../key", "gen-1?x=1"] {
        let unasked = provider("http://127.0.0.1:9".to_string())
            .call_usage(id, "m")
            .await;
        assert!(unasked.is_none(), "{id:?}");
    }
}

/// OpenRouter charges nothing for a request it answered with an error. An
/// error from a proxy at another base URL is the proxy's, and says nothing.
#[test]
fn an_error_answer_is_not_billed_at_openrouters_own_endpoint() {
    let own = OpenRouterProvider::new(
        crate::provider::build_http_client(None).expect("a test client builds"),
        "key".to_string(),
    );
    assert!(own.bills_no_error_answer());
    let slashed = own.with_base_url(Some("https://openrouter.ai/api/v1/".to_string()));
    assert!(slashed.bills_no_error_answer());
    assert!(!provider("http://127.0.0.1:9".to_string()).bills_no_error_answer());
}
