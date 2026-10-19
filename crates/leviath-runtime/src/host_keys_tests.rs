//! Spawns under an idempotency key, through the host: what the host makes of
//! a repeated, a concurrent, a conflicting and a refused one. The key's own
//! bookkeeping is tested beside it in `host/claims_tests.rs`.

use super::*;

/// A request for the run `id` under the idempotency key `key`.
fn keyed(id: &str, key: &str) -> Box<crate::spec::request::SpawnRequest> {
    let mut request = named(id);
    request.delivery.idempotency_key = Some(crate::spec::names::IdempotencyKey::new(key).unwrap());
    request
}

/// What a spawn answers.
type Answer = Result<crate::spec::summary::Spawned, crate::spec::issues::SpawnIssues>;

/// Send `request` as a spawn without waiting for it, and the channel its
/// answer comes on.
fn send_spawn(
    host: &mut WorldHost,
    request: Box<crate::spec::request::SpawnRequest>,
) -> oneshot::Receiver<Answer> {
    let (reply, rx) = oneshot::channel();
    host.handle(ControlOp::Spawn { request, reply });
    rx
}

/// Send `request` as a spawn and wait for its answer.
async fn spawn_keyed(
    host: &mut WorldHost,
    request: Box<crate::spec::request::SpawnRequest>,
) -> Answer {
    ask(host, |reply| ControlOp::Spawn { request, reply }).await
}

/// The same request sent twice under one key starts one run: the second is
/// answered with the first one's id, marked as a repeat, and the starter
/// was asked once.
#[tokio::test]
async fn a_spawn_sent_again_under_its_key_starts_nothing_new() {
    let mut host = host_with(vec![]);
    let starter = TestStarter::new(Starts::Place);
    host.set_starter(starter.clone());
    let first = spawn_keyed(&mut host, keyed("r1", "pw:task-1"))
        .await
        .unwrap();
    assert!(!first.deduplicated);
    let again = spawn_keyed(&mut host, keyed("r1", "pw:task-1"))
        .await
        .unwrap();
    assert_eq!(again.run_id, first.run_id);
    assert!(again.deduplicated);
    assert_eq!(starter.callers.lock().unwrap().len(), 1);
    assert_eq!(host.by_run_id.len(), 1);
}

/// Two calls under one key at once: the second waits for the first's start
/// and both callers get its run.
#[tokio::test]
async fn two_spawns_at_once_under_one_key_start_one_run() {
    let mut host = host_with(vec![]);
    let starter = TestStarter::new(Starts::Place);
    host.set_starter(starter.clone());
    let one = send_spawn(&mut host, keyed("r1", "k"));
    let two = send_spawn(&mut host, keyed("r1", "k"));
    host.finish_starts().await;
    let (one, two) = (one.await.unwrap().unwrap(), two.await.unwrap().unwrap());
    assert_eq!((one.run_id.as_str(), two.run_id.as_str()), ("r1", "r1"));
    assert_eq!((one.deduplicated, two.deduplicated), (false, true));
    assert_eq!(starter.callers.lock().unwrap().len(), 1);
}

/// A different request under a key that is held is refused with a conflict
/// naming the run, and starts nothing.
#[tokio::test]
async fn a_different_request_under_a_held_key_is_refused() {
    let mut host = host_with(vec![]);
    let starter = TestStarter::new(Starts::Place);
    host.set_starter(starter.clone());
    spawn_keyed(&mut host, keyed("r1", "k")).await.unwrap();
    let refused = spawn_keyed(&mut host, keyed("r2", "k")).await.unwrap_err();
    let issue = &refused.0[0];
    assert_eq!(issue.code, crate::spec::issues::IssueCode::Conflict);
    assert_eq!(issue.path.to_string(), "delivery.idempotency_key");
    assert!(issue.message.contains("'r1'"), "{}", issue.message);
    assert_eq!(starter.callers.lock().unwrap().len(), 1);
}

