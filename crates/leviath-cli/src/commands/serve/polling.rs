//! World-event consumer: subscribe to the daemon's pushed [`WorldEvent`] stream,
//! map each event to a [`ServerEvent`] for WebSocket subscribers, and fire a
//! completion webhook when a run finishes. Replaces the old filesystem poll — the
//! daemon now pushes changes, so there is no polling interval.

use std::time::Duration;

use leviath_runtime::host::WorldEvent;

use super::types::*;
use crate::runstate;

/// The reconnect backoff between subscribe passes in production.
pub(super) const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// Subscribe to the daemon's event stream and forward events forever,
/// reconnecting (after `backoff`) whenever the stream ends or the daemon is
/// briefly unreachable. Never returns; tests pass a zero backoff.
pub(super) async fn event_loop(state: AppState, backoff: Duration) {
    let client = reqwest::Client::new();
    loop {
        consume_once(&state, &client).await;
        // The stream ended (daemon closed / restarted) or was unreachable; back
        // off briefly, then re-subscribe.
        tokio::time::sleep(backoff).await;
    }
}

/// One subscribe-and-consume pass: forward events until the stream ends. Returns
/// immediately if the daemon can't be reached.
async fn consume_once(state: &AppState, client: &reqwest::Client) {
    let Ok(mut stream) = state.control.subscribe().await else {
        return;
    };
    while let Some(event) = stream.next().await {
        handle_event(state, client, event);
    }
}

/// Broadcast one world event to WebSocket subscribers, firing a completion
/// webhook when a run reaches a terminal status.
fn handle_event(state: &AppState, client: &reqwest::Client, event: WorldEvent) {
    if let WorldEvent::Completed { run_id, status, .. } = &event {
        fire_completion_webhook(client, run_id, status);
    }
    let _ = state.event_tx.send(to_server_event(event));
}

/// Map a [`WorldEvent`] to the [`ServerEvent`] WebSocket clients consume.
fn to_server_event(event: WorldEvent) -> ServerEvent {
    match event {
        WorldEvent::Spawned {
            run_id,
            agent_id,
            blueprint,
        } => ServerEvent::AgentSpawned {
            agent_id,
            run_id,
            parent_id: None,
            blueprint,
        },
        WorldEvent::Status {
            run_id,
            agent_id,
            status,
            stage,
            iteration,
            tool_calls,
            accepts_messages,
        } => ServerEvent::AgentStatus {
            agent_id,
            run_id,
            status,
            stage,
            iteration,
            tool_calls,
            accepts_messages,
        },
        WorldEvent::Tokens {
            run_id,
            agent_id,
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            cache_write_tokens,
        } => ServerEvent::Tokens {
            agent_id,
            run_id,
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            cache_write_tokens,
        },
        WorldEvent::Context {
            run_id,
            agent_id,
            total_tokens,
            max_tokens,
        } => ServerEvent::ContextUpdate {
            agent_id,
            run_id,
            total_tokens,
            max_tokens,
        },
        WorldEvent::Interaction {
            run_id,
            agent_id,
            request,
        } => ServerEvent::InteractionNeeded {
            agent_id,
            run_id,
            request: serde_json::to_value(&request).unwrap_or(serde_json::Value::Null),
        },
        WorldEvent::Completed {
            run_id,
            agent_id,
            status,
        } => ServerEvent::AgentCompleted {
            agent_id,
            run_id: run_id.clone(),
            status,
            // The result text is served by `/api/agents/{id}/result`; the run's
            // error (if any) rides the webhook payload below.
            result: runstate::read_meta(&run_id).ok().and_then(|m| m.error),
        },
        WorldEvent::Log {
            run_id,
            agent_id,
            line,
        } => ServerEvent::Log {
            agent_id,
            run_id,
            line,
        },
    }
}

/// POST a completion webhook for `run_id` if its persisted metadata carries a
/// `callback_url`.
fn fire_completion_webhook(client: &reqwest::Client, run_id: &str, status: &str) {
    let Ok(meta) = runstate::read_meta(run_id) else {
        return; // metadata not yet persisted
    };
    let Some(url) = meta.callback_url.clone() else {
        return; // no webhook configured
    };
    let payload = serde_json::json!({
        "event": "agent_completed",
        "run_id": meta.run_id,
        "agent_id": meta.agent_name,
        "status": status,
        "result": meta.error,
        "metadata": meta.metadata,
        "tokens": { "prompt": meta.prompt_tokens, "completion": meta.completion_tokens },
    });
    tokio::spawn(fire_webhook(client.clone(), url, payload));
}

