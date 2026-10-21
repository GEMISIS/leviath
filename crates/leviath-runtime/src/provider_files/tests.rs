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
    /// Every upload asked for, answered or not.
    attempts: AtomicUsize,
    uploads: AtomicUsize,
    deletes: AtomicUsize,
    refuse_uploads: bool,
    /// Uploads never answer, like a host that takes the connection and
    /// says nothing.
    silent_uploads: bool,
    refuse_deletes: bool,
    /// Deletes are answered 501, as Meta answers them.
    deletes_unsupported: bool,
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
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.silent_uploads {
            std::future::pending::<()>().await;
        }
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
        match (self.refuse_deletes, self.deletes_unsupported) {
            (true, _) => Err(ProviderError::ApiError("HTTP 403".into())),
            (_, true) => Err(ProviderError::ApiError(
                "HTTP 501 Not Implemented: {}".into(),
            )),
            _ => Ok(()),
        }
    }
}

struct Run {
    _dir: tempfile::TempDir,
    store: Arc<dyn BlobStore>,
    run_dir: PathBuf,
    registry: MimeRegistry,
}

fn run() -> Run {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn BlobStore> = Arc::new(FsBlobStore::new(dir.path().to_path_buf()));
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
        store: run.store.clone(),
        run_id: "run-1".into(),
        ledger: run.run_dir.join(LEDGER_FILE),
        ttl_secs: 3_600,
    }
}

/// A call deadline a test's uploads never come near.
fn far() -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_secs(60)
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
    let uploaded = attach(&mut first, &route, &vision(), &run.registry, false, far()).await;
    assert_eq!(uploaded, 1, "the same part twice is one upload");
    assert_eq!(
        remote_ids(&first),
        [None, Some("file-0".into()), Some("file-0".into()), None]
    );
    assert!(names_files(&first));
    assert_eq!(storage.names.lock().unwrap()[0], "re_port_.pdf");

    let mut second = request(vec![pdf.clone()]);
    assert_eq!(
        attach(&mut second, &route, &vision(), &run.registry, false, far()).await,
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
        attach(&mut refused, &route, &vision(), &run.registry, true, far()).await,
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
    assert!(
        !leviath_core::sync::lock(&LOCKS).contains_key(&route.ledger),
        "a run done with its ledger leaves no lock behind"
    );
}

/// Everyone using a ledger at once shares one lock, and the last to let go
/// takes it out of the table.
#[test]
fn a_ledger_lock_is_shared_while_held_and_dropped_after() {
    let path = PathBuf::from("/nowhere/run-locks/provider-files.json");
    let in_table = || leviath_core::sync::lock(&LOCKS).contains_key(&path);
    let first = LedgerLock::new(&path);
    let second = LedgerLock::new(&path);
    assert!(
        Arc::ptr_eq(&first.lock, &second.lock),
        "one lock per ledger"
    );
    drop(first);
    assert!(in_table(), "still held by the second");
    drop(second);
    assert!(!in_table(), "nobody holds it");
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
            &run.registry,
            false,
            far()
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
            &run.registry,
            false,
            far()
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
            &run.registry,
            false,
            far()
        )
        .await,
        0
    );
}

/// A part whose bytes are already named by a file id the vendor lost.
fn named_gone(block: &ContentBlock) -> ContentBlock {
    let mut block = block.clone();
    if let ContentBlock::Mime { remote, .. } = &mut block {
        *remote = Some(RemoteFile {
            id: "gone".into(),
            uri: None,
            expires_at: None,
        });
    }
    block
}

/// A provider whose upload failed is not asked again for a while: the rest
/// of the request and every later call of the run send their parts inline
/// without a round trip each. A renewal still asks, since only an upload
/// mends a file the vendor lost, and once the pause is up a success ends it.
#[tokio::test]
async fn a_failed_upload_pauses_the_providers_uploads_for_the_run() {
    let run = run();
    std::fs::create_dir_all(&run.run_dir).unwrap();
    let pdf = |text: &str| {
        stored(
            &run,
            "application/pdf",
            format!("%PDF-1.7 {text}").as_bytes(),
            None,
        )
    };
    let refusing = Arc::new(Storage {
        refuse_uploads: true,
        ..Storage::default()
    });
    let paused = route(&run, refusing.clone());
    let mut first = request(vec![pdf("a"), pdf("b")]);
    let uploaded = attach(&mut first, &paused, &vision(), &run.registry, false, far()).await;
    assert_eq!(uploaded, 0);
    assert_eq!(remote_ids(&first), [None, None]);
    assert_eq!(
        refusing.attempts.load(Ordering::SeqCst),
        1,
        "the part after a failure is not tried"
    );
    let ledger = run.run_dir.join(LEDGER_FILE);
    assert!(load(&ledger).refusing("anthropic", now_secs()));

    let mut later = request(vec![pdf("c")]);
    attach(&mut later, &paused, &vision(), &run.registry, false, far()).await;
    assert_eq!(
        refusing.attempts.load(Ordering::SeqCst),
        1,
        "a later call asks nobody"
    );
    assert_eq!(remote_ids(&later), [None]);

    let mut renewal = request(vec![named_gone(&pdf("d")), named_gone(&pdf("e"))]);
    attach(&mut renewal, &paused, &vision(), &run.registry, true, far()).await;
    assert_eq!(refusing.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        remote_ids(&renewal),
        [None, None],
        "both go inline, the second without asking"
    );

    // The pause is up: the provider is asked, and its answer ends the pause.
    let mut held = load(&ledger);
    held.refused[0].until = now_secs();
    save(&ledger, &held);
    let working = Arc::new(Storage::default());
    let mut next = request(vec![pdf("f")]);
    let uploaded = attach(
        &mut next,
        &route(&run, working.clone()),
        &vision(),
        &run.registry,
        false,
        far(),
    )
    .await;
    assert_eq!(uploaded, 1);
    assert_eq!(remote_ids(&next), [Some("file-0".into())]);
    assert!(load(&ledger).refused.is_empty());
    let written = std::fs::read_to_string(&ledger).unwrap();
    assert!(!written.contains("refused"), "{written}");
}

