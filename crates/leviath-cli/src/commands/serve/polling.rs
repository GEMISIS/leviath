//! World-event consumer: subscribe to the daemon's pushed [`WorldEvent`] stream,
//! map each event to a [`ServerEvent`] for WebSocket subscribers, and fire a
//! completion webhook when a run finishes. The daemon pushes changes, so there
//! is no filesystem poll and no polling interval.

use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use leviath_runtime::host::WorldEvent;
use sha2::Sha256;

use super::types::*;
use crate::config::WebhookConfig;
use crate::runstate;

/// The reconnect backoff between subscribe passes in production.
pub(super) const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// Subscribe to the daemon's event stream and forward events forever,
/// reconnecting (after `backoff`) whenever the stream ends or the daemon is
/// briefly unreachable. Never returns; tests pass a zero backoff.
pub(super) async fn event_loop(state: AppState, backoff: Duration) {
    // Was a bare `Client::new()`, which has no timeouts at all: a webhook
    // endpoint that accepts a connection and never answers hung this delivery
    // forever. The shared factory supplies a connect+total timeout floor and
    // caps redirects.
    // `checked_client`, not `client`: the webhook URL comes from a request body,
    // and it was checked once at `POST /api/agents` and then never again. A
    // caller registered a public endpoint that answered `307 Location:
    // http://169.254.169.254/…`, and since 307 preserves the method *and* the
    // body, that was a repeatable POST primitive against the internal network -
    // re-followed on every retry.
    let client = leviath_core::checked_client(
        leviath_core::ClientTimeouts::default(),
        state.limits.allow_local_network,
    );
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
        fire_completion_webhook(client, &state.config.webhook, run_id, status);
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
/// `callback_url`. When the metadata also carries a `callback_secret`, the body
/// is signed with HMAC-SHA256 and the signature travels in `X-Leviath-Signature`.
fn fire_completion_webhook(
    client: &reqwest::Client,
    cfg: &WebhookConfig,
    run_id: &str,
    status: &str,
) {
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
    // Serialize once so the signature covers the exact bytes we send. `Value`'s
    // `Display` is infallible and byte-identical to `to_vec`.
    let body = payload.to_string().into_bytes();
    let signature = meta.callback_secret.as_deref().map(|s| sign(s, &body));
    tokio::spawn(fire_webhook(
        client.clone(),
        url,
        body,
        signature,
        cfg.clone(),
    ));
}

/// Compute the `X-Leviath-Signature` value: `sha256=<hex(HMAC-SHA256(secret, body))>`.
fn sign(secret: &str, body: &[u8]) -> String {
    type HmacSha256 = Hmac<Sha256>;
    // HMAC accepts a key of any length, so this construction never fails.
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// The backoff before retry `attempt` (1-based): `base * 2^(attempt-1)`, capped
/// at `max_delay_ms`. Mirrors the exponential shape used for provider rate limits.
fn backoff_delay(cfg: &WebhookConfig, attempt: u32) -> Duration {
    let factor = 2u64.saturating_pow(attempt.saturating_sub(1));
    let millis = cfg
        .base_delay_ms
        .saturating_mul(factor)
        .min(cfg.max_delay_ms);
    Duration::from_millis(millis)
}

/// Whether a delivery whose HTTP status was `status` is worth retrying.
/// Transient server-side conditions (5xx), rate limiting (429) and request
/// timeout (408) are retryable; any other non-2xx is a permanent rejection.
fn status_is_retryable(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
}

/// Deliver a webhook POST, retrying transient failures with exponential backoff.
/// Logs the final failure once the retries are exhausted (or a permanent
/// rejection is seen); never panics.
async fn fire_webhook(
    client: reqwest::Client,
    url: String,
    body: Vec<u8>,
    signature: Option<String>,
    cfg: WebhookConfig,
) {
    let max_attempts = cfg.max_retries.saturating_add(1);
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let mut req = client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .body(body.clone());
        if let Some(sig) = &signature {
            req = req.header("X-Leviath-Signature", sig);
        }
        // `outcome` describes this attempt: Ok(status) for a completed request
        // (2xx = done), Err(message) for a transport failure.
        let (retryable, outcome) = match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return; // delivered
                }
                (status_is_retryable(status), format!("HTTP {status}"))
            }
            Err(e) => (true, e.to_string()),
        };
        if !retryable || attempt >= max_attempts {
            let span = tracing::error_span!(
                "webhook_callback_failed",
                url = tracing::field::Empty,
                error = tracing::field::Empty,
                attempts = attempt,
            );
            let _enter = span.enter();
            span.record("url", tracing::field::display(&url));
            span.record("error", tracing::field::display(&outcome));
            tracing::error!("Webhook callback failed");
            return;
        }
        tokio::time::sleep(backoff_delay(&cfg, attempt)).await;
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
                limits: Default::default(),
            },
            rx,
        )
    }

    /// A webhook config with tiny delays so retry tests stay fast.
    fn fast_cfg() -> WebhookConfig {
        WebhookConfig {
            max_retries: 2,
            base_delay_ms: 1,
            max_delay_ms: 4,
            timeout_secs: 2,
        }
    }

    /// Spawn a fake webhook receiver that answers the i-th request with
    /// `statuses[i]` (each on its own connection, `Connection: close`), capturing
    /// every request's raw bytes. Serves exactly `statuses.len()` requests then
    /// stops - so a test asserts the attempt count by matching `statuses.len()`
    /// to the number of requests the receiver's join handle yields.
    async fn fake_receiver(statuses: Vec<u16>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut requests = Vec::new();
            for code in statuses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap();
                requests.push(String::from_utf8_lossy(&buf[..n]).to_string());
                let resp =
                    format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                sock.write_all(resp.as_bytes()).await.unwrap();
            }
            requests
        });
        (format!("http://{addr}/hook"), handle)
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
                let cfg = fast_cfg();
                // No meta on disk → no-op.
                fire_completion_webhook(&client, &cfg, "ghost", "complete");
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
                fire_completion_webhook(&client, &cfg, "no-cb", "complete");
            },
        )
        .await;
    }

    #[test]
    fn backoff_delay_grows_exponentially_then_caps() {
        let cfg = WebhookConfig {
            max_retries: 5,
            base_delay_ms: 100,
            max_delay_ms: 350,
            timeout_secs: 1,
        };
        assert_eq!(backoff_delay(&cfg, 1), Duration::from_millis(100));
        assert_eq!(backoff_delay(&cfg, 2), Duration::from_millis(200));
        // 400 would exceed the 350ms cap.
        assert_eq!(backoff_delay(&cfg, 3), Duration::from_millis(350));
        assert_eq!(backoff_delay(&cfg, 99), Duration::from_millis(350));
    }

    #[test]
    fn status_is_retryable_classifies_transient_vs_permanent() {
        use reqwest::StatusCode;
        assert!(status_is_retryable(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(status_is_retryable(StatusCode::BAD_GATEWAY));
        assert!(status_is_retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(status_is_retryable(StatusCode::REQUEST_TIMEOUT));
        assert!(!status_is_retryable(StatusCode::BAD_REQUEST));
        assert!(!status_is_retryable(StatusCode::NOT_FOUND));
        assert!(!status_is_retryable(StatusCode::OK));
    }

    #[test]
    fn sign_produces_stable_hmac_sha256() {
        // Known HMAC-SHA256 vector: key "key", message "hi".
        let sig = sign("key", b"hi");
        assert_eq!(
            sig,
            "sha256=1c9dc82e5f8e5ed5a0180aad33b8204dea12fde2fb62ffb5e963035bf324a7a4"
        );
        // Different secret or body ⇒ different signature.
        assert_ne!(sign("other", b"hi"), sig);
        assert_ne!(sign("key", b"bye"), sig);
    }

    #[tokio::test]
    async fn fire_webhook_succeeds_on_first_attempt() {
        let (url, server) = fake_receiver(vec![200]).await;
        fire_webhook(
            reqwest::Client::new(),
            url,
            b"{}".to_vec(),
            None,
            fast_cfg(),
        )
        .await;
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fire_webhook_retries_transient_then_succeeds() {
        // Two 503s then a 200 - delivery must make exactly three attempts.
        let (url, server) = fake_receiver(vec![503, 503, 200]).await;
        fire_webhook(
            reqwest::Client::new(),
            url,
            b"{}".to_vec(),
            None,
            fast_cfg(),
        )
        .await;
        assert_eq!(server.await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn fire_webhook_gives_up_after_exhausting_retries() {
        // Always 500; with max_retries = 2 that is exactly 3 attempts.
        let (url, server) = fake_receiver(vec![500, 500, 500]).await;
        fire_webhook(
            reqwest::Client::new(),
            url,
            b"{}".to_vec(),
            None,
            fast_cfg(),
        )
        .await;
        assert_eq!(server.await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn fire_webhook_does_not_retry_a_permanent_4xx() {
        // A 400 is a permanent rejection - exactly one attempt, no retries.
        let (url, server) = fake_receiver(vec![400]).await;
        fire_webhook(
            reqwest::Client::new(),
            url,
            b"{}".to_vec(),
            None,
            fast_cfg(),
        )
        .await;
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fire_webhook_sends_signature_header_when_present() {
        let body = b"{\"event\":\"agent_completed\"}".to_vec();
        let expected = sign("s3cret", &body);
        let (url, server) = fake_receiver(vec![200]).await;
        fire_webhook(
            reqwest::Client::new(),
            url,
            body,
            Some(expected.clone()),
            fast_cfg(),
        )
        .await;
        let requests = server.await.unwrap();
        let header = format!("x-leviath-signature: {expected}");
        assert!(requests[0].contains(&header));
    }

    #[tokio::test]
    async fn fire_webhook_omits_signature_header_when_absent() {
        let (url, server) = fake_receiver(vec![200]).await;
        fire_webhook(
            reqwest::Client::new(),
            url,
            b"{}".to_vec(),
            None,
            fast_cfg(),
        )
        .await;
        let requests = server.await.unwrap();
        assert!(!requests[0].to_lowercase().contains("x-leviath-signature"));
    }

    #[tokio::test]
    async fn fire_webhook_logs_transport_failure_without_panicking() {
        // An unroutable URL makes every attempt fail at the transport layer; it
        // must exhaust retries, log, and return without panicking.
        let client = reqwest::Client::new();
        fire_webhook(
            client,
            "http://127.0.0.1:1/never".to_string(),
            b"{}".to_vec(),
            None,
            fast_cfg(),
        )
        .await;
    }

    #[tokio::test]
    async fn fire_completion_webhook_signs_and_delivers() {
        crate::runstate::with_isolated_runs_dir_async(
            "fire_completion_webhook_signs",
            |_d| async move {
                let (url, server) = fake_receiver(vec![200]).await;
                let mut meta = RunMeta::new(
                    "signed".into(),
                    "coder".into(),
                    "/p".into(),
                    "t".into(),
                    None,
                    "/w".into(),
                    1,
                );
                meta.callback_url = Some(url);
                meta.callback_secret = Some("topsecret".into());
                create_run(&meta).unwrap();

                fire_completion_webhook(&reqwest::Client::new(), &fast_cfg(), "signed", "complete");
                let requests = server.await.unwrap();
                assert!(
                    requests[0]
                        .to_lowercase()
                        .contains("x-leviath-signature: sha256=")
                );
                assert!(requests[0].contains("agent_completed"));
            },
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
            let stream = listener
                .accept()
                .await
                .expect("accept succeeds")
                .expect("our own connection is admitted");
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
                let stream = listener
                    .accept()
                    .await
                    .expect("accept succeeds")
                    .expect("our own connection is admitted");
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
        // zero backoff keeps it prompt, but the `recv().await`s - not scheduler
        // timing - are what gate the assertions, so this is platform-stable.
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
