//! Muse Spark's audio goes only by uploaded file: Meta drops an inline
//! `input_audio` part unread (meta-model-cookbook#59), so an audio part that
//! is not uploaded is refused with its reason, and every other part is sent
//! as it always was.

use std::sync::Mutex;

use super::*;
use crate::blob_store::FsBlobStore;
use leviath_core::mime::{Blob, BlobStore, MimeRegistry, MimeType, Part};
use leviath_providers::files::{FileUpload, MediaLimits, RemoteFile};
use leviath_providers::{ContentBlock, Message, MessageContent};

/// Meta's documented limits, with an upload that works or fails.
struct Meta {
    fail: bool,
    uploads: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Provider for Meta {
    async fn infer(&self, _: &InferenceRequest) -> leviath_providers::Result<InferenceResponse> {
        Err(ProviderError::ApiError("not called".into()))
    }
    async fn count_tokens(&self, _: &str, _: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _: &str) -> usize {
        1_000_000
    }
    fn name(&self) -> &str {
        "meta"
    }
    fn capabilities(&self, _: &str) -> leviath_providers::ModelCapabilities {
        leviath_providers::ModelCapabilities::default()
    }
    fn media_limits(&self, _: &str) -> MediaLimits {
        leviath_providers::files::provider_limits("meta")
    }
    async fn upload_file(&self, upload: &FileUpload) -> leviath_providers::Result<RemoteFile> {
        if self.fail {
            return Err(ProviderError::ApiError("HTTP 500: storage is down".into()));
        }
        let mut uploads = self.uploads.lock().unwrap();
        uploads.push(upload.mime_type.clone());
        Ok(RemoteFile {
            id: format!("file-{}", uploads.len()),
            uri: None,
            expires_at: None,
        })
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    meta: Arc<Meta>,
    hydration: JobHydration,
    request: InferenceRequest,
}

/// A request carrying a small WAV part, a small MP3 part and a small PNG
/// part for `muse-spark-1.2-contributor`, the model the bug was reported on.
/// `route` gives the run an upload route; `why_inline` is what the settings
/// say when it has none.
fn setup(fail: bool, route: bool, why_inline: &'static str) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsBlobStore::new(dir.path().to_path_buf()));
    let registry = Arc::new(MimeRegistry::builtin());
    let stored = |mime: &str, bytes: &[u8], name: &str| {
        let blob = Blob::new(MimeType::parse(mime).unwrap(), bytes.to_vec()).named(name);
        let part = Part::stored(store.put("run-1", &blob, &registry).unwrap()).named(name);
        ContentBlock::mime(&part).unwrap()
    };
    let blocks = vec![
        ContentBlock::Text {
            text: "transcribe these".into(),
        },
        stored("audio/wav", b"RIFF-small-wav", "clip.wav"),
        stored("audio/mpeg", b"ID3-small-mp3", "clip.mp3"),
        stored("image/png", b"\x89PNG-small", "still.png"),
    ];
    let meta = Arc::new(Meta {
        fail,
        uploads: Mutex::new(Vec::new()),
    });
    let model = "muse-spark-1.2-contributor";
    let files = route.then(|| crate::provider_files::FileRoute {
        provider: meta.clone(),
        provider_name: "meta".into(),
        ledger: store
            .run_dir("run-1")
            .unwrap()
            .join(crate::provider_files::LEDGER_FILE),
        ttl_secs: 3_600,
    });
    let hydration = JobHydration {
        store,
        run_id: "run-1".into(),
        registry,
        mime: leviath_providers::mime_tables::builtin_mime("meta", model),
        // Room for every part inline: nothing here is held back by size.
        max_media_bytes: 1024 * 1024,
        as_text: Vec::new(),
        limits: leviath_providers::files::provider_limits("meta"),
        files,
        why_inline,
    };
    let request = InferenceRequest {
        system: vec![],
        messages: vec![Message {
            role: "user".into(),
            content: MessageContent::Blocks(blocks),
            cache_breakpoint: false,
            reasoning: None,
        }],
        model: model.into(),
        max_tokens: 100,
        temperature: 0.0,
        tools: vec![],
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    };
    Setup {
        _dir: dir,
        meta,
        hydration,
        request,
    }
}

fn blocks(request: &InferenceRequest) -> &[ContentBlock] {
    match &request.messages[0].content {
        MessageContent::Blocks(blocks) => blocks,
        MessageContent::Text(_) => &[],
    }
}

/// Every content part as Meta's Responses route writes it.
fn wire(request: &InferenceRequest) -> Vec<serde_json::Value> {
    blocks(request)
        .iter()
        .filter_map(leviath_providers::mime::responses_part)
        .collect()
}

#[tokio::test]
async fn every_muse_spark_audio_part_goes_by_file_id_however_small() {
    let Setup {
        meta,
        hydration,
        mut request,
        _dir,
    } = setup(false, true, "");
    hydration.apply(&mut request).await;
    let parts = wire(&request);
    let audio: Vec<_> = parts
        .iter()
        .filter(|p| p["type"] == "input_audio")
        .collect();
    assert_eq!(audio.len(), 2, "{parts:?}");
    for part in audio {
        assert!(part["file_id"].is_string(), "{part}");
        assert!(part.get("input_audio").is_none(), "never inline: {part}");
        assert!(part.get("audio_url").is_none(), "never a data uri: {part}");
    }
    // The image is uploaded too, as every part this route can take by file
    // always was.
    let uploads = meta.uploads.lock().unwrap();
    assert_eq!(*uploads, ["audio/wav", "audio/mpeg", "image/png"]);
}

#[tokio::test]
async fn audio_that_cannot_be_uploaded_is_refused_and_never_sent_inline() {
    // Uploads switched off: the audio is refused with that reason, and the
    // image goes inline as it always did.
    let Setup {
        meta,
        hydration,
        mut request,
        _dir,
    } = setup(false, false, "[providers] file_uploads is off");
    hydration.apply(&mut request).await;
    assert!(meta.uploads.lock().unwrap().is_empty());
    let parts = wire(&request);
    assert!(
        !parts.iter().any(|p| p["type"] == "input_audio"),
        "{parts:?}"
    );
    let image = parts
        .iter()
        .find(|p| p["type"] == "input_image")
        .expect("the image is still sent");
    assert!(
        image["image_url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png")
    );
    let refusals: Vec<&str> = blocks(&request)
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } if text.contains("[not sent") => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(refusals.len(), 2, "{refusals:?}");
    for text in refusals {
        assert!(text.contains("only as an uploaded file"), "{text}");
        assert!(text.contains("meta-model-cookbook#59"), "{text}");
        assert!(text.contains("file_uploads is off"), "{text}");
    }

    // An upload that fails: the same refusal, saying so, and still nothing
    // inline.
    let Setup {
        meta,
        hydration,
        mut request,
        _dir,
    } = setup(true, true, "");
    hydration.apply(&mut request).await;
    assert!(meta.uploads.lock().unwrap().is_empty());
    let parts = wire(&request);
    assert!(
        !parts.iter().any(|p| p["type"] == "input_audio"),
        "{parts:?}"
    );
    let image = parts
        .iter()
        .find(|p| p["type"] == "input_image")
        .expect("an image whose upload failed goes inline");
    assert!(image["image_url"].is_string());
    assert!(
        blocks(&request).iter().any(|b| matches!(
            b,
            ContentBlock::Text { text } if text.contains("could not be uploaded")
        )),
        "{:?}",
        blocks(&request)
    );
}

#[tokio::test]
async fn the_trait_obligations_of_the_fake_hold() {
    let Setup { meta, _dir, .. } = setup(false, false, "");
    assert!(meta.infer(&setup(false, false, "").request).await.is_err());
    assert_eq!(meta.count_tokens("", "").await, 1);
    assert_eq!(meta.max_context_tokens(""), 1_000_000);
    assert_eq!(meta.name(), "meta");
    let _ = meta.capabilities("");
    assert!(!meta.media_limits("").file_only.is_empty());
    let mut plain = setup(false, false, "").request;
    plain.messages[0].content = MessageContent::Text("hi".into());
    assert!(blocks(&plain).is_empty());
}
