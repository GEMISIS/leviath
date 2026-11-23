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

/// A shoot stage that named a region holding a reference picture as its
/// prompt was refused for want of one: the one-prompt request's text held the
/// picture's stand-in, and was read as the picture's pointer.
#[test]
fn a_job_whose_prompt_region_holds_a_picture_still_has_its_prompt() {
    use crate::provider::{PROMPT_REGIONS_KEY, SystemBlock};
    use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part};
    let blob = Blob::new(MimeType::parse("image/png").unwrap(), b"PNG".to_vec()).named("porch.png");
    let part = Part::stored(blob.describe(&MimeRegistry::builtin())).named("porch.png");
    let part = part.blob().unwrap().clone();
    let stand_in = part.stand_in.clone();
    let region = |name: &str, text: &str| SystemBlock {
        text: format!("## {name}\n{text}"),
        cache_hint: leviath_core::CacheHint::Always,
        region: name.into(),
        volatility: leviath_core::Volatility::Stable,
    };
    let mut shoot = request("google/veo-3.1", "", video_stage());
    shoot.system = vec![
        region("shot", "the camera pushes in on the porch"),
        region("first_frame", &format!("the opening frame\n{stand_in}")),
    ];
    shoot.messages[0].content = MessageContent::Blocks(vec![
        ContentBlock::Text {
            text: format!("[first_frame] {stand_in}"),
        },
        ContentBlock::Mime {
            part,
            data: "UE5H".into(),
            name: Some("porch.png".into()),
            deliver: None,
            remote: None,
        },
    ]);
    shoot.extra[PROMPT_REGIONS_KEY] = json!(["shot", "first_frame"]);
    let body = job_body("google/veo-3.1", &crate::prompt::prompt_only(shoot)).unwrap();
    assert_eq!(
        body["prompt"],
        "the camera pushes in on the porch\nthe opening frame"
    );
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

/// A video stage carrying one picture per name in `pictures`, each picture's
/// base64 the name itself.
fn pictured(model: &str, pictures: &[&str]) -> InferenceRequest {
    use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part};
    let mut shoot = request(model, "the camera pushes in", video_stage());
    let mut blocks = vec![ContentBlock::Text {
        text: "the camera pushes in".into(),
    }];
    blocks.extend(pictures.iter().map(|name| {
        let blob = Blob::new(MimeType::parse("image/png").unwrap(), b"PNG".to_vec()).named(*name);
        let part = Part::stored(blob.describe(&MimeRegistry::builtin())).named(*name);
        ContentBlock::Mime {
            part: part.blob().unwrap().clone(),
            data: (*name).into(),
            name: Some((*name).into()),
            deliver: None,
            remote: None,
        }
    }));
    shoot.messages[0].content = MessageContent::Blocks(blocks);
    shoot
}

/// The url a picture named `name` by [`pictured`] is sent as.
fn url_of(name: &str) -> String {
    format!("data:image/png;base64,{name}")
}

/// A picture as the Videos API takes it, with its frame type when it has one.
fn item(name: &str, frame: Option<&str>) -> Value {
    let mut item = json!({ "type": "image_url", "image_url": { "url": url_of(name) } });
    if let Some(frame) = frame {
        item["frame_type"] = json!(frame);
    }
    item
}

/// The video listing as the public endpoint had it (2026-11-23), trimmed: a
/// model that takes both ends, one that takes a first frame, one that names
/// none, and rows that are not a model's.
fn video_listing() -> (u16, &'static str, Vec<u8>) {
    status(json!({ "data": [
        { "id": "google/veo-3.1", "supported_frame_images": ["first_frame", "last_frame", 7] },
        { "id": "runway/gen-4.5", "supported_frame_images": ["first_frame"] },
        { "id": "runway/aleph-2", "supported_frame_images": null },
        { "name": "nameless", "supported_frame_images": ["first_frame"] },
        { "id": 7 },
    ] }))
}

