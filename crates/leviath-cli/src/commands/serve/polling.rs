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

        for meta in &runs {
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
                    accepts_messages: true, // default; stage-level control via agent state
                });

                // Token update
                let _ = state.event_tx.send(ServerEvent::Tokens {
                    agent_id: meta.agent_name.clone(),
                    run_id: meta.run_id.clone(),
                    prompt_tokens: meta.prompt_tokens,
                    completion_tokens: meta.completion_tokens,
                });

                // Detect completion
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

                    // Fire webhook callback if configured
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
        let already_fired = poll
            .callback_fired
            .get("run-1")
            .copied()
            .unwrap_or(false);
        assert!(!already_fired);
        poll.callback_fired.insert("run-1".to_string(), true);

        // Second check → should NOT fire again
        let already_fired = poll
            .callback_fired
            .get("run-1")
            .copied()
            .unwrap_or(false);
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
        let had_pending = poll
            .last_pending
            .get("run-1")
            .copied()
            .unwrap_or(false);
        assert!(has_pending && !had_pending, "should trigger on first pending");
        poll.last_pending.insert("run-1".to_string(), has_pending);

        // Second time → had_pending is now true, should NOT emit again
        let had_pending = poll
            .last_pending
            .get("run-1")
            .copied()
            .unwrap_or(false);
        assert!(
            !(has_pending && !had_pending),
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

    #[tokio::test]
    async fn polling_loop_runs_and_sends_events() {
        use std::sync::Arc;
        use tokio::sync::broadcast;

        use crate::config::Config;
        use crate::runstate::{create_run, RunMeta, RunStatus};

        let (tx, mut rx) = broadcast::channel::<ServerEvent>(64);
        let state = AppState {
            config: Arc::new(Config::default()),
            event_tx: tx.clone(),
        };

        // Create a run that the polling loop should pick up
        let run_id = format!(
            "test-poll-loop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test-agent".to_string(),
            "/path".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        meta.status = RunStatus::Running;
        create_run(&meta).unwrap();

        // Run the polling loop for a short time
        let poll_state = state.clone();
        let handle = tokio::spawn(async move {
            polling_loop(poll_state).await;
        });

        // Wait long enough for multiple poll iterations (200ms interval)
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        handle.abort();

        // Should have received at least one AgentStatus event for our run
        let mut got_status = false;
        while let Ok(ev) = rx.try_recv() {
            if let ServerEvent::AgentStatus { run_id: eid, .. } = &ev {
                if eid == &run_id {
                    got_status = true;
                }
            }
        }
        assert!(got_status, "polling loop should have emitted AgentStatus event");

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn polling_loop_emits_completion_event() {
        use std::sync::Arc;
        use tokio::sync::broadcast;

        use crate::config::Config;
        use crate::runstate::{create_run, write_meta, RunMeta, RunStatus};

        let (tx, mut rx) = broadcast::channel::<ServerEvent>(128);
        let state = AppState {
            config: Arc::new(Config::default()),
            event_tx: tx.clone(),
        };

        let run_id = format!(
            "test-poll-complete-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        // Start as Running, then transition to Complete
        let mut meta = RunMeta::new(
            run_id.clone(),
            "test-agent".to_string(),
            "/path".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        meta.status = RunStatus::Running;
        create_run(&meta).unwrap();

        let run_id2 = run_id.clone();
        let poll_state = state.clone();
        let handle = tokio::spawn(async move {
            polling_loop(poll_state).await;
        });

        // Let polling loop see the Running state first
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        // Now update to Complete
        let mut meta2 = meta.clone();
        meta2.status = RunStatus::Complete;
        meta2.touch();
        write_meta(&meta2).unwrap();

        // Wait for polling loop to detect the change
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        handle.abort();

        // Drain events to find AgentCompleted
        let mut got_completed = false;
        while let Ok(ev) = rx.try_recv() {
            if let ServerEvent::AgentCompleted { run_id: eid, .. } = &ev {
                if eid == &run_id2 {
                    got_completed = true;
                }
            }
        }
        assert!(
            got_completed,
            "polling loop should have emitted AgentCompleted event"
        );

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(&run_id2));
    }

    #[tokio::test]
    async fn polling_loop_emits_context_update() {
        use std::sync::Arc;
        use tokio::sync::broadcast;

        use crate::config::Config;
        use crate::runstate::{create_run, write_context_snapshot, ContextSnapshot, RunMeta};

        let (tx, mut rx) = broadcast::channel::<ServerEvent>(128);
        let state = AppState {
            config: Arc::new(Config::default()),
            event_tx: tx.clone(),
        };

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
            "test-agent".to_string(),
            "/path".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        create_run(&meta).unwrap();

        // Write a context snapshot
        let snap = ContextSnapshot {
            stage_name: "plan".to_string(),
            total_tokens: 7500,
            max_tokens: 200000,
            regions: vec![],
        };
        write_context_snapshot(&run_id, &snap).unwrap();

        let run_id2 = run_id.clone();
        let poll_state = state.clone();
        let handle = tokio::spawn(async move {
            polling_loop(poll_state).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        handle.abort();

        let mut got_context_update = false;
        while let Ok(ev) = rx.try_recv() {
            if let ServerEvent::ContextUpdate {
                run_id: eid,
                total_tokens,
                ..
            } = &ev
            {
                if eid == &run_id2 && *total_tokens == 7500 {
                    got_context_update = true;
                }
            }
        }
        assert!(
            got_context_update,
            "polling loop should have emitted ContextUpdate event"
        );

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(&run_id2));
    }

    #[tokio::test]
    async fn polling_loop_emits_interaction_needed() {
        use std::sync::Arc;
        use tokio::sync::broadcast;

        use crate::config::Config;
        use crate::interaction::{self, InteractionRequest};
        use crate::runstate::{create_run, RunMeta};

        let (tx, mut rx) = broadcast::channel::<ServerEvent>(128);
        let state = AppState {
            config: Arc::new(Config::default()),
            event_tx: tx.clone(),
        };

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
            "test-agent".to_string(),
            "/path".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        create_run(&meta).unwrap();

        // Write a pending interaction
        let req = InteractionRequest::free_text("req-poll-001", "What next?", "plan", true);
        interaction::write_request(&run_id, &req).unwrap();

        let run_id2 = run_id.clone();
        let poll_state = state.clone();
        let handle = tokio::spawn(async move {
            polling_loop(poll_state).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        handle.abort();

        let mut got_interaction = false;
        while let Ok(ev) = rx.try_recv() {
            if let ServerEvent::InteractionNeeded { run_id: eid, .. } = &ev {
                if eid == &run_id2 {
                    got_interaction = true;
                }
            }
        }
        assert!(
            got_interaction,
            "polling loop should have emitted InteractionNeeded event"
        );

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(&run_id2));
    }
}
