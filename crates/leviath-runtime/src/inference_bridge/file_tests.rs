//! A job whose parts go by file id: uploaded before the call, and uploaded
//! again once when the vendor says a named file is gone.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::blob_store::FsBlobStore;
use crate::inference_pool::{InferencePoolConfig, InferencePools};
use leviath_core::mime::{Blob, BlobStore, MimeRegistry, MimeType, Part};
use leviath_providers::files::{FileUpload, MediaLimits, RemoteFile};
use leviath_providers::{ContentBlock, Message, MessageContent, ModelMime};
use tokio::sync::mpsc;

/// Refuses the first `lost` calls with a missing file, then answers; stores
/// what it is sent.
struct Vendor {
    lost: AtomicUsize,
    uploads: AtomicUsize,
    seen: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Provider for Vendor {
    async fn infer(
        &self,
        request: &InferenceRequest,
    ) -> leviath_providers::Result<InferenceResponse> {
        let named = serde_json::to_string(&request.messages).unwrap();
        self.seen.lock().unwrap().push(named);
        if self.lost.load(Ordering::SeqCst) > 0 {
            self.lost.fetch_sub(1, Ordering::SeqCst);
            return Err(ProviderError::ApiError(
                "HTTP 404: File `file-0` not found.".into(),
            ));
        }
        Ok(InferenceResponse {
            parts: Vec::new(),
            content: "read it".into(),
            tool_calls: vec![],
            tokens_used: leviath_providers::TokenUsage::new(1, 0, 0, 1),
            finish_reason: leviath_providers::FinishReason::Complete,
            reasoning: None,
        })
    }
    async fn count_tokens(&self, _: &str, _: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _: &str) -> usize {
        1_000_000
    }
    fn name(&self) -> &str {
        "vendor"
    }
    fn capabilities(&self, _: &str) -> leviath_providers::ModelCapabilities {
        leviath_providers::ModelCapabilities::default()
    }
    fn media_limits(&self, _: &str) -> MediaLimits {
        leviath_providers::files::provider_limits("anthropic")
    }
    async fn upload_file(&self, _: &FileUpload) -> leviath_providers::Result<RemoteFile> {
        let n = self.uploads.fetch_add(1, Ordering::SeqCst);
        Ok(RemoteFile {
            id: format!("file-{n}"),
            uri: None,
            expires_at: None,
        })
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    vendor: Arc<Vendor>,
    job: InferenceJob,
    _pools: InferencePools,
}

fn setup(lost: usize, with_route: bool) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsBlobStore::new(dir.path().to_path_buf()));
    let registry = Arc::new(MimeRegistry::builtin());
    let blob = Blob::new(
        MimeType::parse("application/pdf").unwrap(),
        b"%PDF-1.7".to_vec(),
    )
    .named("r.pdf");
    let part = Part::stored(store.put("run-1", &blob, &registry).unwrap()).named("r.pdf");
    let vendor = Arc::new(Vendor {
        lost: AtomicUsize::new(lost),
        uploads: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
    });
    let files = with_route.then(|| crate::provider_files::FileRoute {
        provider: vendor.clone(),
        provider_name: "anthropic".into(),
        ledger: store
            .run_dir("run-1")
            .unwrap()
            .join(crate::provider_files::LEDGER_FILE),
        ttl_secs: 3_600,
    });
    let pools = InferencePools::new(InferencePoolConfig::new());
    let job = InferenceJob {
        entity: Entity::from_raw_u32(7).expect("a small literal index is a valid entity id"),
        refused: None,
        provider: vendor.clone(),
        request: InferenceRequest {
            system: vec![],
            messages: vec![Message {
                role: "user".into(),
                content: MessageContent::Blocks(vec![ContentBlock::mime(&part).unwrap()]),
                cache_breakpoint: false,
                reasoning: None,
            }],
            model: "claude".into(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        },
        permit: pools.try_acquire("p", "m").expect("free pool"),
        calibration: None,
        stream: false,
        hydration: Some(JobHydration {
            store,
            run_id: "run-1".into(),
            registry,
            mime: ModelMime::new(&["text/*", "application/pdf"], &["text/*"]),
            max_media_bytes: 1024,
            as_text: Vec::new(),
            limits: leviath_providers::files::provider_limits("anthropic"),
            files,
            why_inline: "",
        }),
    };
    Setup {
        _dir: dir,
        vendor,
        job,
        _pools: pools,
    }
}

async fn run(job: InferenceJob) -> leviath_providers::Result<InferenceResponse> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let policy = RetryPolicy {
        max_attempts: 1,
        ..RetryPolicy::default()
    };
    run_inference_job(
        job,
        tx,
        Arc::new(Notify::new()),
        policy,
        crate::cancel::CancelToken::new(),
    )
    .await;
    rx.try_recv().expect("an outcome").result
}

#[tokio::test]
async fn a_gone_file_is_uploaded_again_and_the_call_retried_once() {
    let Setup {
        vendor,
        job,
        _dir,
        _pools,
    } = setup(1, true);
    let answer = run(job).await.expect("the retry answers");
    assert_eq!(answer.content, "read it");
    assert_eq!(vendor.uploads.load(Ordering::SeqCst), 2);
    let seen = vendor.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen[0].contains("file-0"), "{}", seen[0]);
    assert!(seen[1].contains("file-1"), "{}", seen[1]);
}

#[tokio::test]
async fn a_file_gone_twice_is_the_error_and_no_route_means_no_renewal() {
    let Setup {
        vendor,
        job,
        _dir,
        _pools,
    } = setup(2, true);
    let err = run(job).await.unwrap_err();
    assert!(err.to_string().contains("not found"), "{err}");
    assert_eq!(
        vendor.uploads.load(Ordering::SeqCst),
        2,
        "renewed once only"
    );

    let Setup {
        vendor,
        job,
        _dir,
        _pools,
    } = setup(1, false);
    assert!(run(job).await.is_err());
    assert_eq!(vendor.uploads.load(Ordering::SeqCst), 0);
    assert!(
        vendor.seen.lock().unwrap()[0].contains("\"data\""),
        "with no route the part went inline"
    );
}

#[tokio::test]
async fn the_trait_obligations_of_the_fake_hold() {
    let Setup {
        vendor,
        _dir,
        _pools,
        ..
    } = setup(0, false);
    assert_eq!(vendor.count_tokens("", "").await, 1);
    assert_eq!(vendor.max_context_tokens(""), 1_000_000);
    assert_eq!(vendor.name(), "vendor");
    let _ = vendor.capabilities("");
}
