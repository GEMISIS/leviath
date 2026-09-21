//! The upload ledger, over a provider that stores files in memory.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::blob_store::FsBlobStore;
use leviath_core::mime::{Blob, BlobStore, MimeRegistry, MimeType, Part};
use leviath_providers::files::{FileUpload, MediaLimits};
use leviath_providers::{
    InferenceResponse, Message, ModelCapabilities, ProviderError, capabilities::ModelMime,
};

/// A provider that keeps what it is sent, and can be told to refuse.
#[derive(Default)]
struct Storage {
    uploads: AtomicUsize,
    deletes: AtomicUsize,
    refuse_uploads: bool,
    refuse_deletes: bool,
    no_files: bool,
    names: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Provider for Storage {
    async fn infer(&self, _: &InferenceRequest) -> leviath_providers::Result<InferenceResponse> {
        Err(ProviderError::Other("unused".into()))
    }
    async fn count_tokens(&self, _: &str, _: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _: &str) -> usize {
        1_000
    }
    fn name(&self) -> &str {
        "storage"
    }
    fn capabilities(&self, _: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn media_limits(&self, _: &str) -> MediaLimits {
        match self.no_files {
            true => MediaLimits::NONE,
            false => leviath_providers::files::provider_limits("anthropic"),
        }
    }
    async fn upload_file(&self, upload: &FileUpload) -> leviath_providers::Result<RemoteFile> {
        if self.refuse_uploads {
            return Err(ProviderError::ApiError("HTTP 500".into()));
        }
        let n = self.uploads.fetch_add(1, Ordering::SeqCst);
        self.names.lock().unwrap().push(upload.name.clone());
        Ok(RemoteFile {
            id: format!("file-{n}"),
            uri: None,
            expires_at: Some(now_secs() + upload.ttl_secs as i64),
        })
    }
    async fn delete_file(&self, _: &RemoteFile) -> leviath_providers::Result<()> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        match self.refuse_deletes {
            true => Err(ProviderError::ApiError("HTTP 403".into())),
            false => Ok(()),
        }
    }
}

struct Run {
    _dir: tempfile::TempDir,
    store: FsBlobStore,
    run_dir: PathBuf,
    registry: MimeRegistry,
}

fn run() -> Run {
    let dir = tempfile::tempdir().unwrap();
    let store = FsBlobStore::new(dir.path().to_path_buf());
    let run_dir = store.run_dir("run-1").unwrap();
    Run {
        run_dir,
        store,
        _dir: dir,
        registry: MimeRegistry::builtin(),
    }
}

fn stored(run: &Run, mime: &str, bytes: &[u8], name: Option<&str>) -> ContentBlock {
    let mut blob = Blob::new(MimeType::parse(mime).unwrap(), bytes.to_vec());
    if let Some(name) = name {
        blob = blob.named(name);
    }
    let mut part = Part::stored(run.store.put("run-1", &blob, &run.registry).unwrap());
    if let Some(name) = name {
        part = part.named(name);
    }
    ContentBlock::mime(&part).unwrap()
}

fn request(blocks: Vec<ContentBlock>) -> InferenceRequest {
    InferenceRequest {
        system: Vec::new(),
        messages: vec![
            Message {
                role: "user".into(),
                content: MessageContent::Text("plain".into()),
                cache_breakpoint: false,
                reasoning: None,
            },
            Message {
                role: "user".into(),
                content: MessageContent::Blocks(blocks),
                cache_breakpoint: false,
                reasoning: None,
            },
        ],
        model: "claude".into(),
        max_tokens: 10,
        temperature: 0.0,
        tools: Vec::new(),
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    }
}

fn route(run: &Run, provider: Arc<dyn Provider>) -> FileRoute {
    FileRoute {
        provider,
        provider_name: "anthropic".into(),
        ledger: run.run_dir.join(LEDGER_FILE),
        ttl_secs: 3_600,
    }
}

fn remote_ids(request: &InferenceRequest) -> Vec<Option<String>> {
    match &request.messages[1].content {
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|b| match b {
                ContentBlock::Mime { remote, .. } => remote.as_ref().map(|f| f.id.clone()),
                _ => None,
            })
            .collect(),
        MessageContent::Text(_) => Vec::new(),
    }
}