/// A host that takes an upload and never answers gets half the time the
/// call has left, not all of it: the call still goes out, its part inline,
/// and the provider is paused like any other failure.
#[tokio::test]
async fn an_upload_that_never_answers_gives_up_at_half_the_call_time() {
    let run = run();
    std::fs::create_dir_all(&run.run_dir).unwrap();
    let silent = Arc::new(Storage {
        silent_uploads: true,
        ..Storage::default()
    });
    let mut req = request(vec![stored(&run, "application/pdf", b"%PDF-1.7 s", None)]);
    let started = tokio::time::Instant::now();
    let deadline = started + std::time::Duration::from_millis(600);
    let uploaded = attach(
        &mut req,
        &route(&run, silent),
        &vision(),
        &run.registry,
        false,
        deadline,
    )
    .await;
    assert_eq!(uploaded, 0);
    assert!(
        tokio::time::Instant::now() < deadline,
        "the upload gave up before the call's deadline"
    );
    assert_eq!(remote_ids(&req), [None]);
    assert!(load(&run.run_dir.join(LEDGER_FILE)).refusing("anthropic", now_secs()));
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
        &run.registry,
        false,
        far(),
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
        &run.registry,
        false,
        far(),
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

/// A vendor that answers a delete with 501 deletes nothing: the file is not
/// counted as deleted, and it is asked once, not again.
#[tokio::test]
async fn a_vendor_that_deletes_no_file_is_asked_once_and_counts_none() {
    let unsupported = Arc::new(Storage {
        deletes_unsupported: true,
        ..Storage::default()
    });
    let mut registry = crate::ProviderRegistry::new();
    registry.register("meta".into(), unsupported.clone());
    let entries: Vec<Entry> = serde_json::from_value(serde_json::json!([
        { "provider": "meta", "sha256": "a", "file": { "id": "f1" } }
    ]))
    .unwrap();
    assert_eq!(delete_entries(&entries, &registry).await, 0);
    assert_eq!(unsupported.deletes.load(Ordering::SeqCst), 1);
}

#[test]
fn a_file_name_keeps_a_readable_name_and_falls_back_to_the_hash() {
    let png = ["png".to_string()];
    assert_eq!(file_name(Some("a<b>.png"), "abc", &png), "a_b_.png");
    assert_eq!(
        file_name(Some("  "), "0123456789abcdef", &png),
        "part-0123456789ab.png"
    );
    assert_eq!(file_name(None, "short", &[]), "part-short");
    assert_eq!(
        file_name(Some("x\ny"), "s", &[]),
        "x_y",
        "a type that claims no extension adds none"
    );
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

    let memory: Arc<dyn BlobStore> = Arc::new(leviath_core::mime::MemoryBlobStore::new());
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
        attach(&mut req, &route, &vision(), &run.registry, false, far()).await,
        1
    );
    assert_eq!(remote_ids(&req), [Some("file-0".into())]);
}

/// OpenAI reads an uploaded image's format from its file name, and refuses
/// one with no extension ("Expected image type ... but got none"). A part
/// named after an artifact (`image`), or not named at all, is uploaded under
/// a name that ends in its type's extension.
#[tokio::test]
async fn an_upload_is_named_with_its_types_extension() {
    let run = run();
    std::fs::create_dir_all(&run.run_dir).unwrap();
    let storage = Arc::new(Storage::default());
    let route = route(&run, storage.clone());
    let mut req = request(vec![
        stored(&run, "image/png", b"\x89PNG\r\n\x1a\n1", Some("image")),
        stored(&run, "image/png", b"\x89PNG\r\n\x1a\n2", None),
        stored(&run, "image/jpeg", b"\xff\xd8\xff3", Some("photo.JPEG")),
        stored(&run, "image/png", b"\x89PNG\r\n\x1a\n4", Some("shot.jpg")),
    ]);
    assert_eq!(
        attach(&mut req, &route, &vision(), &run.registry, false, far()).await,
        4
    );
    let names = storage.names.lock().unwrap().clone();
    assert_eq!(names[0], "image.png");
    assert!(names[1].starts_with("part-"));
    assert!(names[1].ends_with(".png"));
    assert_eq!(names[2], "photo.JPEG", "an extension the type claims stays");
    assert_eq!(
        names[3], "shot.jpg.png",
        "one the type does not claim gains its own"
    );
}
