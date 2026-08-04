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
    if let WorldEvent::Completed {
        run_id,
        status,
        final_output,
        ..
    } = &event
    {
        fire_completion_webhook(
            client,
            &state.config.webhook,
            run_id,
            status,
            final_output.as_ref(),
        );
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
            final_output,
        } => ServerEvent::AgentCompleted {
            agent_id,
            run_id: run_id.clone(),
            status,
            // Named `result` since before there was one; it carries the run's
            // *error*. The answer is `final_output`, beside it.
            result: runstate::read_meta(&run_id).ok().and_then(|m| m.error),
            // Taken from the event rather than re-read from disk: the event
            // fires the moment the run goes terminal, and the persist tick that
            // writes `meta.json` has not necessarily run yet.
            final_output: final_output.map(Into::into),
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
        // Everything else (stage transitions, tool call start/finish, and any
        // variant the runtime grows later - the enum is non-exhaustive) is
        // forwarded verbatim as its own serde-tagged JSON.
        other => ServerEvent::World {
            event: serde_json::to_value(&other).unwrap_or(serde_json::Value::Null),
        },
    }
}

/// The dedupe key for one delivery: `<event>:<run_id>`.
///
/// Deterministic on purpose - no randomness, no timestamp. A retried attempt, or
/// the same completion re-fired after a daemon restart, carries the identical id,
/// so a receiver deduping on it processes each completion once no matter how many
/// times it is delivered. It rides inside the signed body (so it is covered by
/// the HMAC) and in the `X-Leviath-Delivery` header for cheap access.
fn delivery_id(event: &str, run_id: &str) -> String {
    format!("{event}:{run_id}")
}

/// POST a completion webhook for `run_id` if its persisted metadata carries a
/// `callback_url`. When the metadata also carries a `callback_secret`, the body
/// is signed with HMAC-SHA256 and the signature travels in `X-Leviath-Signature`.
/// Every delivery carries a deterministic [`delivery_id`] for receiver-side
/// dedupe (in the signed body and the `X-Leviath-Delivery` header).
fn fire_completion_webhook(
    client: &reqwest::Client,
    cfg: &WebhookConfig,
    run_id: &str,
    status: &str,
    final_output: Option<&leviath_core::output::FinalOutput>,
) {
    let Ok(meta) = runstate::read_meta(run_id) else {
        return; // metadata not yet persisted
    };
    let Some(url) = meta.callback_url.clone() else {
        return; // no webhook configured
    };
    let delivery = delivery_id("agent_completed", &meta.run_id);
    let payload = completion_payload(&meta, status, final_output, &delivery);
    // Serialize once so the signature covers the exact bytes we send. `Value`'s
    // `Display` is infallible and byte-identical to `to_vec`.
    let body = payload.to_string().into_bytes();
    let signature = meta.callback_secret.as_deref().map(|s| sign(s, &body));
    tokio::spawn(fire_webhook(
        client.clone(),
        url,
        body,
        signature,
        delivery,
        cfg.clone(),
    ));
}