fn vision() -> ModelMime {
    ModelMime::new(&["text/*", "image/*", "application/pdf"], &["text/*"])
}

#[tokio::test]
async fn a_part_uploads_once_and_every_later_request_names_it() {
    let run = run();
    std::fs::create_dir_all(&run.run_dir).unwrap();
    let storage = Arc::new(Storage::default());
    let route = route(&run, storage.clone());
    let pdf = stored(&run, "application/pdf", b"%PDF-1.7 a", Some("re:port?.pdf"));
    let obj = stored(&run, "model/obj", b"v 1 2 3\n", None);
    let mut first = request(vec![
        ContentBlock::Text { text: "t".into() },
        pdf.clone(),
        pdf.clone(),
        obj,
    ]);
    let uploaded = attach(&mut first, &route, &vision(), &run.store, "run-1", false).await;
    assert_eq!(uploaded, 1, "the same part twice is one upload");
    assert_eq!(
        remote_ids(&first),
        [None, Some("file-0".into()), Some("file-0".into()), None]
    );
    assert!(names_files(&first));
    assert_eq!(storage.names.lock().unwrap()[0], "re_port_.pdf");

    let mut second = request(vec![pdf.clone()]);
    assert_eq!(
        attach(&mut second, &route, &vision(), &run.store, "run-1", false).await,
        0
    );
    assert_eq!(remote_ids(&second), [Some("file-0".into())]);

    // The vendor lost it: every named part uploads again, once.
    let mut refused = second.clone();
    refused.messages[1] = Message {
        role: "user".into(),
        content: MessageContent::Blocks(vec![
            match &second.messages[1].content {
                MessageContent::Blocks(b) => b[0].clone(),
                MessageContent::Text(_) => unreachable!(),
            },
            match &second.messages[1].content {
                MessageContent::Blocks(b) => b[0].clone(),
                MessageContent::Text(_) => unreachable!(),
            },
            pdf.clone(),
        ]),
        cache_breakpoint: false,
        reasoning: None,
    };
    assert_eq!(
        attach(&mut refused, &route, &vision(), &run.store, "run-1", true).await,
        1
    );
    assert_eq!(
        remote_ids(&refused),
        [Some("file-1".into()), Some("file-1".into()), None],
        "renewal leaves an unnamed part alone"
    );

    // The run ends: its uploads are deleted and its ledger goes.
    let mut registry = crate::ProviderRegistry::new();
    registry.register("anthropic".into(), storage.clone());
    assert_eq!(forget_run(&run.run_dir, &registry).await, 1);
    assert!(!run.run_dir.join(LEDGER_FILE).exists());
    assert_eq!(forget_run(&run.run_dir, &registry).await, 0, "nothing left");
}

#[tokio::test]
async fn a_failed_upload_goes_inline_and_a_provider_without_files_is_skipped() {
    let run = run();
    let pdf = stored(&run, "application/pdf", b"%PDF-1.7 b", Some("b.pdf"));
    let refusing = Arc::new(Storage {
        refuse_uploads: true,
        ..Storage::default()
    });
    let mut req = request(vec![pdf.clone()]);
    assert_eq!(
        attach(
            &mut req,
            &route(&run, refusing),
            &vision(),
            &run.store,
            "run-1",
            false
        )
        .await,
        0
    );
    assert_eq!(remote_ids(&req), [None]);
    assert!(!names_files(&req));
    assert!(!names_files(&request(vec![])));

    let without = Arc::new(Storage {
        no_files: true,
        ..Storage::default()
    });
    assert_eq!(
        attach(
            &mut req,
            &route(&run, without),
            &vision(),
            &run.store,
            "run-1",
            false
        )
        .await,
        0
    );

    // A part whose bytes are gone from the store stays as it was.
    let gone = Storage::default();
    let mut missing = request(vec![pdf]);
    std::fs::remove_dir_all(&run.run_dir).unwrap();
    assert_eq!(
        attach(
            &mut missing,
            &route(&run, Arc::new(gone)),
            &vision(),
            &run.store,
            "run-1",
            false
        )
        .await,
        0
    );
}

