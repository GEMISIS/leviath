//! The gateway's video jobs over local mocks, in the shapes measured live.

use super::*;
use crate::learned::LearnedModel;
use crate::provider::{ContentBlock, Message, MessageContent, Provider};
use leviath_testkit::spawn_mock_sequence;

const OMNI: &str = "google/gemini-omni-1.2-flash";

fn request(model: &str, text: &str, extra: Value) -> InferenceRequest {
    InferenceRequest {
        system: vec![],
        messages: vec![Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![ContentBlock::Text { text: text.into() }]),
            cache_breakpoint: false,
            reasoning: None,
        }],
        model: model.into(),
        max_tokens: 100,
        temperature: 0.5,
        tools: vec![],
        extra,
        request_timeout_secs: Some(5),
    }
}

fn video_stage() -> Value {
    json!({ "output_mimes": ["video/mp4"] })
}

fn provider(url: &str) -> OpenRouterProvider {
    let mut provider = OpenRouterProvider::new(reqwest::Client::new(), "or-key".into())
        .with_base_url(Some(url.into()));
    provider.video_poll = Duration::from_millis(1);
    provider
}

fn accepted() -> (u16, &'static str, Vec<u8>) {
    (
        202,
        "Accepted",
        br#"{"id":"job1","status":"pending"}"#.to_vec(),
    )
}

fn status(body: Value) -> (u16, &'static str, Vec<u8>) {
    (200, "OK", body.to_string().into_bytes())
}

#[test]
fn a_video_stage_on_a_model_that_makes_video_is_a_job() {
    let gateway = provider("http://127.0.0.1:1");
    // The table knows the omni models make text and video: a job when the
    // stage asks for video, a chat turn when it asks for text.
    assert!(gateway.is_video_job(&request(OMNI, "a boat", video_stage())));
    assert!(!gateway.is_video_job(&request(OMNI, "a boat", Value::Null)));
    assert!(!gateway.is_video_job(&request(
        OMNI,
        "a boat",
        json!({ "output_mimes": ["text/markdown"] })
    )));
    // A chat model is never one, whatever the stage declares.
    assert!(!gateway.is_video_job(&request(
        "anthropic/claude-sonnet-5",
        "a boat",
        video_stage()
    )));
    // A model the listing says makes video and nothing else always is.
    gateway.learned.replace(std::collections::HashMap::from([(
        "acme/clips-1".to_string(),
        LearnedModel {
            output_types: Some(vec!["video/*".into()]),
            ..Default::default()
        },
    )]));
    assert!(gateway.is_video_job(&request("acme/clips-1", "a boat", Value::Null)));

    assert!(!gateway.capabilities(OMNI).supports_tools);
    assert!(!gateway.capabilities("acme/clips-1").supports_tools);
    assert!(
        gateway
            .capabilities("anthropic/claude-sonnet-5")
            .supports_tools
    );
}

#[tokio::test]
async fn a_video_is_started_polled_downloaded_and_priced_as_reported() {
    let (url, bodies) = spawn_mock_sequence(vec![
        accepted(),
        status(json!({ "id": "job1", "status": "pending" })),
        status(json!({ "id": "job1", "status": "in_progress" })),
        status(json!({ "id": "job1", "status": "completed", "usage": { "cost": 0.0487128 } })),
        (200, "OK", b"MP4".to_vec()),
    ])
    .await;
    let mut extra = video_stage();
    extra["duration"] = json!("4s");
    extra["resolution"] = json!("480p");
    extra["aspect_ratio"] = json!("16:9");
    extra["size"] = json!("854x480");
    extra["seed"] = json!(7);
    extra["generate_audio"] = json!(false);
    let response = provider(&url)
        .infer(&request(OMNI, "a paper boat", extra))
        .await
        .expect("a video");
    assert_eq!(response.parts[0].bytes, b"MP4");
    assert_eq!(response.parts[0].mime_type.as_str(), "video/mp4");
    assert_eq!(response.parts[0].name.as_deref(), Some("video.mp4"));
    assert_eq!(response.tokens_used.reported_cost_usd, Some(0.0487128));
    assert!(response.content.contains(OMNI), "{}", response.content);
    let body: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(
        body,
        json!({
            "model": OMNI,
            "prompt": "a paper boat",
            "duration": 4,
            "resolution": "480p",
            "aspect_ratio": "16:9",
            "size": "854x480",
            "seed": 7,
            "generate_audio": false,
        })
    );
}

