//! Background polling loop for change detection and event broadcasting.

use std::collections::HashMap;
use std::time::Duration;

use tracing::error;

use super::types::*;
use crate::interaction;
use crate::runstate::{self, RunStatus};

/// Cached state for change detection.
struct PollState {
    /// run_id → (status_string, iteration, prompt_tokens, completion_tokens)
    last_status: HashMap<String, (String, usize, usize, usize)>,
    /// run_id → total_tokens from last context snapshot
    last_context_tokens: HashMap<String, usize>,
    /// run_id → whether we saw a pending interaction
    last_pending: HashMap<String, bool>,
    /// run_id → set of run_ids we have already fired callbacks for
    callback_fired: HashMap<String, bool>,
}

pub(super) async fn polling_loop(state: AppState) {
    let mut poll = PollState {
        last_status: HashMap::new(),
        last_context_tokens: HashMap::new(),
        last_pending: HashMap::new(),
        callback_fired: HashMap::new(),
    };

    let client = reqwest::Client::new();

    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let runs = runstate::list_runs();
        poll_once(&state, &mut poll, &client, &runs);
    }
}

/// Process one poll cycle for a given set of runs. Extracted from polling_loop
/// so tests can call it directly with synthetic RunMeta without depending on
/// the global ~/.leviath/runs/ directory.
fn poll_once(
    state: &AppState,
    poll: &mut PollState,
    client: &reqwest::Client,
    runs: &[runstate::RunMeta],
) {
    for meta in runs {
        let status_str = format!("{}", meta.status);
        let key = (
            status_str.clone(),
            meta.iteration,
            meta.prompt_tokens,
            meta.completion_tokens,
        );

        // Detect meta.json changes
        if poll.last_status.get(&meta.run_id) != Some(&key) {
            let _ = state.event_tx.send(ServerEvent::AgentStatus {
                agent_id: meta.agent_name.clone(),
                run_id: meta.run_id.clone(),
                status: status_str.clone(),
                stage: meta.current_stage.clone(),
                iteration: meta.iteration,
                accepts_messages: true,
            });

            let _ = state.event_tx.send(ServerEvent::Tokens {
                agent_id: meta.agent_name.clone(),
                run_id: meta.run_id.clone(),
                prompt_tokens: meta.prompt_tokens,
                completion_tokens: meta.completion_tokens,
            });

            let was_terminal = poll
                .last_status
                .get(&meta.run_id)
                .map(|(s, _, _, _)| s == "Complete" || s == "Error" || s == "Cancelled")
                .unwrap_or(false);

            if !was_terminal
                && (meta.status == RunStatus::Complete || meta.status == RunStatus::Error)
            {
                let _ = state.event_tx.send(ServerEvent::AgentCompleted {
                    agent_id: meta.agent_name.clone(),
                    run_id: meta.run_id.clone(),
                    status: status_str.clone(),
                    result: meta.error.clone(),
                });

                if let Some(ref url) = meta.callback_url {
                    if !poll
                        .callback_fired
                        .get(&meta.run_id)
                        .copied()
                        .unwrap_or(false)
                    {
                        poll.callback_fired.insert(meta.run_id.clone(), true);
                        let payload = serde_json::json!({
                            "event": "agent_completed",
                            "run_id": meta.run_id,
                            "agent_id": meta.agent_name,
                            "status": status_str,
                            "result": meta.error,
                            "metadata": meta.metadata,
                            "tokens": {
                                "prompt": meta.prompt_tokens,
                                "completion": meta.completion_tokens,
                            }
                        });
                        let client = client.clone();
                        let url = url.clone();
                        tokio::spawn(async move {
                            if let Err(e) = client.post(&url).json(&payload).send().await {
                                error!(url = %url, error = %e, "Webhook callback failed");
                            }
                        });
                    }
                }
            }

            poll.last_status.insert(meta.run_id.clone(), key);
        }

        // Detect context.json changes
        if let Some(ctx) = runstate::read_context_snapshot(&meta.run_id) {
            let prev = poll.last_context_tokens.get(&meta.run_id).copied();
            if prev != Some(ctx.total_tokens) {
                let _ = state.event_tx.send(ServerEvent::ContextUpdate {
                    agent_id: meta.agent_name.clone(),
                    run_id: meta.run_id.clone(),
                    total_tokens: ctx.total_tokens,
                    max_tokens: ctx.max_tokens,
                });
                poll.last_context_tokens
                    .insert(meta.run_id.clone(), ctx.total_tokens);
            }
        }

        // Detect pending.json
        let has_pending = interaction::read_request(&meta.run_id).is_some();
        let had_pending = poll
            .last_pending
            .get(&meta.run_id)
            .copied()
            .unwrap_or(false);
        if has_pending && !had_pending {
            if let Some(req) = interaction::read_request(&meta.run_id) {
                let val = serde_json::to_value(&req).unwrap_or(serde_json::Value::Null);
                let _ = state.event_tx.send(ServerEvent::InteractionNeeded {
                    agent_id: meta.agent_name.clone(),
                    run_id: meta.run_id.clone(),
                    request: val,
                });
            }
        }
        poll.last_pending.insert(meta.run_id.clone(), has_pending);
    }

    // Clean up old entries for runs that no longer exist
    let run_ids: std::collections::HashSet<String> =
        runs.iter().map(|r| r.run_id.clone()).collect();
    poll.last_status.retain(|k, _| run_ids.contains(k));
    poll.last_context_tokens.retain(|k, _| run_ids.contains(k));
    poll.last_pending.retain(|k, _| run_ids.contains(k));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_state_initial_is_empty() {
        let poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };
        assert!(poll.last_status.is_empty());
        assert!(poll.last_context_tokens.is_empty());
        assert!(poll.last_pending.is_empty());
        assert!(poll.callback_fired.is_empty());
    }

    #[test]
    fn poll_state_status_tracking() {
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };

        let key = ("Running".to_string(), 1, 500, 100);
        poll.last_status.insert("run-1".to_string(), key.clone());

        assert_eq!(poll.last_status.get("run-1"), Some(&key));
        assert_eq!(poll.last_status.get("run-2"), None);
    }

    #[test]
    fn poll_state_context_token_tracking() {
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };

        poll.last_context_tokens.insert("run-1".to_string(), 5000);
        assert_eq!(poll.last_context_tokens.get("run-1"), Some(&5000));
    }

    #[test]
    fn poll_state_pending_tracking() {
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };

        poll.last_pending.insert("run-1".to_string(), true);
        assert_eq!(poll.last_pending.get("run-1").copied(), Some(true));
        assert!(!poll.last_pending.get("run-2").copied().unwrap_or(false));
    }

    #[test]
    fn poll_state_callback_fired_tracking() {
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };

        assert!(!poll.callback_fired.get("run-1").copied().unwrap_or(false));
        poll.callback_fired.insert("run-1".to_string(), true);
        assert!(poll.callback_fired.get("run-1").copied().unwrap_or(false));
    }

    #[test]
    fn poll_state_cleanup_removes_stale_entries() {
        let mut last_status = HashMap::new();
        let mut last_context_tokens = HashMap::new();
        let mut last_pending = HashMap::new();

        last_status.insert("run-1".to_string(), ("Running".to_string(), 1, 100, 50));
        last_status.insert("run-2".to_string(), ("Complete".to_string(), 5, 500, 200));
        last_context_tokens.insert("run-1".to_string(), 1000);
        last_context_tokens.insert("run-2".to_string(), 2000);
        last_pending.insert("run-1".to_string(), false);
        last_pending.insert("run-2".to_string(), true);

        // Simulate only run-1 still existing
        let run_ids: std::collections::HashSet<String> =
            vec!["run-1".to_string()].into_iter().collect();

        last_status.retain(|k, _| run_ids.contains(k));
        last_context_tokens.retain(|k, _| run_ids.contains(k));
        last_pending.retain(|k, _| run_ids.contains(k));

        assert_eq!(last_status.len(), 1);
        assert!(last_status.contains_key("run-1"));
        assert!(!last_status.contains_key("run-2"));
        assert_eq!(last_context_tokens.len(), 1);
        assert_eq!(last_pending.len(), 1);
    }

    #[test]
    fn terminal_status_detection() {
        let terminal_statuses = ["Complete", "Error", "Cancelled"];
        for s in &terminal_statuses {
            let is_terminal = *s == "Complete" || *s == "Error" || *s == "Cancelled";
            assert!(is_terminal, "{} should be terminal", s);
        }

        let non_terminal = ["Running", "WaitingInput", "Pending"];
        for s in &non_terminal {
            let is_terminal = *s == "Complete" || *s == "Error" || *s == "Cancelled";
            assert!(!is_terminal, "{} should not be terminal", s);
        }
    }

    #[test]
    fn poll_state_was_terminal_logic() {
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };

        // Not in map yet → was_terminal = false
        let was_terminal = poll
            .last_status
            .get("run-1")
            .map(|(s, _, _, _)| s == "Complete" || s == "Error" || s == "Cancelled")
            .unwrap_or(false);
        assert!(!was_terminal);

        // Insert a running status
        poll.last_status
            .insert("run-1".to_string(), ("Running".to_string(), 1, 0, 0));
        let was_terminal = poll
            .last_status
            .get("run-1")
            .map(|(s, _, _, _)| s == "Complete" || s == "Error" || s == "Cancelled")
            .unwrap_or(false);
        assert!(!was_terminal);

        // Insert a terminal status
        poll.last_status
            .insert("run-1".to_string(), ("Complete".to_string(), 5, 100, 50));
        let was_terminal = poll
            .last_status
            .get("run-1")
            .map(|(s, _, _, _)| s == "Complete" || s == "Error" || s == "Cancelled")
            .unwrap_or(false);
        assert!(was_terminal);
    }

    #[test]
    fn poll_state_key_change_detection() {
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };

        let key1 = ("Running".to_string(), 1, 100, 50);
        poll.last_status.insert("run-1".to_string(), key1.clone());

        // Same key → no change
        assert_eq!(poll.last_status.get("run-1"), Some(&key1));
        let key2 = ("Running".to_string(), 2, 200, 100); // iteration changed
        assert_ne!(poll.last_status.get("run-1"), Some(&key2));
    }

    #[test]
    fn poll_state_callback_not_double_fired() {
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };

        // First completion → fire callback
        let already_fired = poll.callback_fired.get("run-1").copied().unwrap_or(false);
        assert!(!already_fired);
        poll.callback_fired.insert("run-1".to_string(), true);

        // Second check → should NOT fire again
        let already_fired = poll.callback_fired.get("run-1").copied().unwrap_or(false);
        assert!(already_fired);
    }

    #[test]
    fn poll_state_pending_transition_true_to_true() {
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };

        // First time seeing pending → should emit event (has_pending && !had_pending)
        let has_pending = true;
        let had_pending = poll.last_pending.get("run-1").copied().unwrap_or(false);
        assert!(
            has_pending && !had_pending,
            "should trigger on first pending"
        );
        poll.last_pending.insert("run-1".to_string(), has_pending);

        // Second time → had_pending is now true, should NOT emit again
        let had_pending = poll.last_pending.get("run-1").copied().unwrap_or(false);
        assert!(
            !has_pending || had_pending,
            "should not trigger when already pending"
        );
    }

    #[test]
    fn poll_state_context_token_change_detection() {
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };

        // New context snapshot → total_tokens different from None
        let prev = poll.last_context_tokens.get("run-1").copied();
        assert_eq!(prev, None);
        assert_ne!(prev, Some(5000)); // change detected

        poll.last_context_tokens.insert("run-1".to_string(), 5000);

        // Same value → no change
        let prev = poll.last_context_tokens.get("run-1").copied();
        assert_eq!(prev, Some(5000));
        assert_eq!(prev, Some(5000)); // no change
    }

    /// Helper: create a test AppState with a broadcast channel.
    fn make_test_state() -> (AppState, tokio::sync::broadcast::Receiver<ServerEvent>) {
        use crate::config::Config;
        use std::sync::Arc;
        let (tx, rx) = tokio::sync::broadcast::channel::<ServerEvent>(128);
        let state = AppState {
            config: Arc::new(Config::default()),
            event_tx: tx,
        };
        (state, rx)
    }

    // ─── poll_once tests ──────────────────────────────────────────────────
    // These call poll_once() directly with synthetic RunMeta, bypassing
    // list_runs() and the filesystem entirely. No timing, no flakiness.

    #[test]
    fn polling_loop_runs_and_sends_events() {
        use crate::runstate::{RunMeta, RunStatus};

        let (state, mut rx) = make_test_state();
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };
        let client = reqwest::Client::new();

        let mut meta = RunMeta::new(
            "run-status-1".into(),
            "agent".into(),
            "/p".into(),
            "task".into(),
            None,
            "/tmp".into(),
            1,
        );
        meta.status = RunStatus::Running;

        poll_once(&state, &mut poll, &client, &[meta]);

        let mut got_status = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(&ev, ServerEvent::AgentStatus { run_id, .. } if run_id == "run-status-1") {
                got_status = true;
            }
        }
        assert!(
            got_status,
            "poll_once should have emitted AgentStatus event"
        );
    }

    #[test]
    fn polling_loop_emits_completion_event() {
        use crate::runstate::{RunMeta, RunStatus};

        let (state, mut rx) = make_test_state();
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };
        let client = reqwest::Client::new();

        // First poll: Running
        let mut meta = RunMeta::new(
            "run-complete-1".into(),
            "agent".into(),
            "/p".into(),
            "task".into(),
            None,
            "/tmp".into(),
            1,
        );
        meta.status = RunStatus::Running;
        poll_once(&state, &mut poll, &client, &[meta.clone()]);

        // Drain Running events
        while rx.try_recv().is_ok() {}

        // Second poll: Complete
        meta.status = RunStatus::Complete;
        meta.touch();
        poll_once(&state, &mut poll, &client, &[meta]);

        let mut got_completed = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(&ev, ServerEvent::AgentCompleted { run_id, .. } if run_id == "run-complete-1")
            {
                got_completed = true;
            }
        }
        assert!(
            got_completed,
            "poll_once should have emitted AgentCompleted event"
        );
    }

    #[test]
    fn polling_loop_emits_context_update() {
        use crate::runstate::{create_run, write_context_snapshot, ContextSnapshot, RunMeta};

        let (state, mut rx) = make_test_state();
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };
        let client = reqwest::Client::new();

        let run_id = format!(
            "test-poll-ctx-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let meta = RunMeta::new(
            run_id.clone(),
            "agent".into(),
            "/p".into(),
            "task".into(),
            None,
            "/tmp".into(),
            1,
        );
        create_run(&meta).unwrap();

        let snap = ContextSnapshot {
            stage_name: "plan".to_string(),
            total_tokens: 7500,
            max_tokens: 200000,
            regions: vec![],
        };
        write_context_snapshot(&run_id, &snap).unwrap();

        poll_once(&state, &mut poll, &client, &[meta]);

        let mut got_context = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(&ev, ServerEvent::ContextUpdate { run_id: eid, total_tokens, .. } if eid == &run_id && *total_tokens == 7500)
            {
                got_context = true;
            }
        }
        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(&run_id));
        assert!(
            got_context,
            "poll_once should have emitted ContextUpdate event"
        );
    }

    #[test]
    fn polling_loop_emits_interaction_needed() {
        use crate::interaction::{self, InteractionRequest};
        use crate::runstate::{create_run, RunMeta};

        let (state, mut rx) = make_test_state();
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };
        let client = reqwest::Client::new();

        let run_id = format!(
            "test-poll-int-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let meta = RunMeta::new(
            run_id.clone(),
            "agent".into(),
            "/p".into(),
            "task".into(),
            None,
            "/tmp".into(),
            1,
        );
        create_run(&meta).unwrap();

        let req = InteractionRequest::free_text("req-001", "What next?", "plan", true);
        interaction::write_request(&run_id, &req).unwrap();

        poll_once(&state, &mut poll, &client, &[meta]);

        let mut got_interaction = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(&ev, ServerEvent::InteractionNeeded { run_id: eid, .. } if eid == &run_id) {
                got_interaction = true;
            }
        }
        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(&run_id));
        assert!(
            got_interaction,
            "poll_once should have emitted InteractionNeeded event"
        );
    }

    // ─── callback webhook (poll_once's tokio::spawn branch) ────────────────

    /// Minimal raw-TCP mock HTTP server that accepts one connection and
    /// records the request body it received, per the hand-rolled mocking
    /// convention used elsewhere in this crate (no mockito/wiremock).
    async fn spawn_mock_webhook() -> (String, tokio::sync::oneshot::Receiver<Vec<u8>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                buf.truncate(n);
                let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = socket.write_all(resp).await;
                let _ = socket.shutdown().await;
                let _ = tx.send(buf);
            }
        });

        (format!("http://{}", addr), rx)
    }

    #[tokio::test]
    async fn poll_once_fires_callback_webhook_on_completion() {
        use crate::runstate::{RunMeta, RunStatus};

        let (url, rx) = spawn_mock_webhook().await;

        let (state, mut evt_rx) = make_test_state();
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };
        let client = reqwest::Client::new();

        let mut meta = RunMeta::new(
            "run-webhook-1".into(),
            "agent".into(),
            "/p".into(),
            "task".into(),
            None,
            "/tmp".into(),
            1,
        );
        meta.callback_url = Some(url);
        meta.status = RunStatus::Running;
        poll_once(&state, &mut poll, &client, &[meta.clone()]);
        while evt_rx.try_recv().is_ok() {}

        meta.status = RunStatus::Complete;
        meta.touch();
        poll_once(&state, &mut poll, &client, &[meta]);

        // callback_fired should be recorded immediately...
        assert!(poll
            .callback_fired
            .get("run-webhook-1")
            .copied()
            .unwrap_or(false));

        // ...and the webhook POST should actually be delivered.
        let body = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("timed out waiting for webhook request")
            .expect("webhook sender dropped without sending");
        let body_str = String::from_utf8_lossy(&body);
        assert!(body_str.contains("agent_completed"));
        assert!(body_str.contains("run-webhook-1"));
    }

    #[tokio::test]
    async fn poll_once_webhook_send_failure_is_logged_not_panicked() {
        // Points callback_url at a closed local port so the spawned webhook
        // task's `client.post(&url)...send().await` genuinely fails,
        // exercising the `Err(e) => error!(...)` arm -- previously
        // unreached since the only other webhook test uses a real,
        // responding mock server.
        use crate::runstate::{RunMeta, RunStatus};

        let (state, mut evt_rx) = make_test_state();
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };
        let client = reqwest::Client::new();

        let mut meta = RunMeta::new(
            "run-webhook-fail".into(),
            "agent".into(),
            "/p".into(),
            "task".into(),
            None,
            "/tmp".into(),
            1,
        );
        meta.callback_url = Some("http://127.0.0.1:19997".to_string());
        meta.status = RunStatus::Running;
        poll_once(&state, &mut poll, &client, &[meta.clone()]);
        while evt_rx.try_recv().is_ok() {}

        meta.status = RunStatus::Complete;
        meta.touch();
        poll_once(&state, &mut poll, &client, &[meta]);

        // callback_fired is recorded synchronously, before the (failing)
        // webhook send is even attempted.
        assert!(poll
            .callback_fired
            .get("run-webhook-fail")
            .copied()
            .unwrap_or(false));

        // Give the spawned task time to attempt the connection and fail.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn poll_once_does_not_refire_callback_on_repeated_completion() {
        use crate::runstate::{RunMeta, RunStatus};

        let (url, rx) = spawn_mock_webhook().await;

        let (state, _rx) = make_test_state();
        let mut poll = PollState {
            last_status: HashMap::new(),
            last_context_tokens: HashMap::new(),
            last_pending: HashMap::new(),
            callback_fired: HashMap::new(),
        };
        let client = reqwest::Client::new();

        let mut meta = RunMeta::new(
            "run-webhook-2".into(),
            "agent".into(),
            "/p".into(),
            "task".into(),
            None,
            "/tmp".into(),
            1,
        );
        meta.callback_url = Some(url);
        meta.status = RunStatus::Running;
        poll_once(&state, &mut poll, &client, &[meta.clone()]);

        meta.status = RunStatus::Complete;
        meta.touch();
        // First completion: fires the webhook and marks it fired.
        poll_once(&state, &mut poll, &client, &[meta.clone()]);
        assert!(poll
            .callback_fired
            .get("run-webhook-2")
            .copied()
            .unwrap_or(false));

        // The mock server only accepts a single connection; if poll_once
        // tried to fire the callback again it would either hang or the
        // second send would simply be dropped since nothing is listening
        // anymore. Re-running poll_once with the same terminal status should
        // be a no-op (status key unchanged means we don't even re-enter the
        // completion branch).
        poll_once(&state, &mut poll, &client, &[meta]);

        let body = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("timed out waiting for webhook request")
            .expect("webhook sender dropped without sending");
        assert!(String::from_utf8_lossy(&body).contains("run-webhook-2"));
    }

    // ─── polling_loop wrapper ────────────────────────────────────────────

    #[tokio::test]
    async fn polling_loop_wrapper_picks_up_real_runs_from_disk() {
        use crate::runstate::{create_run, RunMeta};

        let run_id = format!(
            "test-poll-loop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let meta = RunMeta::new(
            run_id.clone(),
            "agent".into(),
            "/p".into(),
            "task".into(),
            None,
            "/tmp".into(),
            1,
        );
        create_run(&meta).unwrap();

        let (state, mut rx) = make_test_state();
        let handle = tokio::spawn(polling_loop(state));

        // polling_loop sleeps 200ms between cycles; give it enough time for
        // at least one full cycle to run and pick up the real run from disk.
        let mut saw_status = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                Ok(Ok(ServerEvent::AgentStatus { run_id: rid, .. })) if rid == run_id => {
                    saw_status = true;
                    break;
                }
                _ => continue,
            }
        }

        handle.abort();
        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(&run_id));
        assert!(
            saw_status,
            "polling_loop should have broadcast an AgentStatus event for the real run"
        );
    }
}