#[tokio::test]
async fn an_expiring_entry_is_uploaded_again_and_a_bad_ledger_starts_afresh() {
    let run = run();
    let pdf = stored(&run, "application/pdf", b"%PDF-1.7 c", None);
    let ledger = run.run_dir.join(LEDGER_FILE);
    let sha = match &pdf {
        ContentBlock::Mime { part, .. } => part.sha256.clone(),
        _ => unreachable!(),
    };
    std::fs::write(
        &ledger,
        serde_json::to_vec(&serde_json::json!({ "files": [
            { "provider": "anthropic", "sha256": sha, "file": { "id": "old", "expires_at": now_secs() + 5 } }
        ]}))
        .unwrap(),
    )
    .unwrap();
    let storage = Arc::new(Storage::default());
    let mut req = request(vec![pdf.clone()]);
    attach(
        &mut req,
        &route(&run, storage.clone()),
        &vision(),
        &run.store,
        "run-1",
        false,
    )
    .await;
    assert_eq!(remote_ids(&req), [Some("file-0".into())]);
    let text = std::fs::read_to_string(&ledger).unwrap();
    assert!(
        !text.contains("\"old\""),
        "the stale entry is replaced: {text}"
    );

    std::fs::write(&ledger, b"not json").unwrap();
    let mut again = request(vec![pdf]);
    attach(
        &mut again,
        &route(&run, storage),
        &vision(),
        &run.store,
        "run-1",
        false,
    )
    .await;
    assert_eq!(remote_ids(&again), [Some("file-1".into())]);
}

#[tokio::test]
async fn forgetting_a_run_leaves_what_it_cannot_delete_to_expire() {
    let run = run();
    std::fs::create_dir_all(&run.run_dir).unwrap();
    std::fs::write(
        run.run_dir.join(LEDGER_FILE),
        serde_json::to_vec(&serde_json::json!({ "files": [
            { "provider": "anthropic", "sha256": "a", "file": { "id": "f1" } },
            { "provider": "gone", "sha256": "b", "file": { "id": "f2" } }
        ]}))
        .unwrap(),
    )
    .unwrap();
    let refusing = Arc::new(Storage {
        refuse_deletes: true,
        ..Storage::default()
    });
    let mut registry = crate::ProviderRegistry::new();
    registry.register("anthropic".into(), refusing.clone());
    assert_eq!(forget_run(&run.run_dir, &registry).await, 0);
    assert_eq!(refusing.deletes.load(Ordering::SeqCst), 1);
    assert!(!run.run_dir.join(LEDGER_FILE).exists());
}

#[test]
fn a_file_name_keeps_a_readable_name_and_falls_back_to_the_hash() {
    assert_eq!(file_name(Some("a<b>.png"), "abc"), "a_b_.png");
    assert_eq!(
        file_name(Some("  "), "0123456789abcdef"),
        "part-0123456789ab"
    );
    assert_eq!(file_name(None, "short"), "part-short");
    assert_eq!(file_name(Some("x\ny"), "s"), "x_y");
}

#[tokio::test]
async fn the_trait_obligations_of_the_fake_hold() {
    let storage = Storage::default();
    assert!(storage.infer(&request(vec![])).await.is_err());
    assert_eq!(storage.count_tokens("", "").await, 1);
    assert_eq!(storage.max_context_tokens(""), 1_000);
    assert_eq!(storage.name(), "storage");
    let _ = storage.capabilities("");
}