/// A job that is accepted, finishes on the first look and hands back a clip.
fn made() -> [(u16, &'static str, Vec<u8>); 3] {
    [
        accepted(),
        status(json!({ "status": "completed" })),
        (200, "OK", b"MP4".to_vec()),
    ]
}

/// Where each picture goes, by what the model's listing entry names.
#[test]
fn pictures_go_where_the_models_frame_types_say() {
    let urls = |names: &[&str]| names.iter().map(|n| url_of(n)).collect::<Vec<_>>();
    let frames = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    let carried = |names: &[&str], frames: Option<&[String]>| {
        let mut body = json!({ "model": "m", "prompt": "p" });
        carry_pictures(&mut body, &urls(names), frames);
        body
    };
    let both = frames(&["first_frame", "last_frame"]);
    let first = frames(&["first_frame"]);
    let last = frames(&["last_frame"]);

    // No picture: the body is untouched, whatever the model takes.
    for known in [None, Some(&both[..]), Some(&[][..])] {
        assert_eq!(carried(&[], known), json!({ "model": "m", "prompt": "p" }));
    }
    // One picture is the first frame.
    let body = carried(&["a"], Some(&both));
    assert_eq!(
        body["frame_images"],
        json!([item("a", Some("first_frame"))])
    );
    assert!(body.get("input_references").is_none(), "{body}");
    // Several: the first and the last, and none in between.
    let body = carried(&["a", "b", "c"], Some(&both));
    assert_eq!(
        body["frame_images"],
        json!([
            item("a", Some("first_frame")),
            item("c", Some("last_frame"))
        ])
    );
    assert!(body.get("input_references").is_none(), "{body}");
    // A model that takes a first frame alone gets the first picture alone.
    let body = carried(&["a", "b"], Some(&first));
    assert_eq!(
        body["frame_images"],
        json!([item("a", Some("first_frame"))])
    );
    // So does one the listing does not describe.
    let body = carried(&["a", "b"], None);
    assert_eq!(
        body["frame_images"],
        json!([item("a", Some("first_frame"))])
    );
    // A model that takes no first frame gets every picture as a reference.
    for known in [&[][..], &last[..]] {
        let body = carried(&["a", "b"], Some(known));
        assert_eq!(
            body["input_references"],
            json!([item("a", None), item("b", None)])
        );
        assert!(body.get("frame_images").is_none(), "{body}");
    }
}

/// The listing is read into each model's frame types, and a bare name finds
/// the entry its vendor prefix hides.
#[test]
fn the_video_listing_names_each_models_frame_types() {
    let (_, _, listing) = video_listing();
    let support = frame_support(&serde_json::from_slice(&listing).unwrap()).unwrap();
    assert_eq!(support.len(), 3);
    let of = |model: &str| frames_of(&support, model).map(<[String]>::to_vec);
    assert_eq!(
        of("google/veo-3.1"),
        Some(vec!["first_frame".into(), "last_frame".into()])
    );
    assert_eq!(of("veo-3.1"), of("google/veo-3.1"));
    assert_eq!(of("runway/aleph-2"), Some(vec![]));
    assert_eq!(of("acme/unlisted"), None);
    assert_eq!(of("unlisted"), None);
    let err = frame_support(&json!({ "models": [] })).unwrap_err();
    assert!(err.to_string().contains("data"), "{err}");
}

/// A stage's pictures reach the job, placed by the listing, which is read
/// once however many jobs follow.
#[tokio::test]
async fn a_jobs_pictures_are_sent_as_the_listing_says_the_model_takes_them() {
    let mut sequence = vec![video_listing()];
    sequence.extend(made());
    sequence.extend(made());
    sequence.extend(made());
    let (url, bodies) = spawn_mock_sequence(sequence).await;
    let gateway = provider(&url);
    // The main listing names each as a model that makes video alone.
    gateway.learned.replace(
        ["google/veo-3.1", "runway/gen-4.5", "runway/aleph-2"]
            .into_iter()
            .map(|id| {
                let makes_video = LearnedModel {
                    output_types: Some(vec!["video/*".into()]),
                    ..Default::default()
                };
                (id.to_string(), makes_video)
            })
            .collect(),
    );
    for (model, pictures) in [
        ("google/veo-3.1", &["a", "b", "c"][..]),
        ("runway/gen-4.5", &["a", "b"]),
        ("runway/aleph-2", &["a", "b"]),
    ] {
        gateway
            .infer(&pictured(model, pictures))
            .await
            .expect("a video");
    }
    let bodies = bodies.lock().unwrap().clone();
    let body = |i: usize| serde_json::from_str::<Value>(&bodies[i]).unwrap();
    assert_eq!(bodies[0], "", "the listing is read first");
    assert_eq!(
        body(1),
        json!({
            "model": "google/veo-3.1",
            "prompt": "the camera pushes in",
            "frame_images": [item("a", Some("first_frame")), item("c", Some("last_frame"))],
        })
    );
    assert_eq!(
        body(4)["frame_images"],
        json!([item("a", Some("first_frame"))])
    );
    assert_eq!(
        body(7)["input_references"],
        json!([item("a", None), item("b", None)])
    );
    assert!(body(7).get("frame_images").is_none());
}

/// A listing that cannot be read does not fail the job: the first picture
/// starts the clip, and the next job asks for the listing again.
#[tokio::test]
async fn an_unreadable_video_listing_sends_the_first_picture_as_the_first_frame() {
    let mut sequence = vec![(500, "Internal Server Error", b"{}".to_vec())];
    sequence.extend(made());
    sequence.push(video_listing());
    sequence.extend(made());
    let (url, bodies) = spawn_mock_sequence(sequence).await;
    let gateway = provider(&url);
    let shoot = pictured("google/veo-3.1", &["a", "b"]);
    gateway.infer(&shoot).await.expect("a video");
    gateway.infer(&shoot).await.expect("a video");
    let bodies = bodies.lock().unwrap().clone();
    let frames =
        |i: usize| serde_json::from_str::<Value>(&bodies[i]).unwrap()["frame_images"].clone();
    assert_eq!(frames(1), json!([item("a", Some("first_frame"))]));
    assert_eq!(
        frames(5),
        json!([
            item("a", Some("first_frame")),
            item("b", Some("last_frame"))
        ])
    );
}
