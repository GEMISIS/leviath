//! What a webhook outbox remembers across a restart, and what it owes.

use leviath_core::run_meta::{RunMeta, RunStatus};

use super::*;

fn delivery(run_id: &str) -> String {
    format!("agent_completed:{run_id}")
}

/// A run that finished at `updated_at`, with a callback when `callback`.
fn run(id: &str, status: RunStatus, updated_at: i64, callback: bool) -> RunMeta {
    let mut meta = RunMeta::new(
        id.into(),
        "a".into(),
        String::new(),
        String::new(),
        None,
        String::new(),
        1,
    );
    meta.status = status;
    meta.updated_at = updated_at;
    meta.callback_url = callback.then(|| "https://example.invalid/hook".to_string());
    meta
}

/// A new outbox counts from when it was made, and one opened again keeps
/// that second rather than the new one.
#[test]
fn an_outbox_remembers_when_it_began() {
    let dir = tempfile::tempdir().unwrap();
    let first = Outbox::open(dir.path().join("webhooks"), 100);
    assert_eq!(first.since, 100);
    let again = Outbox::open(dir.path().join("webhooks"), 500);
    assert_eq!(again.since, 100);
}

/// A finished run with a callback is owed until its delivery is settled; a
/// running one, one with no callback, and one that finished before the
/// outbox began are not.
#[test]
fn a_finished_run_is_owed_until_its_delivery_is_settled() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = Outbox::open(dir.path().join("webhooks"), 100);
    let runs = [
        run("done", RunStatus::Complete, 150, true),
        run("failed", RunStatus::Error, 150, true),
        run("going", RunStatus::Running, 150, true),
        run("silent", RunStatus::Complete, 150, false),
        run("old", RunStatus::Complete, 50, true),
    ];
    let owed = |o: &Outbox| -> Vec<String> {
        o.owed(&runs, delivery)
            .iter()
            .map(|m| m.run_id.clone())
            .collect()
    };
    assert_eq!(owed(&outbox), ["done", "failed"]);
    outbox.settle(&delivery("done"));
    assert_eq!(owed(&outbox), ["failed"]);
    // A restart reads the same settled set.
    let restarted = Outbox::open(dir.path().join("webhooks"), 900);
    assert_eq!(owed(&restarted), ["failed"]);
}

/// One delivery is sent once at a time: a second claim fails while the first
/// is sending, and a settled one is never claimed again.
#[test]
fn a_delivery_is_claimed_once() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = Outbox::open(dir.path().join("webhooks"), 0);
    assert!(outbox.claim("agent_completed:r1"));
    assert!(!outbox.claim("agent_completed:r1"), "already sending");
    outbox.settle("agent_completed:r1");
    assert!(!outbox.claim("agent_completed:r1"), "settled");
}

/// An outbox that cannot be written still hands out deliveries, as a server
/// with no outbox would; it only forgets them across a restart.
#[test]
fn an_unwritable_outbox_still_delivers() {
    let dir = tempfile::tempdir().unwrap();
    let blocked = dir.path().join("webhooks");
    std::fs::write(&blocked, b"a file where the directory would go").unwrap();
    let outbox = Outbox::open(blocked, 7);
    assert_eq!(outbox.since, 7);
    assert!(outbox.claim("agent_completed:r1"));
    outbox.settle("agent_completed:r1");
    assert!(outbox.claim("agent_completed:r1"), "nothing was recorded");
}
