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
}