/// The completion webhook's body.
///
/// Pure over its inputs so the exact bytes a receiver gets are testable without
/// standing up an HTTP server - the signature covers these bytes, so getting
/// them wrong is not a cosmetic bug.
fn completion_payload(
    meta: &leviath_core::run_meta::RunMeta,
    status: &str,
    final_output: Option<&leviath_core::output::FinalOutput>,
    delivery: &str,
) -> serde_json::Value {
    serde_json::json!({
        "event": "agent_completed",
        "delivery_id": delivery,
        "run_id": meta.run_id,
        "agent_id": meta.agent_name,
        "status": status,
        // Named `result` since before a run could produce one; it carries the
        // run's *error*. The answer is `final_output`, below.
        "result": meta.error,
        // Taken from the event, not from `meta`: the webhook fires the moment
        // the run goes terminal, and the persist tick that writes `meta.json`
        // has not necessarily run yet - reading it here would race and deliver
        // a finished run with no answer.
        "final_output": final_output,
        "metadata": meta.metadata,
        "tokens": { "prompt": meta.prompt_tokens, "completion": meta.completion_tokens },
        // A `complete` status only says the pipeline ran to the end, not that
        // it achieved anything. A harness batching hundreds of runs has no
        // other way to tell the difference without re-reading the workspace.
        "empty_output": meta.flags.empty_output,
    })
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
    delivery: String,
    cfg: WebhookConfig,
) {
    let max_attempts = cfg.max_retries.saturating_add(1);
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let mut req = client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("X-Leviath-Delivery", &delivery)
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
                status: "complete".into(),
                final_output: None,
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
        // Source-emitted events (and any future variant) ride the generic
        // passthrough, keeping their own serde tags inside `event`.
        assert_eq!(
            mapped_tag(WorldEvent::StageTransition {
                run_id: "r".into(),
                agent_id: "a".into(),
                from: "plan".into(),
                to: "implement".into(),
                iteration: 1
            }),
            "world"
        );
        assert_eq!(
            mapped_tag(WorldEvent::ToolCallStarted {
                run_id: "r".into(),
                agent_id: "a".into(),
                call_id: "c1".into(),
                tool: "read_file".into()
            }),
            "world"
        );
        assert_eq!(
            mapped_tag(WorldEvent::ToolCallFinished {
                run_id: "r".into(),
                agent_id: "a".into(),
                call_id: "c1".into(),
                tool: "read_file".into(),
                ok: true,
                summary: "ok".into()
            }),
            "world"
        );
    }

    #[test]
    fn passthrough_keeps_the_inner_event_tag_and_run_id() {
        let ev = to_server_event(WorldEvent::StageTransition {
            run_id: "run-9".into(),
            agent_id: "a".into(),
            from: "plan".into(),
            to: "implement".into(),
            iteration: 2,
        });
        assert_eq!(ev.run_id(), "run-9");
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["event"]["event"], "stage_transition");
        assert_eq!(json["event"]["from"], "plan");
        assert_eq!(json["event"]["to"], "implement");
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
                        final_output: None,
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
                fire_completion_webhook(&client, &cfg, "ghost", "complete", None);
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
                fire_completion_webhook(&client, &cfg, "no-cb", "complete", None);
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
            "agent_completed:run-x".to_string(),
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
            "agent_completed:run-x".to_string(),
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
            "agent_completed:run-x".to_string(),
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
            "agent_completed:run-x".to_string(),
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
            "agent_completed:run-x".to_string(),
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
            "agent_completed:run-x".to_string(),
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
            "agent_completed:run-x".to_string(),
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
                meta.flags.empty_output = true;
                create_run(&meta).unwrap();

                fire_completion_webhook(
                    &reqwest::Client::new(),
                    &fast_cfg(),
                    "signed",
                    "complete",
                    None,
                );
                let requests = server.await.unwrap();
                assert!(
                    requests[0]
                        .to_lowercase()
                        .contains("x-leviath-signature: sha256=")
                );
                assert!(requests[0].contains("agent_completed"));
                // The delivery id travels in the signed body and the header.
                assert!(
                    requests[0].contains(r#""delivery_id":"agent_completed:signed""#),
                    "delivery id in the body"
                );
                assert!(
                    requests[0]
                        .to_lowercase()
                        .contains("x-leviath-delivery: agent_completed:signed"),
                    "delivery id in the header"
                );
                // A `complete` status alone would tell the receiver this run
                // succeeded, when it finished with nothing to show (#192).
                assert!(
                    requests[0].contains(r#""empty_output":true"#),
                    "the empty-run verdict travels with the completion"
                );
            },
        )
        .await;
    }

    #[test]
    fn delivery_id_is_deterministic_per_completion() {
        // No randomness, no timestamp: a retry and a post-restart re-fire carry
        // the same id, which is what makes receiver-side dedupe a key check.
        assert_eq!(
            delivery_id("agent_completed", "run-7"),
            "agent_completed:run-7"
        );
        assert_eq!(
            delivery_id("agent_completed", "run-7"),
            delivery_id("agent_completed", "run-7")
        );
    }

    #[tokio::test]
    async fn fire_webhook_repeats_the_same_delivery_id_across_retries() {
        let (url, server) = fake_receiver(vec![503, 200]).await;
        fire_webhook(
            reqwest::Client::new(),
            url,
            b"{}".to_vec(),
            None,
            "agent_completed:run-r".to_string(),
            fast_cfg(),
        )
        .await;
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        for req in &requests {
            assert!(
                req.to_lowercase()
                    .contains("x-leviath-delivery: agent_completed:run-r")
            );
        }
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

    fn payload_meta() -> leviath_core::run_meta::RunMeta {
        let mut meta = leviath_core::run_meta::RunMeta::new(
            "run-7".to_string(),
            "researcher".to_string(),
            "/agents/researcher".to_string(),
            "look it up".to_string(),
            None,
            "/work".to_string(),
            2,
        );
        meta.prompt_tokens = 10;
        meta.completion_tokens = 3;
        meta
    }

    /// The answer rides the payload, so a receiver learns what the run
    /// concluded without a second round trip to `/api/agents/{id}/result`.
    #[test]
    fn the_completion_payload_carries_the_agents_answer() {
        let answer = leviath_core::output::FinalOutput::new(
            r#"{"root":{"component":"Card"}}"#,
            Some("a2ui".to_string()),
            "summary".to_string(),
            88,
        );
        let payload = completion_payload(&payload_meta(), "complete", Some(&answer), "d-1");
        let output = &payload["final_output"];
        assert_eq!(
            output["content"].as_str().unwrap(),
            r#"{"root":{"component":"Card"}}"#
        );
        // An unrecognized label travels intact for the receiver to dispatch on.
        assert_eq!(output["format"].as_str().unwrap(), "a2ui");
        assert_eq!(output["stage"].as_str().unwrap(), "summary");
        assert_eq!(payload["event"].as_str().unwrap(), "agent_completed");
        assert_eq!(payload["status"].as_str().unwrap(), "complete");
        assert_eq!(payload["tokens"]["prompt"].as_u64().unwrap(), 10);
    }

    /// `result` has always carried the *error*, despite its name. A successful
    /// run leaves it null and puts the answer in `final_output`.
    #[test]
    fn result_stays_the_error_field_it_has_always_been() {
        let mut meta = payload_meta();
        meta.error = Some("provider refused".to_string());
        let payload = completion_payload(&meta, "error", None, "d-2");
        assert_eq!(payload["result"].as_str().unwrap(), "provider refused");
        assert!(payload["final_output"].is_null());
    }

    #[test]
    fn a_run_that_submitted_nothing_sends_a_null_answer() {
        let payload = completion_payload(&payload_meta(), "complete", None, "d-3");
        assert!(payload["final_output"].is_null());
    }
}