/// A job run on the account's own upstream key reports OpenRouter's fee
/// alone, which is not what the clip cost.
#[tokio::test]
async fn a_video_made_on_the_accounts_own_key_has_no_whole_figure() {
    let (url, _) = spawn_mock_sequence(vec![
        accepted(),
        status(json!({ "id": "job1", "status": "completed",
            "usage": { "cost": 0.01, "is_byok": true } })),
        (200, "OK", b"MP4".to_vec()),
    ])
    .await;
    let response = provider(&url)
        .infer(&request(OMNI, "a paper boat", video_stage()))
        .await
        .expect("a video");
    assert_eq!(
        response.tokens_used.cost(None),
        leviath_core::cost::CallCost::Unknown(leviath_core::cost::CostUnknown::UnmodelledTerms)
    );
}

/// The streaming path hands the clip on in its one chunk, and a job that
/// reports no cost is recorded without one.
#[tokio::test]
async fn a_streamed_video_arrives_in_one_chunk() {
    use tokio_stream::StreamExt as _;
    let (url, bodies) = spawn_mock_sequence(vec![
        accepted(),
        status(json!({ "status": "completed" })),
        (200, "OK", b"MP4".to_vec()),
    ])
    .await;
    let mut stream = provider(&url)
        .infer_stream(&request(OMNI, "a paper boat", video_stage()))
        .await
        .expect("a stream");
    let chunk = stream.next().await.expect("one chunk").expect("a video");
    assert_eq!(chunk.parts[0].bytes, b"MP4");
    assert_eq!(chunk.tokens.unwrap().reported_cost_usd, None);
    assert!(stream.next().await.is_none());
    let body: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(body, json!({ "model": OMNI, "prompt": "a paper boat" }));
}

#[tokio::test]
async fn a_job_that_fails_at_any_step_is_an_error() {
    let run = |steps: Vec<(u16, &'static str, Vec<u8>)>| async move {
        let (url, _) = spawn_mock_sequence(steps).await;
        provider(&url)
            .infer(&request(OMNI, "a boat", video_stage()))
            .await
            .unwrap_err()
            .to_string()
    };
    // The job is refused, or answers no id.
    run(vec![(400, "Bad Request", b"{}".to_vec())]).await;
    let said = run(vec![(202, "Accepted", b"{}".to_vec())]).await;
    assert!(said.contains("answered no id"), "{said}");
    // It ends failed, with the gateway's reason or without one.
    for (ended, expected) in [
        (
            json!({ "status": "failed", "error": "Video generation failed" }),
            "Video generation failed",
        ),
        (json!({ "status": "cancelled" }), "no reason given"),
        (json!({ "status": "expired" }), "no reason given"),
    ] {
        let said = run(vec![accepted(), status(ended)]).await;
        assert!(said.contains(expected), "{said}");
    }
    // The job cannot be read, or the clip cannot be fetched.
    run(vec![accepted()]).await;
    run(vec![
        accepted(),
        status(json!({ "status": "completed" })),
        (404, "Not Found", b"gone".to_vec()),
    ])
    .await;
    run(vec![accepted(), status(json!({ "status": "completed" }))]).await;
}

/// A prompt or a length that does not read is refused before anything is
/// sent.
#[tokio::test]
async fn a_job_with_no_prompt_or_an_unreadable_length_is_refused_unsent() {
    let nothing_listens = provider("http://127.0.0.1:9");
    let err = nothing_listens
        .infer(&request(OMNI, "", video_stage()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("needs a prompt"), "{err}");
    assert_eq!(err.request_sent(), Some(false));
    let mut extra = video_stage();
    extra["duration"] = json!("four");
    let err = nothing_listens
        .infer(&request(OMNI, "a boat", extra))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("`duration`"), "{err}");
}

/// A call made again after a restart polls the job it started before and
/// starts nothing.
#[tokio::test]
async fn a_call_made_again_polls_the_job_it_started() {
    let (url, bodies) = spawn_mock_sequence(vec![
        status(json!({ "status": "completed" })),
        (200, "OK", b"MP4".to_vec()),
    ])
    .await;
    let log =
        crate::jobs::JobLog::new([(format!("openrouter/{OMNI}/video"), "job1".to_string())].into());
    let response = log
        .scope(provider(&url).infer(&request(OMNI, "a paper boat", video_stage())))
        .await
        .expect("the video it paid for");
    assert_eq!(response.parts[0].bytes, b"MP4");
    assert!(bodies.lock().unwrap().iter().all(String::is_empty));
}