/// Fire a webhook POST and log any delivery failure.
async fn fire_webhook(client: reqwest::Client, url: String, payload: serde_json::Value) {
    if let Err(e) = client.post(&url).json(&payload).send().await {
        let span = tracing::error_span!(
            "webhook_callback_failed",
            url = tracing::field::Empty,
            error = tracing::field::Empty
        );
        let _enter = span.enter();
        span.record("url", tracing::field::display(&url));
        span.record("error", tracing::field::display(&e));
        tracing::error!("Webhook callback failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::serve::testutil::no_daemon_client;
    use crate::config::Config;
    use crate::runstate::{RunMeta, create_run};
    use leviath_core::interaction::InteractionRequest;
    use leviath_runtime::control_socket::{ControlClient, bind_control_listener, control_id};
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::broadcast;

    fn state_with(control: ControlClient) -> (AppState, broadcast::Receiver<ServerEvent>) {
        let (tx, rx) = broadcast::channel(64);
        (
            AppState {
                config: Arc::new(Config::default()),
                event_tx: tx,
                control,
                mcp: crate::commands::serve::mcp::McpAdmin::default(),
            },
            rx,
        )
    }

    /// The `type` tag of a serialized [`ServerEvent`] (avoids `matches!` whose
    /// always-taken arm leaves the other arm uncovered).
    fn tag(event: &ServerEvent) -> String {
        serde_json::to_value(event).unwrap()["type"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn mapped_tag(event: WorldEvent) -> String {
        tag(&to_server_event(event))
    }

    #[test]
    fn to_server_event_maps_every_variant() {
        assert_eq!(
            mapped_tag(WorldEvent::Spawned {
                run_id: "r".into(),
                agent_id: "a".into(),
                blueprint: "coder".into()
            }),
            "agent_spawned"
        );
        assert_eq!(
            mapped_tag(WorldEvent::Status {
                run_id: "r".into(),
                agent_id: "a".into(),
                status: "active".into(),
                stage: "s".into(),
                iteration: 1,
                tool_calls: 0,
                accepts_messages: true
            }),
            "agent_status"
        );
        assert_eq!(
            mapped_tag(WorldEvent::Tokens {
                run_id: "r".into(),
                agent_id: "a".into(),
                prompt_tokens: 1,
                completion_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0
            }),
            "tokens"
        );
        assert_eq!(
            mapped_tag(WorldEvent::Context {
                run_id: "r".into(),
                agent_id: "a".into(),
                total_tokens: 10,
                max_tokens: 100
            }),
            "context_update"
        );
        assert_eq!(
            mapped_tag(WorldEvent::Interaction {
                run_id: "r".into(),
                agent_id: "a".into(),
                request: InteractionRequest::free_text("q", "p", "s", true)
            }),
            "interaction_needed"
        );
        assert_eq!(
            mapped_tag(WorldEvent::Completed {
                run_id: "r".into(),
                agent_id: "a".into(),
                status: "complete".into()
            }),
            "agent_completed"
        );
        assert_eq!(
            mapped_tag(WorldEvent::Log {
                run_id: "r".into(),
                agent_id: "a".into(),
                line: "hello".into()
            }),
            "log"
        );
    }

    #[tokio::test]
    async fn handle_event_broadcasts_status() {
        let (state, mut rx) = state_with(no_daemon_client());
        let client = reqwest::Client::new();
        handle_event(
            &state,
            &client,
            WorldEvent::Status {
                run_id: "r".into(),
                agent_id: "a".into(),
                status: "active".into(),
                stage: "s".into(),
                iteration: 0,
                tool_calls: 0,
                accepts_messages: true,
            },
        );
        assert_eq!(tag(&rx.try_recv().unwrap()), "agent_status");
    }

    #[tokio::test]
    async fn handle_event_completed_fires_webhook_and_broadcasts() {
        crate::runstate::with_isolated_runs_dir_async(
            "handle_event_completed_fires_webhook",
            |_d| async move {
                let mut meta = RunMeta::new(
                    "run-done".into(),
                    "coder".into(),
                    "/p".into(),
                    "t".into(),
                    None,
                    "/w".into(),
                    1,
                );
                meta.callback_url = Some("http://127.0.0.1:0/hook".into());
                meta.error = Some("boom".into());
                create_run(&meta).unwrap();

                let (state, mut rx) = state_with(no_daemon_client());
                let client = reqwest::Client::new();
                handle_event(
                    &state,
                    &client,
                    WorldEvent::Completed {
                        run_id: "run-done".into(),
                        agent_id: "a".into(),
                        status: "error".into(),
                    },
                );
                // The completion is broadcast with the run's error as the result.
                let ev = rx.try_recv().unwrap();
                assert_eq!(tag(&ev), "agent_completed");
                assert_eq!(
                    serde_json::to_value(&ev).unwrap()["result"].as_str(),
                    Some("boom")
                );
            },
        )
        .await;
    }

    #[tokio::test]
    async fn fire_completion_webhook_skips_missing_meta_and_missing_url() {
        crate::runstate::with_isolated_runs_dir_async(
            "fire_completion_webhook_skips",
            |_d| async move {
                let client = reqwest::Client::new();
                // No meta on disk → no-op.
                fire_completion_webhook(&client, "ghost", "complete");
                // Meta without a callback_url → no-op.
                let meta = RunMeta::new(
                    "no-cb".into(),
                    "coder".into(),
                    "/p".into(),
                    "t".into(),
                    None,
                    "/w".into(),
                    1,
                );
                create_run(&meta).unwrap();
                fire_completion_webhook(&client, "no-cb", "complete");
            },
        )
        .await;
    }

    #[tokio::test]
    async fn fire_webhook_succeeds_when_the_endpoint_accepts() {
        // A local listener that accepts the POST and replies 200 exercises the
        // success (non-error) path of the delivery.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        fire_webhook(
            reqwest::Client::new(),
            format!("http://{addr}/hook"),
            serde_json::json!({"x": 1}),
        )
        .await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn fire_webhook_logs_failure_without_panicking() {
        // An unroutable URL makes the POST fail; it must be logged, not panic.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        fire_webhook(
            client,
            "http://127.0.0.1:1/never".to_string(),
            serde_json::json!({"x": 1}),
        )
        .await;
    }

    /// A fake daemon that answers a `Subscribe` request by streaming `events`
    /// then closing.
    fn streaming_daemon(
        dir: &std::path::Path,
        events: Vec<WorldEvent>,
    ) -> (ControlClient, tokio::task::JoinHandle<()>) {
        let id = control_id(dir);
        let mut listener = bind_control_listener(&id).unwrap();
        let handle = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _subscribe = lines.next_line().await.unwrap();
            for event in events {
                let mut line = serde_json::to_string(&event).unwrap();
                line.push('\n');
                write_half.write_all(line.as_bytes()).await.unwrap();
            }
            // Drop write_half → the client's stream ends.
        });
        (ControlClient::new(id), handle)
    }

    #[tokio::test]
    async fn consume_once_forwards_streamed_events() {
        let dir = tempfile::tempdir().unwrap();
        let (control, server) = streaming_daemon(
            dir.path(),
            vec![WorldEvent::Status {
                run_id: "r".into(),
                agent_id: "a".into(),
                status: "active".into(),
                stage: "s".into(),
                iteration: 0,
                tool_calls: 0,
                accepts_messages: true,
            }],
        );
        let (state, mut rx) = state_with(control);
        let client = reqwest::Client::new();
        consume_once(&state, &client).await; // returns when the stream closes
        server.await.unwrap();
        assert_eq!(tag(&rx.try_recv().unwrap()), "agent_status");
    }

    #[tokio::test]
    async fn consume_once_returns_when_daemon_absent() {
        let (state, _rx) = state_with(no_daemon_client());
        let client = reqwest::Client::new();
        consume_once(&state, &client).await; // subscribe fails → returns immediately
    }

    /// A fake daemon that accepts `passes` subscribe connections, streaming one
    /// [`WorldEvent::Status`] on each before closing it.
    fn reconnecting_daemon(
        dir: &std::path::Path,
        passes: usize,
    ) -> (ControlClient, tokio::task::JoinHandle<()>) {
        let id = control_id(dir);
        let mut listener = bind_control_listener(&id).unwrap();
        let handle = tokio::spawn(async move {
            for _ in 0..passes {
                let stream = listener.accept().await.unwrap();
                let (read_half, mut write_half) = tokio::io::split(stream);
                let mut lines = BufReader::new(read_half).lines();
                let _subscribe = lines.next_line().await.unwrap();
                let mut line = serde_json::to_string(&WorldEvent::Status {
                    run_id: "r".into(),
                    agent_id: "a".into(),
                    status: "active".into(),
                    stage: "s".into(),
                    iteration: 0,
                    tool_calls: 0,
                    accepts_messages: true,
                })
                .unwrap();
                line.push('\n');
                write_half.write_all(line.as_bytes()).await.unwrap();
                // Drop write_half → the stream ends → event_loop reconnects.
            }
        });
        (ControlClient::new(id), handle)
    }

    #[tokio::test]
    async fn event_loop_reconnects_after_each_pass() {
        // Deterministically prove the loop reconnects: a daemon streams one event
        // per subscribe pass and closes; awaiting two events forces the loop
        // through consume_once → backoff → re-subscribe (its loop-back edge). A
        // zero backoff keeps it prompt, but the `recv().await`s — not scheduler
        // timing — are what gate the assertions, so this is platform-stable.
        let dir = tempfile::tempdir().unwrap();
        let (control, server) = reconnecting_daemon(dir.path(), 2);
        let (state, mut rx) = state_with(control);
        let handle = tokio::spawn(event_loop(state, Duration::ZERO));
        assert_eq!(tag(&rx.recv().await.unwrap()), "agent_status");
        assert_eq!(tag(&rx.recv().await.unwrap()), "agent_status");
        handle.abort();
        server.await.unwrap();
    }
}