/// A refused spawn does not claim its key: the same key starts a run once
/// the request can start. Nor does one that panics as it is placed.
#[tokio::test]
async fn a_refused_spawn_does_not_claim_its_key() {
    let mut host = host_with(vec![]);
    host.set_starter(TestStarter::new(Starts::Refuse));
    assert!(spawn_keyed(&mut host, keyed("r1", "k")).await.is_err());
    host.set_starter(TestStarter::new(Starts::Place));
    let started = spawn_keyed(&mut host, keyed("r1", "k")).await.unwrap();
    assert!(!started.deduplicated);

    host.set_starter(TestStarter::new(Starts::PanicOnPlacing));
    assert!(spawn_keyed(&mut host, keyed("r2", "other")).await.is_err());
    host.set_starter(TestStarter::new(Starts::Place));
    let placed = spawn_keyed(&mut host, keyed("r2", "other")).await.unwrap();
    assert!(!placed.deduplicated);
}

/// A key is held for as long as its run's record exists, and deleting the
/// run frees it. A key recalled at start answers for its run.
#[tokio::test]
async fn a_key_lasts_as_long_as_its_runs_record() {
    let dir = tempfile::tempdir().unwrap();
    let world = PipelineWorld::new(
        crate::providers::ProviderRegistry::new(),
        Arc::new(NoTools),
        InferencePoolConfig::new(),
        1,
        Some(dir.path().to_path_buf()),
        Handle::current(),
    );
    let mut host = WorldHost::new(world);
    let starter = TestStarter::new(Starts::Place);
    host.set_starter(starter.clone());
    let asked = keyed("r1", "k").idempotency().unwrap();
    host.recall_keys(vec![HeldKey {
        idempotency: asked,
        run_id: crate::spec::names::RunId::new("r-before").unwrap(),
        warnings: Default::default(),
    }]);
    let record = dir.path().join("r-before");
    std::fs::create_dir_all(&record).unwrap();
    std::fs::write(record.join(leviath_core::files::RUN_FILE), b"x").unwrap();
    let recalled = spawn_keyed(&mut host, keyed("r1", "k")).await.unwrap();
    assert_eq!(
        (recalled.run_id.as_str(), recalled.deduplicated),
        ("r-before", true)
    );
    assert!(starter.callers.lock().unwrap().is_empty());

    std::fs::remove_dir_all(&record).unwrap();
    let fresh = spawn_keyed(&mut host, keyed("r1", "k")).await.unwrap();
    assert_eq!((fresh.run_id.as_str(), fresh.deduplicated), ("r1", false));
}

/// A start that panics as it is placed marks its record refused, so a
/// restarted host does not hand its key back to a run that never started.
/// The host's housekeeping drops the keys of runs deleted since.
#[tokio::test]
async fn a_placement_that_panics_marks_its_record_and_deleted_runs_are_pruned() {
    let dir = tempfile::tempdir().unwrap();
    let world = PipelineWorld::new(
        crate::providers::ProviderRegistry::new(),
        Arc::new(NoTools),
        InferencePoolConfig::new(),
        1,
        Some(dir.path().to_path_buf()),
        Handle::current(),
    );
    let mut host = WorldHost::new(world);
    std::fs::create_dir_all(dir.path().join("r2")).unwrap();
    host.set_starter(TestStarter::new(Starts::PanicOnPlacing));
    assert!(spawn_keyed(&mut host, keyed("r2", "k")).await.is_err());
    assert!(
        dir.path()
            .join("r2")
            .join(crate::host::REFUSED_AT_START)
            .exists()
    );

    host.recall_keys(vec![HeldKey {
        idempotency: keyed("r3", "old").idempotency().unwrap(),
        run_id: crate::spec::names::RunId::new("r-deleted").unwrap(),
        warnings: Default::default(),
    }]);
    host.housekeep();
    assert_eq!(host.claims.len(), 0, "the deleted run's key is dropped");
}
