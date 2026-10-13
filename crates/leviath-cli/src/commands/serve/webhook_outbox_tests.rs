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
/// with no outbox would. A delivery it settles is not sent again while it
/// runs, though nothing on disk says so: a restart may send it again.
#[test]
fn an_unwritable_outbox_still_delivers() {
    let dir = tempfile::tempdir().unwrap();
    let blocked = dir.path().join("webhooks");
    std::fs::write(&blocked, b"a file where the directory would go").unwrap();
    let outbox = Outbox::open(blocked.clone(), 7);
    assert_eq!(outbox.since, 7);
    assert!(outbox.claim("agent_completed:r1"));
    outbox.settle("agent_completed:r1");
    assert!(
        !outbox.claim("agent_completed:r1"),
        "settled in this process"
    );
    let restarted = Outbox::open(blocked, 9);
    assert!(
        restarted.claim("agent_completed:r1"),
        "nothing was recorded"
    );
}

/// A start that does not read as a second is written again from now, and
/// the next open keeps that. One that cannot be read at all (a directory
/// where the file goes) is left as it is, and the outbox counts from now.
#[test]
fn a_damaged_start_is_written_again_and_an_unreadable_one_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let webhooks = dir.path().join("webhooks");
    std::fs::create_dir_all(&webhooks).unwrap();
    std::fs::write(webhooks.join(SINCE), b"not a second").unwrap();
    assert_eq!(Outbox::open(webhooks.clone(), 40).since, 40);
    assert_eq!(Outbox::open(webhooks.clone(), 90).since, 40, "rewritten");

    let other = dir.path().join("other");
    std::fs::create_dir_all(other.join(SINCE)).unwrap();
    assert_eq!(Outbox::open(other.clone(), 60).since, 60);
    assert!(other.join(SINCE).is_dir(), "left as it was");

    // A start that does not read as a second, in an outbox whose `settled`
    // is a file: it cannot be written again, and the outbox counts from now.
    let stuck = dir.path().join("stuck");
    std::fs::create_dir_all(&stuck).unwrap();
    std::fs::write(stuck.join(SINCE), b"not a second").unwrap();
    std::fs::write(stuck.join(SETTLED), b"a file where the directory goes").unwrap();
    assert_eq!(Outbox::open(stuck.clone(), 70).since, 70);
    assert_eq!(std::fs::read(stuck.join(SINCE)).unwrap(), b"not a second");
}

/// A delivery given up unsent, because something it needs did not read just
/// now, can be claimed again by the next sweep.
#[test]
fn a_released_delivery_can_be_claimed_again() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = Outbox::open(dir.path().join("webhooks"), 0);
    assert!(outbox.claim("agent_completed:r1"));
    outbox.release("agent_completed:r1");
    assert!(outbox.claim("agent_completed:r1"));
}

/// One sweep runs at a time. One asked for while it runs is folded into a
/// single pass more, however many times it is asked for.
#[test]
fn sweeps_asked_for_while_one_runs_are_one_more_pass() {
    let dir = tempfile::tempdir().unwrap();
    let outbox = Outbox::open(dir.path().join("webhooks"), 0);
    assert!(outbox.begin_sweep());
    assert!(!outbox.begin_sweep());
    assert!(!outbox.begin_sweep());
    assert!(outbox.end_sweep(), "one more pass");
    assert!(!outbox.end_sweep(), "and then done");
    assert!(outbox.begin_sweep(), "a new sweep can start");
    assert!(!outbox.end_sweep());
}
