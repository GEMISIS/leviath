use super::*;
use crate::commands::serve::testutil::fake_daemon;

/// Only a run the daemon lists as held has its questions read, off its file,
/// with the stage the daemon names and what to put back.
#[tokio::test]
async fn the_questions_of_held_runs_are_read_off_their_files() {
    crate::runstate::with_isolated_runs_dir_async("core-held-read", |_d| async {
        let held = seed_held("held-1", "held-1-ask-1");
        let mut live = seed_held("live-1", "live-1-ask-1");
        live.wait_reason = Some(WaitReason::UserPrompt);
        let mut idle = seed_held("idle-1", "idle-1-ask-1");
        idle.wait_reason = None;
        let reply = listing(vec![held, live, idle]);
        let (control, _socket, _srv) = fake_daemon(move |_| reply.clone());
        let found = held_questions(&control).await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].run_id, "held-1");
        assert_eq!(found[0].stage, "ask");
        assert_eq!(found[0].question.id, "held-1-ask-1");
        let why = found[0].refusal();
        assert!(
            why.starts_with("'held-1-ask-1' was asked by run 'held-1'"),
            "{why}"
        );
        assert!(why.contains("configure 'openai' again"), "{why}");
        assert!(why.ends_with("reopens under a new id"), "{why}");
    })
    .await;
}

/// A held run whose file will not read has no question to name, and a daemon
/// that does not list its runs holds none.
#[tokio::test]
async fn nothing_is_held_without_a_listing_or_a_file() {
    crate::runstate::with_isolated_runs_dir_async("core-held-none", |_d| async {
        let mut row = seed_held("held-1", "held-1-ask-1");
        row.run_id = "no-such-run".to_string();
        let reply = listing(vec![row]);
        let (control, _socket, _srv) = fake_daemon(move |_| reply.clone());
        assert!(held_questions(&control).await.is_empty());
        let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
        assert!(held_questions(&control).await.is_empty());
    })
    .await;
}

/// A miss on a question a held run asked is refused as held; any other miss
/// keeps the failure it came with.
#[tokio::test]
async fn a_miss_is_held_only_for_a_held_runs_question() {
    crate::runstate::with_isolated_runs_dir_async("core-held-or", |_d| async {
        let reply = listing(vec![seed_held("held-1", "held-1-ask-1")]);
        let for_hit = reply.clone();
        let (control, _socket, _srv) = fake_daemon(move |_| for_hit.clone());
        let hit = or_held(
            &control,
            |h| h.question.id == "held-1-ask-1",
            ServeError::NotFound("gone".into()),
        )
        .await;
        assert_eq!(hit.code(), "RUN_HELD");
        assert!(hit.to_string().contains("run 'held-1'"), "{hit}");
        let (control, _socket, _srv) = fake_daemon(move |_| reply.clone());
        let miss = or_held(
            &control,
            |h| h.question.id == "other",
            ServeError::NotFound("gone".into()),
        )
        .await;
        assert_eq!(miss.to_string(), "gone");
    })
    .await;
}