#[tokio::test]
async fn a_finished_agent_deletes_its_uploads_in_the_background_and_a_paused_one_keeps_them() {
    use crate::blob_store::BlobStoreHandle;
    use crate::components::{AgentState, AgentStatus};

    let run = run();
    std::fs::create_dir_all(&run.run_dir).unwrap();
    let ledger = run.run_dir.join(LEDGER_FILE);
    let write_ledger = || {
        std::fs::write(
            &ledger,
            br#"{"files":[{"provider":"anthropic","sha256":"a","file":{"id":"f"}}]}"#,
        )
        .unwrap()
    };
    write_ledger();
    let storage = Arc::new(Storage::default());
    let mut registry = crate::ProviderRegistry::new();
    registry.register("anthropic".into(), storage.clone());

    let mut world = bevy_ecs::world::World::new();
    let state = AgentState {
        agent_id: "run-1".into(),
        current_visit: String::new(),
        current_stage: "s".into(),
        iteration: 0,
        status: AgentStatus::Paused,
        spawned_children_ids: Vec::new(),
        pending_wait: None,
        accepts_messages: false,
    };
    let entity = world.spawn(state).id();
    let bare = world.spawn(()).id();
    assert!(!forget_finished(&world, bare), "no agent state");
    assert!(
        !forget_finished(&world, entity),
        "a paused run keeps its files"
    );
    world.get_mut::<AgentState>(entity).unwrap().status = AgentStatus::Complete;
    assert!(!forget_finished(&world, entity), "no store or registry yet");
    world.insert_resource(crate::pipeline::Providers(registry.clone()));
    world.insert_resource(BlobStoreHandle(Arc::new(
        leviath_core::mime::MemoryBlobStore::new(),
    )));
    assert!(!forget_finished(&world, entity), "a store with no run dir");
    world.insert_resource(BlobStoreHandle(Arc::new(FsBlobStore::new(
        run.run_dir.parent().unwrap().to_path_buf(),
    ))));
    assert!(forget_finished(&world, entity));
    for _ in 0..200 {
        if storage.deletes.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(storage.deletes.load(Ordering::SeqCst), 1);
    assert!(!ledger.exists());
    assert!(
        !forget_in_background(run.run_dir.clone(), &registry),
        "nothing left"
    );
    assert!(take_ledger(&run.run_dir).is_empty());
}

#[test]
fn off_a_runtime_nothing_is_started() {
    let run = run();
    std::fs::create_dir_all(&run.run_dir).unwrap();
    std::fs::write(run.run_dir.join(LEDGER_FILE), b"{}").unwrap();
    assert!(!forget_in_background(
        run.run_dir.clone(),
        &crate::ProviderRegistry::new()
    ));
    let dir = run.run_dir.join(LEDGER_FILE);
    std::fs::remove_file(&dir).unwrap();
    std::fs::create_dir(&dir).unwrap();
    assert!(
        take_ledger(&run.run_dir).is_empty(),
        "a directory is no ledger"
    );
}

#[test]
fn a_route_needs_file_storage_the_switch_no_zero_retention_and_a_run_dir() {
    let run = run();
    let storage: Arc<dyn Provider> = Arc::new(Storage::default());
    let files = leviath_providers::files::provider_limits("anthropic");
    let allowed = leviath_providers::retention::RetentionSettings {
        file_uploads: true,
        ..Default::default()
    };
    let (route, why) = route_for(
        &storage,
        "anthropic",
        &files,
        &allowed,
        &run.store,
        "run-1",
        60,
    );
    let route = route.expect("a route");
    assert_eq!(route.ledger, run.run_dir.join(LEDGER_FILE));
    assert_eq!(route.ttl_secs, 60);
    assert_eq!(route.provider_name, "anthropic");
    assert_eq!(why, "");

    let zero = leviath_providers::retention::RetentionSettings {
        zero_requested: true,
        ..allowed.clone()
    };
    let (route, why) = route_for(
        &storage,
        "anthropic",
        &files,
        &zero,
        &run.store,
        "run-1",
        60,
    );
    assert!(route.is_none());
    assert!(why.contains("zero data retention"));

    let memory = leviath_core::mime::MemoryBlobStore::new();
    assert!(
        route_for(
            &storage,
            "anthropic",
            &files,
            &allowed,
            &memory,
            "run-1",
            60
        )
        .0
        .is_none()
    );

    let none = MediaLimits::NONE;
    assert_eq!(
        route_for(&storage, "x", &none, &zero, &run.store, "run-1", 60).1,
        ""
    );
}

#[tokio::test]
async fn a_ledger_that_cannot_be_written_leaves_the_upload_in_place() {
    let run = run();
    let pdf = stored(&run, "application/pdf", b"%PDF-1.7 w", None);
    let route = FileRoute {
        ledger: run.run_dir.join("no-such-dir").join(LEDGER_FILE),
        ..route(&run, Arc::new(Storage::default()))
    };
    let mut req = request(vec![pdf]);
    assert_eq!(
        attach(&mut req, &route, &vision(), &run.store, "run-1", false).await,
        1
    );
    assert_eq!(remote_ids(&req), [Some("file-0".into())]);
}
